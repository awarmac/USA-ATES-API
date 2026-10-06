//! Backend-neutral raster I/O traits.

use std::path::Path;

use ates_core::{BBox, Crs, GeoTransform, Grid, GridError, crs::CrsError};
use serde::Deserialize;
use thiserror::Error;

use crate::Provenance;

#[derive(Debug, Error)]
pub enum IoError {
    #[cfg(feature = "gdal")]
    #[error("GDAL: {0}")]
    Gdal(#[from] gdal::errors::GdalError),
    #[error(transparent)]
    Grid(#[from] GridError),
    #[error(transparent)]
    Crs(#[from] CrsError),
    #[error("{0} uses a geographic CRS; set a target resolution in metres")]
    NeedsTargetRes(String),
    #[error("requested window is {rows}x{cols} cells, above the limit of {max}")]
    WindowTooLarge {
        rows: usize,
        cols: usize,
        max: usize,
    },
    #[error("{0} has no data inside the requested window")]
    NoCoverage(String),
    #[error("bands to write must share shape, transform and CRS ({0})")]
    BandMismatch(String),
    #[error("{0}")]
    Invalid(String),
}

/// Resampling used when warping a DEM onto the analysis grid.
///
/// Only bilinear is offered for now: it is what the `gdal` crate's
/// `reproject` uses, and it is the usual choice for continuous elevation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Resampling {
    #[default]
    Bilinear,
}

/// A request for a DEM window on a projected analysis grid.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowRequest {
    /// Area of interest in WGS 84 lon/lat degrees.
    pub bbox_wgs84: BBox,
    /// Extra margin around the area of interest, in metres of the target CRS.
    pub pad_m: f64,
    /// EPSG code of the projected target CRS (metres).
    pub dst_epsg: u32,
    /// Cell size in metres; `None` keeps the source resolution, which is
    /// only possible for projected sources.
    pub target_res_m: Option<f64>,
    pub resampling: Resampling,
}

/// Anything that can produce an elevation (or other single-band) window.
pub trait RasterSource {
    /// Human-readable identity of the data, stamped into provenance.
    fn describe(&self) -> String;

    fn read_window(&self, req: &WindowRequest) -> Result<Grid<f32>, IoError>;
}

/// A raster delivered directly on a given grid, such as a web image
/// service that resamples on the server.
pub trait GridSource {
    /// Human-readable identity of the data, stamped into provenance.
    fn describe(&self) -> String;

    /// Values on exactly the grid of `like` (same transform, size, CRS).
    fn read_on(&self, like: &Grid<f32>) -> Result<Grid<f32>, IoError>;
}

/// Pixel data of a band to write.
#[derive(Debug, Clone, Copy)]
pub enum BandData<'a> {
    F32(&'a Grid<f32>),
    /// Class codes; written as Int16 when every band is Int16.
    I16(&'a Grid<i16>),
}

/// A named band to write.
#[derive(Debug, Clone, Copy)]
pub struct Band<'a> {
    pub name: &'a str,
    pub data: BandData<'a>,
}

impl<'a> Band<'a> {
    pub fn f32(name: &'a str, grid: &'a Grid<f32>) -> Self {
        Self {
            name,
            data: BandData::F32(grid),
        }
    }

    pub fn i16(name: &'a str, grid: &'a Grid<i16>) -> Self {
        Self {
            name,
            data: BandData::I16(grid),
        }
    }

    pub fn dim(&self) -> (usize, usize) {
        match self.data {
            BandData::F32(g) => g.data.dim(),
            BandData::I16(g) => g.data.dim(),
        }
    }

    pub fn transform(&self) -> GeoTransform {
        match self.data {
            BandData::F32(g) => g.transform,
            BandData::I16(g) => g.transform,
        }
    }

    pub fn crs(&self) -> &Crs {
        match self.data {
            BandData::F32(g) => &g.crs,
            BandData::I16(g) => &g.crs,
        }
    }

    pub fn nodata(&self) -> Option<f64> {
        match self.data {
            BandData::F32(g) => g.nodata,
            BandData::I16(g) => g.nodata,
        }
    }
}

/// Anything that can persist one or more aligned bands with provenance.
pub trait RasterSink {
    fn write(&self, bands: &[Band<'_>], path: &Path, prov: &Provenance) -> Result<(), IoError>;
}

/// Converts WGS 84 lon/lat to map coordinates of a projected CRS.
pub trait Projector {
    fn lonlat_to(&self, lon: f64, lat: f64, epsg: u32) -> Result<(f64, f64), IoError>;

    /// Projected coordinates in `epsg` back to WGS 84 (lon, lat).
    fn to_lonlat(&self, x: f64, y: f64, epsg: u32) -> Result<(f64, f64), IoError>;

    /// [`Projector::lonlat_to`] for many points.
    fn lonlat_to_many(&self, pts: &[(f64, f64)], epsg: u32) -> Result<Vec<(f64, f64)>, IoError> {
        pts.iter()
            .map(|&(lon, lat)| self.lonlat_to(lon, lat, epsg))
            .collect()
    }

    /// [`Projector::to_lonlat`] for many points.
    fn to_lonlat_many(&self, pts: &[(f64, f64)], epsg: u32) -> Result<Vec<(f64, f64)>, IoError> {
        pts.iter()
            .map(|&(x, y)| self.to_lonlat(x, y, epsg))
            .collect()
    }
}

/// Check that all bands share shape, transform and CRS.
pub fn check_aligned(bands: &[Band<'_>]) -> Result<(), IoError> {
    let Some(first) = bands.first() else {
        return Err(IoError::BandMismatch("no bands given".into()));
    };
    for b in &bands[1..] {
        if b.dim() != first.dim() || b.transform() != first.transform() || b.crs() != first.crs() {
            return Err(IoError::BandMismatch(format!(
                "{} vs {}",
                b.name, first.name
            )));
        }
    }
    Ok(())
}

/// Placeholder in a DEM path for the names of 1° × 1° tiles, e.g.
/// `.../13/TIFF/current/{tile}/USGS_13_{tile}.tif` for USGS 3DEP.
pub const TILE_PLACEHOLDER: &str = "{tile}";

/// Names of the 1° × 1° tiles covering a WGS 84 lon/lat box, in the USGS
/// 3DEP style: the tile's north edge and west edge, e.g. `n41w106` for
/// 40–41° N, 106–105° W. Edges lying exactly on a degree line do not pull
/// in the neighbouring tile. Ordered north to south, west to east.
pub fn degree_tiles(b: &BBox) -> Vec<String> {
    let span = |lo: f64, hi: f64| {
        let first = lo.floor() as i32;
        let last = ((hi.ceil() as i32) - 1).max(first);
        first..=last
    };
    let mut out = Vec::new();
    for lat in span(b.min_y, b.max_y).rev() {
        let north = lat + 1;
        let ns = if north >= 0 { 'n' } else { 's' };
        for lon in span(b.min_x, b.max_x) {
            let ew = if lon < 0 { 'w' } else { 'e' };
            out.push(format!(
                "{ns}{:02}{ew}{:03}",
                north.unsigned_abs(),
                lon.unsigned_abs()
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ndarray::Array2;

    #[test]
    fn degree_tile_names() {
        // Cameron Pass, inside one tile.
        let one = BBox::new(-105.95, 40.45, -105.80, 40.58);
        assert_eq!(degree_tiles(&one), ["n41w106"]);
        // Across 106° W and 41° N.
        let four = BBox::new(-106.2, 40.8, -105.9, 41.1);
        assert_eq!(
            degree_tiles(&four),
            ["n42w107", "n42w106", "n41w107", "n41w106"]
        );
        // A box ending exactly on a degree line stays in one tile.
        let edge = BBox::new(-106.0, 40.0, -105.0, 41.0);
        assert_eq!(degree_tiles(&edge), ["n41w106"]);
        assert_eq!(
            degree_tiles(&BBox::new(-99.5, 35.2, -99.4, 35.3)),
            ["n36w100"]
        );
        assert_eq!(degree_tiles(&BBox::new(7.5, -0.5, 7.6, -0.4)), ["n00e007"]);
    }

    fn grid(rows: usize, epsg: u32) -> Grid<f32> {
        Grid::new(
            Array2::zeros((rows, 2)),
            GeoTransform::north_up(0.0, 0.0, 1.0, 1.0),
            Crs::Epsg(epsg),
            None,
        )
        .unwrap()
    }

    #[test]
    fn aligned_bands() {
        let (a, b, c, d) = (
            grid(2, 32611),
            grid(2, 32611),
            grid(3, 32611),
            grid(2, 32612),
        );
        let band = Band::f32;
        assert!(check_aligned(&[band("a", &a), band("b", &b)]).is_ok());
        assert!(check_aligned(&[band("a", &a), band("c", &c)]).is_err());
        assert!(check_aligned(&[band("a", &a), band("d", &d)]).is_err());
        assert!(check_aligned(&[]).is_err());
    }

    #[test]
    fn resampling_parses_lowercase() {
        #[derive(Deserialize)]
        struct T {
            r: Resampling,
        }
        let t: T = toml::from_str("r = \"bilinear\"").unwrap();
        assert_eq!(t.r, Resampling::Bilinear);
        assert!(toml::from_str::<T>("r = \"cubic\"").is_err());
    }
}
