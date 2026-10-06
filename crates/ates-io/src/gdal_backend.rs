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
    Band, BandData, GridSource, IoError, Projector, RasterSink, RasterSource, TILE_PLACEHOLDER,
    WindowRequest, check_aligned, degree_tiles,
};

/// Nodata used for warped DEM windows.
pub const DEM_NODATA: f32 = -9999.0;

/// Upper bound on cells in one window, to fail fast instead of exhausting
/// memory (100 M cells is 400 MB of f32, about 10 000 km² at 10 m).
/// Tiled compute will lift this when regions grow beyond it.
pub const MAX_WINDOW_CELLS: usize = 100_000_000;

/// A DEM in any GDAL-readable format, warped on demand to the analysis grid.
///
/// A path containing [`TILE_PLACEHOLDER`] names a set of 1° × 1° tiles
/// (USGS 3DEP style, see [`degree_tiles`]). Each window then opens the
/// tiles it overlaps and mosaics them in memory (GDAL BuildVRT) before
/// warping, so a region may cross degree lines.
#[derive(Debug, Clone)]
pub struct GdalDem {
    path: PathBuf,
}

impl GdalDem {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Open the tiles covering `footprint` (lon/lat) as one dataset.
    fn open_tiles(&self, footprint: &BBox) -> Result<Dataset, IoError> {
        let template = self.path.to_string_lossy();
        let tiles = degree_tiles(footprint);
        debug!(?tiles, "opening DEM tiles");
        let mut datasets = tiles
            .iter()
            .map(|t| {
                let path = template.replace(TILE_PLACEHOLDER, t);
                Dataset::open(&path)
                    .map_err(|e| IoError::Invalid(format!("DEM tile {t} ({path}): {e}")))
            })
            .collect::<Result<Vec<_>, _>>()?;
        if datasets.len() == 1 {
            return Ok(datasets.remove(0));
        }
        Ok(gdal::programs::raster::build_vrt(None, &datasets, None)?)
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
        let tiled = self.path.to_string_lossy().contains(TILE_PLACEHOLDER);
        let single = if tiled {
            None
        } else {
            Some(Dataset::open(&self.path)?)
        };
        let dst_srs = gis_order(SpatialRef::from_epsg(req.dst_epsg)?);

        let res = match (req.target_res_m, &single) {
            (Some(r), _) if r.is_finite() && r > 0.0 => r,
            (Some(r), _) => {
                return Err(IoError::Invalid(format!(
                    "target_res_m must be > 0, got {r}"
                )));
            }
            (None, Some(src)) if src.spatial_ref()?.is_projected() => {
                let gt = src.geo_transform()?;
                gt[1].abs().min(gt[5].abs()) * src.spatial_ref()?.linear_units()
            }
            // Degree tiles are geographic, so they need a target resolution.
            (None, _) => return Err(IoError::NeedsTargetRes(self.describe())),
        };

        let wgs84 = gis_order(SpatialRef::from_epsg(4326)?);
        let to_dst = CoordTransform::new(&wgs84, &dst_srs)?;
        let b = req.bbox_wgs84;
        let [x0, y0, x1, y1] =
            to_dst.transform_bounds(&[b.min_x, b.min_y, b.max_x, b.max_y], 21)?;
        let (gt, rows, cols) = snapped_window(BBox::new(x0, y0, x1, y1).buffered(req.pad_m), res)?;
        debug!(rows, cols, res, epsg = req.dst_epsg, "warping DEM window");

        let src = match single {
            Some(src) => src,
            None => {
                // The window's lon/lat footprint, plus about 100 m so the
                // bilinear warp has source pixels at its edges.
                let g = gt.0;
                let (wx1, wy0) = (g[0] + g[1] * cols as f64, g[3] + g[5] * rows as f64);
                let to_wgs84 = CoordTransform::new(&dst_srs, &wgs84)?;
                let [lo_x, lo_y, hi_x, hi_y] =
                    to_wgs84.transform_bounds(&[g[0], wy0, wx1, g[3]], 21)?;
                let footprint = BBox::new(lo_x, lo_y, hi_x, hi_y).buffered(0.001);
                self.open_tiles(&footprint)?
            }
        };

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
        Ok(self.lonlat_to_many(&[(lon, lat)], epsg)?[0])
    }

    fn to_lonlat(&self, x: f64, y: f64, epsg: u32) -> Result<(f64, f64), IoError> {
        Ok(self.to_lonlat_many(&[(x, y)], epsg)?[0])
    }

    fn lonlat_to_many(&self, pts: &[(f64, f64)], epsg: u32) -> Result<Vec<(f64, f64)>, IoError> {
        transform_many(pts, 4326, epsg)
    }

    fn to_lonlat_many(&self, pts: &[(f64, f64)], epsg: u32) -> Result<Vec<(f64, f64)>, IoError> {
        transform_many(pts, epsg, 4326)
    }
}

/// Transform points between EPSG codes with one PROJ transformation, in
/// traditional GIS (x/lon, y/lat) axis order.
fn transform_many(pts: &[(f64, f64)], from: u32, to: u32) -> Result<Vec<(f64, f64)>, IoError> {
    if pts.is_empty() {
        return Ok(Vec::new());
    }
    let ct = CoordTransform::new(
        &gis_order(SpatialRef::from_epsg(from)?),
        &gis_order(SpatialRef::from_epsg(to)?),
    )?;
    let mut x: Vec<f64> = pts.iter().map(|p| p.0).collect();
    let mut y: Vec<f64> = pts.iter().map(|p| p.1).collect();
    let mut z = vec![0.0; pts.len()];
    ct.transform_coords(&mut x, &mut y, &mut z)?;
    Ok(x.into_iter().zip(y).collect())
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

/// Encode an RGBA image (row-major, 4 bytes per pixel) as lossless WebP,
/// through GDAL's WEBP driver and an in-memory file. Fully transparent
/// pixels may lose their (invisible) colour, as libwebp does by default.
pub fn encode_webp_lossless(rgba: &[u8], width: usize, height: usize) -> Result<Vec<u8>, IoError> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    if rgba.len() != width * height * 4 {
        return Err(IoError::Invalid(format!(
            "RGBA buffer has {} bytes, expected {}",
            rgba.len(),
            width * height * 4
        )));
    }
    let mem = DriverManager::get_driver_by_name("MEM")?
        .create_with_band_type::<u8, _>("", width, height, 4)?;
    for b in 0..4 {
        let plane: Vec<u8> = rgba.iter().skip(b).step_by(4).copied().collect();
        let mut band = mem.rasterband(b + 1)?;
        band.write(
            (0, 0),
            (width, height),
            &mut Buffer::new((width, height), plane),
        )?;
    }
    let name = format!(
        "/vsimem/ates_tile_{}.webp",
        NEXT.fetch_add(1, Ordering::Relaxed)
    );
    let mut opts = RasterCreationOptions::new();
    opts.set_name_value("LOSSLESS", "TRUE")?;
    let out = mem.create_copy(&DriverManager::get_driver_by_name("WEBP")?, &name, &opts)?;
    drop(out);
    let bytes = gdal::vsi::get_vsi_mem_file_bytes_owned(&name)?;
    // GDAL may add a sidecar for metadata; remove it if present.
    let _ = gdal::vsi::unlink_mem_file(format!("{name}.aux.xml"));
    Ok(bytes)
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
    fn webp_lossless_round_trip() {
        let rgba: Vec<u8> = (0..16 * 8)
            .flat_map(|i| [i as u8, 7, 200, (i % 3) as u8 * 100])
            .collect();
        let webp = encode_webp_lossless(&rgba, 16, 8).unwrap();
        assert_eq!(&webp[..4], b"RIFF");
        assert_eq!(&webp[8..12], b"WEBP");
        // Decode with GDAL. Lossless keeps alpha everywhere and colour
        // wherever alpha > 0; libwebp drops the colour of fully
        // transparent pixels, which an overlay never shows.
        let name = "/vsimem/webp_round_trip.webp";
        gdal::vsi::create_mem_file(name, webp).unwrap();
        let ds = Dataset::open(name).unwrap();
        assert_eq!(ds.raster_count(), 4);
        let planes: Vec<Vec<u8>> = (1..=4)
            .map(|b| {
                let band = ds.rasterband(b).unwrap();
                band.read_band_as::<u8>().unwrap().into_shape_and_vec().1
            })
            .collect();
        for (i, px) in rgba.chunks_exact(4).enumerate() {
            assert_eq!(planes[3][i], px[3], "alpha at {i}");
            if px[3] > 0 {
                let got = [planes[0][i], planes[1][i], planes[2][i]];
                assert_eq!(got, [px[0], px[1], px[2]], "colour at {i}");
            }
        }
        drop(ds);
        gdal::vsi::unlink_mem_file(name).unwrap();
        assert!(encode_webp_lossless(&rgba, 16, 9).is_err());
    }

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

    /// Write a lon/lat GeoTIFF of `cols` x `rows` pixels of 0.01° from
    /// (west, north), valued by a smooth function of lon/lat.
    fn write_degree_raster(path: &str, west: f64, north: f64, cols: usize, rows: usize) {
        let mut ds = DriverManager::get_driver_by_name("GTiff")
            .unwrap()
            .create_with_band_type::<f32, _>(path, cols, rows, 1)
            .unwrap();
        ds.set_geo_transform(&[west, 0.01, 0.0, north, 0.0, -0.01])
            .unwrap();
        ds.set_spatial_ref(&SpatialRef::from_epsg(4326).unwrap())
            .unwrap();
        let data: Vec<f32> = (0..rows)
            .flat_map(|r| {
                (0..cols).map(move |c| {
                    let lon = west + 0.01 * (c as f64 + 0.5);
                    let lat = north - 0.01 * (r as f64 + 0.5);
                    (3000.0 + 400.0 * (lon * 3.0).sin() + 250.0 * (lat * 5.0).cos()) as f32
                })
            })
            .collect();
        let mut buf = Buffer::new((cols, rows), data);
        ds.rasterband(1)
            .unwrap()
            .write((0, 0), (cols, rows), &mut buf)
            .unwrap();
    }

    #[test]
    fn degree_tiles_mosaic_like_one_raster() {
        // One raster over 107-105° W, 40-41° N, and the same split into the
        // two 1° tiles n41w107 and n41w106.
        write_degree_raster("/vsimem/mosaic_whole.tif", -107.0, 41.0, 200, 100);
        write_degree_raster("/vsimem/mosaic_tiles/n41w107.tif", -107.0, 41.0, 100, 100);
        write_degree_raster("/vsimem/mosaic_tiles/n41w106.tif", -106.0, 41.0, 100, 100);
        let req = WindowRequest {
            bbox_wgs84: BBox::new(-106.3, 40.3, -105.7, 40.7),
            pad_m: 2000.0,
            dst_epsg: 32613,
            target_res_m: Some(250.0),
            resampling: Default::default(),
        };
        let whole = GdalDem::new("/vsimem/mosaic_whole.tif")
            .read_window(&req)
            .unwrap();
        let tiled = GdalDem::new("/vsimem/mosaic_tiles/{tile}.tif")
            .read_window(&req)
            .unwrap();
        assert_eq!(tiled.transform, whole.transform);
        assert_eq!(tiled.data, whole.data, "mosaic warps like one raster");
        assert!(!tiled.data.iter().any(|&v| tiled.is_nodata(v)));

        // A missing tile is an error that names it.
        let req_far = WindowRequest {
            bbox_wgs84: BBox::new(-104.5, 40.3, -104.4, 40.4),
            ..req
        };
        let e = GdalDem::new("/vsimem/mosaic_tiles/{tile}.tif")
            .read_window(&req_far)
            .unwrap_err();
        assert!(e.to_string().contains("n41w105"), "{e}");
        // Degree tiles need a target resolution.
        let no_res = WindowRequest {
            target_res_m: None,
            ..req_far
        };
        assert!(matches!(
            GdalDem::new("/vsimem/mosaic_tiles/{tile}.tif").read_window(&no_res),
            Err(IoError::NeedsTargetRes(_))
        ));
        for f in [
            "/vsimem/mosaic_whole.tif",
            "/vsimem/mosaic_tiles/n41w107.tif",
            "/vsimem/mosaic_tiles/n41w106.tif",
        ] {
            gdal::vsi::unlink_mem_file(f).unwrap();
        }
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
