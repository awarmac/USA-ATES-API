//! GDAL-backed DEM reader and GeoTIFF writer.

use std::path::{Path, PathBuf};

use ates_core::autoates::FillNodata;
use ates_core::{BBox, Crs, GeoTransform, Grid};
use gdal::raster::processing::dem::{self, AspectOptions, SlopeOptions};
use gdal::raster::{Buffer, RasterCreationOptions, reproject};
use gdal::spatial_ref::{AxisMappingStrategy, CoordTransform, SpatialRef};
use gdal::{Dataset, DriverManager, Metadata};
use ndarray::Array2;
use tracing::{debug, warn};

use crate::Provenance;
use crate::raster::{
    Band, BandData, GridSource, IoError, Projector, RasterSink, RasterSource, WindowRequest,
    check_aligned,
};

/// Nodata used for warped DEM windows.
pub const DEM_NODATA: f32 = -9999.0;

/// Upper bound on cells in one window, to fail fast instead of exhausting
/// memory (100 M cells is 400 MB of f32). Tiling will lift this later.
pub const MAX_WINDOW_CELLS: usize = 100_000_000;

/// A DEM in any GDAL-readable format, warped on demand to the analysis grid.
#[derive(Debug, Clone)]
pub struct GdalDem {
    path: PathBuf,
}

impl GdalDem {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

impl RasterSource for GdalDem {
    fn describe(&self) -> String {
        self.path.display().to_string()
    }

    fn read_window(&self, req: &WindowRequest) -> Result<Grid<f32>, IoError> {
        if !(req.pad_m.is_finite() && req.pad_m >= 0.0) {
            return Err(IoError::Invalid(format!(
                "pad_m must be >= 0, got {}",
                req.pad_m
            )));
        }
        let src = Dataset::open(&self.path)?;
        let src_srs = src.spatial_ref()?;
        let dst_srs = gis_order(SpatialRef::from_epsg(req.dst_epsg)?);

        let res = match req.target_res_m {
            Some(r) if r.is_finite() && r > 0.0 => r,
            Some(r) => {
                return Err(IoError::Invalid(format!(
                    "target_res_m must be > 0, got {r}"
                )));
            }
            None if src_srs.is_projected() => {
                let gt = src.geo_transform()?;
                gt[1].abs().min(gt[5].abs()) * src_srs.linear_units()
            }
            None => return Err(IoError::NeedsTargetRes(self.describe())),
        };

        let to_dst = CoordTransform::new(&gis_order(SpatialRef::from_epsg(4326)?), &dst_srs)?;
        let b = req.bbox_wgs84;
        let [x0, y0, x1, y1] =
            to_dst.transform_bounds(&[b.min_x, b.min_y, b.max_x, b.max_y], 21)?;
        let (gt, rows, cols) = snapped_window(BBox::new(x0, y0, x1, y1).buffered(req.pad_m), res)?;
        debug!(rows, cols, res, epsg = req.dst_epsg, "warping DEM window");

        let mut dst = DriverManager::get_driver_by_name("MEM")?
            .create_with_band_type::<f32, _>("", cols, rows, 1)?;
        dst.set_geo_transform(&gt.0)?;
        dst.set_spatial_ref(&dst_srs)?;
        {
            let mut band = dst.rasterband(1)?;
            band.set_no_data_value(Some(f64::from(DEM_NODATA)))?;
            band.fill(f64::from(DEM_NODATA), None)?;
        }
        reproject(&src, &dst)?;

        let grid = band_to_grid(&dst, 1, Crs::Epsg(req.dst_epsg))?;
        let missing = grid.data.iter().filter(|&&v| grid.is_nodata(v)).count();
        if missing == grid.data.len() {
            return Err(IoError::NoCoverage(self.describe()));
        }
        if missing > 0 {
            warn!(
                missing,
                total = grid.data.len(),
                "DEM window has nodata cells"
            );
        }
        Ok(grid)
    }
}

/// Writes bands to a DEFLATE-compressed GeoTIFF with provenance metadata.
/// All-Int16 band sets are written as Int16; otherwise Float32.
#[derive(Debug, Clone, Copy, Default)]
pub struct GeoTiffWriter;

impl RasterSink for GeoTiffWriter {
    fn write(&self, bands: &[Band<'_>], path: &Path, prov: &Provenance) -> Result<(), IoError> {
        let all_i16 = bands.iter().all(|b| matches!(b.data, BandData::I16(_)));
        let mut opts = RasterCreationOptions::new();
        let predictor = if all_i16 { "2" } else { "3" };
        for (k, v) in [
            ("COMPRESS", "DEFLATE"),
            ("PREDICTOR", predictor),
            ("TILED", "YES"),
        ] {
            opts.set_name_value(k, v)?;
        }
        create_filled("GTiff", path, bands, &opts, prov)?;
        Ok(())
    }
}

/// Writes bands to a Cloud-Optimized GeoTIFF (GDAL `COG` driver, DEFLATE,
/// nearest-neighbour overviews) with provenance metadata, so a server can
/// read small windows of a large region over HTTP.
#[derive(Debug, Clone, Copy, Default)]
pub struct CogWriter;

impl RasterSink for CogWriter {
    fn write(&self, bands: &[Band<'_>], path: &Path, prov: &Provenance) -> Result<(), IoError> {
        let mem = create_filled(
            "MEM",
            Path::new(""),
            bands,
            &RasterCreationOptions::new(),
            prov,
        )?;
        let mut opts = RasterCreationOptions::new();
        for (k, v) in [
            ("COMPRESS", "DEFLATE"),
            ("PREDICTOR", "YES"),
            ("RESAMPLING", "NEAREST"),
        ] {
            opts.set_name_value(k, v)?;
        }
        mem.create_copy(&DriverManager::get_driver_by_name("COG")?, path, &opts)?;
        Ok(())
    }
}

/// Create a dataset with `driver` and fill it with `bands`, georeferencing
/// and provenance.
fn create_filled(
    driver: &str,
    path: &Path,
    bands: &[Band<'_>],
    opts: &RasterCreationOptions,
    prov: &Provenance,
) -> Result<Dataset, IoError> {
    check_aligned(bands)?;
    let first = &bands[0];
    let (rows, cols) = first.dim();
    let all_i16 = bands.iter().all(|b| matches!(b.data, BandData::I16(_)));
    let driver = DriverManager::get_driver_by_name(driver)?;
    let mut ds = if all_i16 {
        driver.create_with_band_type_with_options::<i16, _>(path, cols, rows, bands.len(), opts)?
    } else {
        driver.create_with_band_type_with_options::<f32, _>(path, cols, rows, bands.len(), opts)?
    };
    {
        ds.set_geo_transform(&first.transform().0)?;
        ds.set_spatial_ref(&to_srs(first.crs())?)?;
        for (k, v) in prov.tags() {
            ds.set_metadata_item(k, &v, "")?;
        }
        for (i, b) in bands.iter().enumerate() {
            let mut band = ds.rasterband(i + 1)?;
            band.set_description(b.name)?;
            band.set_no_data_value(b.nodata())?;
            match (b.data, all_i16) {
                (BandData::I16(g), true) => {
                    let mut buf = Buffer::new((cols, rows), g.data.iter().copied().collect());
                    band.write((0, 0), (cols, rows), &mut buf)?;
                }
                (BandData::I16(g), false) => {
                    let mut buf =
                        Buffer::new((cols, rows), g.data.iter().map(|&v| f32::from(v)).collect());
                    band.write((0, 0), (cols, rows), &mut buf)?;
                }
                (BandData::F32(g), _) => {
                    let mut buf = Buffer::new((cols, rows), g.data.iter().copied().collect());
                    band.write((0, 0), (cols, rows), &mut buf)?;
                }
            }
        }
        ds.flush_cache()?;
    }
    Ok(ds)
}

/// An ArcGIS ImageServer layer of 8-bit values, fetched with `exportImage`
/// directly on the analysis grid (server-side nearest-neighbour), for
/// example USFS Tree Canopy Cover. Values in `invalid` become nodata.
#[derive(Debug, Clone)]
pub struct ImageServerSource {
    /// Service URL ending in `/ImageServer`.
    pub url: String,
    /// Optional `where` clause selecting rasters, e.g. `beginyear=2024`.
    pub where_clause: Option<String>,
    /// Pixel values that mean "no data" (e.g. 254 and 255 for TCC).
    pub invalid: Vec<u8>,
}

impl ImageServerSource {
    /// The `exportImage` request for `like`'s grid.
    pub fn export_url(&self, like: &Grid<f32>) -> Result<String, IoError> {
        let Crs::Epsg(epsg) = like.crs else {
            return Err(IoError::Invalid(
                "image service requests need an EPSG grid".into(),
            ));
        };
        let gt = &like.transform;
        let (x0, y1) = (gt.0[0], gt.0[3]);
        let x1 = x0 + like.cols() as f64 * gt.ew_res();
        let y0 = y1 - like.rows() as f64 * gt.ns_res();
        let mut url = format!(
            "{}/exportImage?bbox={x0},{y0},{x1},{y1}&bboxSR={epsg}&imageSR={epsg}\
             &size={},{}&format=tiff&pixelType=U8&interpolation=RSP_NearestNeighbor\
             &noData=255&f=image",
            self.url.trim_end_matches('/'),
            like.cols(),
            like.rows()
        );
        if let Some(w) = &self.where_clause {
            let rule = format!("{{\"where\":\"{}\"}}", w.replace('"', "\\\""));
            url.push_str("&mosaicRule=");
            url.push_str(&percent_encode(&rule));
        }
        Ok(url)
    }
}

impl GridSource for ImageServerSource {
    fn describe(&self) -> String {
        match &self.where_clause {
            Some(w) => format!("{} [{w}]", self.url),
            None => self.url.clone(),
        }
    }

    fn read_on(&self, like: &Grid<f32>) -> Result<Grid<f32>, IoError> {
        let url = self.export_url(like)?;
        gdal::config::set_config_option(
            "GDAL_HTTP_USERAGENT",
            concat!("USA-ATES-API/", env!("CARGO_PKG_VERSION")),
        )?;
        // The service does not support HTTP range requests, so stream it.
        let ds = Dataset::open(format!("/vsicurl_streaming/{url}"))?;
        let band = ds.rasterband(1)?;
        let ((cols, rows), data) = band.read_band_as::<u8>()?.into_shape_and_vec();
        if (rows, cols) != like.data.dim() {
            return Err(IoError::Invalid(format!(
                "{} returned {rows}x{cols} cells, asked for {:?}",
                self.describe(),
                like.data.dim()
            )));
        }
        let got = GeoTransform(ds.geo_transform()?);
        let close = |a: f64, b: f64| (a - b).abs() <= 1e-6 * like.transform.ew_res();
        if !got
            .0
            .iter()
            .zip(like.transform.0.iter())
            .all(|(&a, &b)| close(a, b))
        {
            return Err(IoError::Invalid(format!(
                "{} returned transform {:?}, asked for {:?}",
                self.describe(),
                got.0,
                like.transform.0
            )));
        }
        let values = data
            .into_iter()
            .map(|v| {
                if self.invalid.contains(&v) {
                    DEM_NODATA
                } else {
                    f32::from(v)
                }
            })
            .collect();
        let data = Array2::from_shape_vec((rows, cols), values)
            .map_err(|e| IoError::Invalid(e.to_string()))?;
        Ok(Grid::new(
            data,
            like.transform,
            like.crs.clone(),
            Some(f64::from(DEM_NODATA)),
        )?)
    }
}

/// Percent-encode everything but RFC 3986 unreserved characters.
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Lon/lat to projected coordinates with GDAL/PROJ.
#[derive(Debug, Clone, Copy, Default)]
pub struct GdalProjector;

impl Projector for GdalProjector {
    fn lonlat_to(&self, lon: f64, lat: f64, epsg: u32) -> Result<(f64, f64), IoError> {
        let ct = CoordTransform::new(
            &gis_order(SpatialRef::from_epsg(4326)?),
            &gis_order(SpatialRef::from_epsg(epsg)?),
        )?;
        let (mut x, mut y, mut z) = ([lon], [lat], [0.0]);
        ct.transform_coords(&mut x, &mut y, &mut z)?;
        Ok((x[0], y[0]))
    }
}

/// [`FillNodata`] backed by `GDALFillNodata`, with the same call AutoATES
/// makes through `rasterio.fill.fillnodata`: Int16 in-memory band, byte
/// mask (0 = fill), inverse-distance interpolation, no smoothing.
#[derive(Debug, Clone, Copy, Default)]
pub struct GdalFill;

impl FillNodata for GdalFill {
    fn fill(
        &self,
        values: &Grid<i16>,
        mask: &Array2<u8>,
        max_search_px: f64,
    ) -> Result<Array2<i16>, String> {
        gdal_fill_nodata(values, mask, max_search_px).map_err(|e| e.to_string())
    }
}

fn gdal_fill_nodata(
    values: &Grid<i16>,
    mask: &Array2<u8>,
    max_search_px: f64,
) -> Result<Array2<i16>, IoError> {
    let (rows, cols) = values.data.dim();
    if mask.dim() != (rows, cols) {
        return Err(IoError::Invalid(
            "fill mask shape differs from values".into(),
        ));
    }
    let mem = DriverManager::get_driver_by_name("MEM")?;
    let ds = mem.create_with_band_type::<i16, _>("", cols, rows, 1)?;
    let mask_ds = mem.create_with_band_type::<u8, _>("", cols, rows, 1)?;
    let mut band = ds.rasterband(1)?;
    let mut buf = Buffer::new((cols, rows), values.data.iter().copied().collect());
    band.write((0, 0), (cols, rows), &mut buf)?;
    let mut mask_band = mask_ds.rasterband(1)?;
    let mut mbuf = Buffer::new((cols, rows), mask.iter().copied().collect());
    mask_band.write((0, 0), (cols, rows), &mut mbuf)?;

    // SAFETY: both band handles come from live datasets (`ds`, `mask_ds`)
    // that outlive this call; null options, progress callback and progress
    // argument are documented as accepted by GDALFillNodata. The function
    // only reads/writes through the given band handles.
    #[allow(unsafe_code)]
    let err = unsafe {
        gdal_sys::GDALFillNodata(
            band.c_rasterband(),
            mask_band.c_rasterband(),
            max_search_px,
            0,
            0,
            std::ptr::null_mut(),
            None,
            std::ptr::null_mut(),
        )
    };
    if err != gdal_sys::CPLErr::CE_None {
        return Err(IoError::Invalid(format!(
            "GDALFillNodata failed with CPLErr {err}"
        )));
    }
    let ((c, r), data) = band.read_band_as::<i16>()?.into_shape_and_vec();
    Array2::from_shape_vec((r, c), data).map_err(|e| IoError::Invalid(e.to_string()))
}

/// Run GDAL's own `gdaldem slope` and `gdaldem aspect` (Horn, degrees,
/// azimuth) on `grid` in memory. Used as the reference implementation when
/// validating [`ates_core::terrain`].
pub fn gdaldem_slope_aspect(
    grid: &Grid<f32>,
    compute_edges: bool,
) -> Result<(Grid<f32>, Grid<f32>), IoError> {
    let src = grid_to_mem(grid)?;
    let mut slope_opts = SlopeOptions::new();
    slope_opts
        .with_compute_edges(compute_edges)
        .with_output_format("MEM");
    let mut aspect_opts = AspectOptions::new();
    aspect_opts
        .with_compute_edges(compute_edges)
        .with_output_format("MEM");
    let slope = dem::slope(&src, "", &slope_opts)?;
    let aspect = dem::aspect(&src, "", &aspect_opts)?;
    Ok((
        band_to_grid(&slope, 1, grid.crs.clone())?,
        band_to_grid(&aspect, 1, grid.crs.clone())?,
    ))
}

/// Read a raster file's first band as-is, without warping.
pub fn read_grid(path: &Path) -> Result<Grid<f32>, IoError> {
    let ds = Dataset::open(path)?;
    let srs = ds.spatial_ref()?;
    let crs = match srs.auth_name().as_deref().zip(srs.auth_code().ok()) {
        Some(("EPSG", code)) if code > 0 => Crs::Epsg(code as u32),
        _ => Crs::Wkt(srs.to_wkt()?),
    };
    band_to_grid(&ds, 1, crs)
}

/// Every band's description and value at (`row`, `col`) of a raster file.
pub fn read_bands_at(path: &Path, row: usize, col: usize) -> Result<Vec<(String, f64)>, IoError> {
    let ds = Dataset::open(path)?;
    let mut out = Vec::new();
    for i in 1..=ds.raster_count() {
        let band = ds.rasterband(i)?;
        let buf = band.read_as::<f64>((col as isize, row as isize), (1, 1), (1, 1), None)?;
        out.push((band.description()?, buf.data()[0]));
    }
    Ok(out)
}

/// Expand `b` outward to whole multiples of `res`, returning a north-up
/// transform and the grid size.
fn snapped_window(b: BBox, res: f64) -> Result<(GeoTransform, usize, usize), IoError> {
    let x0 = (b.min_x / res).floor() * res;
    let y0 = (b.min_y / res).floor() * res;
    let x1 = ((b.max_x / res).ceil() * res).max(x0 + res);
    let y1 = ((b.max_y / res).ceil() * res).max(y0 + res);
    let cols = ((x1 - x0) / res).round() as usize;
    let rows = ((y1 - y0) / res).round() as usize;
    if rows.saturating_mul(cols) > MAX_WINDOW_CELLS {
        return Err(IoError::WindowTooLarge {
            rows,
            cols,
            max: MAX_WINDOW_CELLS,
        });
    }
    Ok((GeoTransform::north_up(x0, y1, res, res), rows, cols))
}

fn gis_order(mut srs: SpatialRef) -> SpatialRef {
    srs.set_axis_mapping_strategy(AxisMappingStrategy::TraditionalGisOrder);
    srs
}

fn to_srs(crs: &Crs) -> Result<SpatialRef, IoError> {
    Ok(gis_order(match crs {
        Crs::Epsg(code) => SpatialRef::from_epsg(*code)?,
        Crs::Wkt(wkt) => SpatialRef::from_wkt(wkt)?,
    }))
}

fn grid_to_mem(grid: &Grid<f32>) -> Result<Dataset, IoError> {
    let mut ds = DriverManager::get_driver_by_name("MEM")?.create_with_band_type::<f32, _>(
        "",
        grid.cols(),
        grid.rows(),
        1,
    )?;
    ds.set_geo_transform(&grid.transform.0)?;
    ds.set_spatial_ref(&to_srs(&grid.crs)?)?;
    let mut band = ds.rasterband(1)?;
    band.set_no_data_value(grid.nodata)?;
    let mut buf = Buffer::new(
        (grid.cols(), grid.rows()),
        grid.data.iter().copied().collect(),
    );
    band.write((0, 0), (grid.cols(), grid.rows()), &mut buf)?;
    Ok(ds)
}

fn band_to_grid(ds: &Dataset, index: usize, crs: Crs) -> Result<Grid<f32>, IoError> {
    let band = ds.rasterband(index)?;
    let ((cols, rows), data) = band.read_band_as::<f32>()?.into_shape_and_vec();
    let data = Array2::from_shape_vec((rows, cols), data)
        .map_err(|e| IoError::Invalid(format!("band {index}: {e}")))?;
    Ok(Grid::new(
        data,
        GeoTransform(ds.geo_transform()?),
        crs,
        band.no_data_value(),
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_server_url_targets_the_grid() {
        let like = Grid::new(
            Array2::<f32>::zeros((2, 3)),
            GeoTransform::north_up(420_000.0, 4_482_000.0, 10.0, 10.0),
            Crs::Epsg(32613),
            None,
        )
        .unwrap();
        let src = ImageServerSource {
            url: "https://example.org/ImageServer/".into(),
            where_clause: Some("beginyear=2024".into()),
            invalid: vec![254, 255],
        };
        let url = src.export_url(&like).unwrap();
        assert!(url.starts_with(
            "https://example.org/ImageServer/exportImage?bbox=420000,4481980,420030,4482000\
             &bboxSR=32613&imageSR=32613&size=3,2&"
        ));
        assert!(url.ends_with("&mosaicRule=%7B%22where%22%3A%22beginyear%3D2024%22%7D"));
    }

    #[test]
    fn snapping_expands_outward() {
        let (gt, rows, cols) = snapped_window(BBox::new(105.0, 203.0, 141.0, 219.0), 10.0).unwrap();
        assert_eq!(gt, GeoTransform::north_up(100.0, 220.0, 10.0, 10.0));
        assert_eq!((rows, cols), (2, 5));
    }

    #[test]
    fn snapping_point_gives_one_cell() {
        let (_, rows, cols) = snapped_window(BBox::point(100.0, 100.0), 10.0).unwrap();
        assert_eq!((rows, cols), (1, 1));
    }

    #[test]
    fn snapping_rejects_huge_windows() {
        let r = snapped_window(BBox::new(0.0, 0.0, 1e6, 1e6), 1.0);
        assert!(matches!(r, Err(IoError::WindowTooLarge { .. })));
    }
}
