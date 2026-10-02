//! Orchestration for the ATES estimator.
//!
//! Every request reduces to one path: take the request extent, read a
//! padded DEM window on a local UTM grid, compute on the whole window, then
//! trim the pad.
//!
//! Products so far:
//! - [`terrain`]: slope, aspect and the slope-band proxy classes for any
//!   DEM window (crude proxy, **not** ATES);
//! - [`run_pra`]: AutoATES v2.0 potential release areas from a DEM and an
//!   optional forest raster;
//! - [`run_flowpy`]: Flow-Py runout from release areas;
//! - [`run_autoates`]: the AutoATES v2.0 classification, given PRA and
//!   Flow-Py rasters.

pub mod config;
pub mod region;
pub mod route;

use ates_core::autoates::{self, AutoAtesLayers, FillNodata, OutputMode};
use ates_core::classify::{ClassifyError, TerrainLayers, slope_classes};
use ates_core::crs::utm_epsg_for;
use ates_core::flowpy::{self, FlowPyLayers};
use ates_core::pra::{self, PraLayers};
use ates_core::terrain::{aspect_deg, slope_deg};
use ates_core::{BBox, Grid};
use ates_io::{IoError, Projector, Provenance, RasterSource, WindowRequest};
use thiserror::Error;

pub use config::{Config, ConfigError, Params};

pub const TOOL_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Error)]
pub enum PipelineError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Io(#[from] IoError),
    #[error(transparent)]
    Crs(#[from] ates_core::crs::CrsError),
    #[error(transparent)]
    Classify(#[from] ClassifyError),
    #[error(transparent)]
    Route(#[from] ates_core::route::RouteError),
    #[error("point ({lon}, {lat}) fell outside the computed window")]
    PointOutside { lon: f64, lat: f64 },
}

/// Terrain derivatives for a request, with the pad already trimmed.
#[derive(Debug, Clone)]
pub struct TerrainProduct {
    pub dem: Grid<f32>,
    pub slope_deg: Grid<f32>,
    pub aspect_deg: Grid<f32>,
    /// Slope-band proxy classes 0-4 (AutoATES slope step only; not ATES).
    pub slope_class: Grid<i16>,
}

/// Read a padded DEM window around `bbox_wgs84`, compute slope and aspect on
/// the full window, and trim the pad so edge effects stay outside the result.
pub fn terrain(
    source: &dyn RasterSource,
    bbox_wgs84: BBox,
    params: &Params,
) -> Result<TerrainProduct, PipelineError> {
    let pad_m = params.pad_m()?;
    let (lon, lat) = bbox_wgs84.center();
    let req = WindowRequest {
        bbox_wgs84,
        pad_m,
        dst_epsg: utm_epsg_for(lon, lat)?,
        target_res_m: params.dem.target_res_m,
        resampling: params.dem.resampling,
    };
    let dem = source.read_window(&req)?;
    let edges = params.terrain.compute_edges;
    let (slope, aspect) = (slope_deg(&dem, edges), aspect_deg(&dem, edges));
    // Classify on the full window so the smoothing sees real neighbours.
    let classes = slope_classes(&slope, &params.slope_thresholds()?)?.classes;

    let pad_cells = (pad_m / dem.transform.ew_res()).floor() as usize;
    Ok(TerrainProduct {
        dem: dem.trim(pad_cells),
        slope_deg: slope.trim(pad_cells),
        aspect_deg: aspect.trim(pad_cells),
        slope_class: classes.trim(pad_cells),
    })
}

/// Terrain values at one location.
#[derive(Debug, Clone, PartialEq)]
pub struct PointResult {
    pub lon: f64,
    pub lat: f64,
    pub epsg: u32,
    /// Projected coordinates of the point.
    pub x: f64,
    pub y: f64,
    pub elevation: Option<f32>,
    pub slope_deg: Option<f32>,
    pub aspect_deg: Option<f32>,
    /// Slope-band proxy class, `None` where nodata.
    pub slope_class: Option<i16>,
}

/// Terrain at a single point, through the same padded-window path as areas.
pub fn point(
    source: &dyn RasterSource,
    projector: &dyn Projector,
    lon: f64,
    lat: f64,
    params: &Params,
) -> Result<PointResult, PipelineError> {
    let product = terrain(source, BBox::point(lon, lat), params)?;
    let epsg = utm_epsg_for(lon, lat)?;
    let (x, y) = projector.lonlat_to(lon, lat, epsg)?;
    let (r, c) = product
        .slope_class
        .cell_at(x, y)
        .ok_or(PipelineError::PointOutside { lon, lat })?;
    let f = |g: &Grid<f32>| Some(g.data[[r, c]]).filter(|&v| !g.is_nodata(v));
    let class = product.slope_class.data[[r, c]];
    Ok(PointResult {
        lon,
        lat,
        epsg,
        x,
        y,
        elevation: f(&product.dem),
        slope_deg: f(&product.slope_deg),
        aspect_deg: f(&product.aspect_deg),
        slope_class: (class != ates_core::classify::CLASS_NODATA).then_some(class),
    })
}

/// Rasters AutoATES classification needs, all on the DEM's grid.
#[derive(Debug, Clone, Copy)]
pub struct AutoAtesInputs<'a> {
    pub dem: &'a Grid<f32>,
    pub forest: &'a Grid<f32>,
    /// Flow-Py flow-path travel angle (degrees).
    pub flowpy_fp: &'a Grid<f32>,
    /// Flow-Py cell counts.
    pub cell_count: &'a Grid<f32>,
    /// Release areas, 1 = release.
    pub pra: &'a Grid<f32>,
}

/// AutoATES v2.0 classification from externally produced PRA and Flow-Py
/// rasters. Slope is computed here from the DEM, as AutoATES does.
pub fn run_autoates(
    inputs: AutoAtesInputs<'_>,
    params: &Params,
    filler: &dyn FillNodata,
    mode: OutputMode,
) -> Result<AutoAtesLayers, PipelineError> {
    let slope = slope_deg(inputs.dem, params.terrain.compute_edges);
    let layers = TerrainLayers {
        dem: Some(inputs.dem),
        slope_deg: Some(&slope),
        forest: Some(inputs.forest),
        flowpy_fp: Some(inputs.flowpy_fp),
        cell_count: Some(inputs.cell_count),
        pra: Some(inputs.pra),
    };
    Ok(autoates::classify(
        &layers,
        &params.autoates()?,
        filler,
        mode,
    )?)
}

/// AutoATES v2.0 potential release areas on the DEM's grid. `forest` must
/// be on the same grid; without it AutoATES's `no_forest` mode applies.
pub fn run_pra(
    dem: &Grid<f32>,
    forest: Option<&Grid<f32>>,
    params: &Params,
) -> Result<PraLayers, PipelineError> {
    let p = params.pra_params(dem.transform.ew_res(), forest.is_some())?;
    Ok(pra::pra(dem, forest, &p)?)
}

/// Flow-Py runout from the release cells (> 0) of `release`. `forest` is
/// Flow-Py's forest layer (0-1), not the classifier's forest density.
pub fn run_flowpy(
    dem: &Grid<f32>,
    release: &Grid<f32>,
    forest: Option<&Grid<f32>>,
    params: &Params,
) -> Result<FlowPyLayers, PipelineError> {
    Ok(flowpy::flowpy(
        dem,
        release,
        forest,
        &params.flowpy_params()?,
    )?)
}

/// Provenance for a run.
pub fn provenance(
    config: &Config,
    region: Option<&str>,
    dem_source: &str,
    product: &str,
) -> Provenance {
    Provenance {
        tool_version: TOOL_VERSION.to_owned(),
        config_source: config.source.clone(),
        config_hash: config.hash.clone(),
        region: region.map(str::to_owned),
        dem_source: dem_source.to_owned(),
        product: product.to_owned(),
    }
}

/// Agreement between two aligned rasters, over cells valid in both.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Agreement {
    pub compared: usize,
    /// Cells valid in exactly one of the two rasters.
    pub validity_mismatches: usize,
    pub max_abs_diff: f64,
    pub mean_abs_diff: f64,
}

/// Compare `ours` against `reference`. With `circular`, differences wrap at
/// 360 degrees (for aspect).
///
/// # Panics
/// If the two grids have different shapes.
pub fn compare(ours: &Grid<f32>, reference: &Grid<f32>, circular: bool) -> Agreement {
    assert_eq!(
        ours.data.dim(),
        reference.data.dim(),
        "grids must be aligned"
    );
    let (mut n, mut mismatch, mut max, mut sum) = (0, 0, 0.0_f64, 0.0_f64);
    for (&a, &b) in ours.data.iter().zip(reference.data.iter()) {
        match (ours.is_nodata(a), reference.is_nodata(b)) {
            (true, true) => {}
            (false, false) => {
                let mut d = (f64::from(a) - f64::from(b)).abs();
                if circular {
                    d = d.min(360.0 - d);
                }
                n += 1;
                max = max.max(d);
                sum += d;
            }
            _ => mismatch += 1,
        }
    }
    Agreement {
        compared: n,
        validity_mismatches: mismatch,
        max_abs_diff: max,
        mean_abs_diff: if n > 0 { sum / n as f64 } else { 0.0 },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ates_core::{Crs, GeoTransform};
    use ndarray::Array2;

    /// A synthetic source: a tilted plane on whatever grid is requested.
    struct Plane;

    impl RasterSource for Plane {
        fn describe(&self) -> String {
            "synthetic plane".into()
        }

        fn read_window(&self, req: &WindowRequest) -> Result<Grid<f32>, IoError> {
            let res = req.target_res_m.unwrap_or(10.0);
            let n = 2 * (req.pad_m / res) as usize + 1;
            let gt = GeoTransform::north_up(0.0, n as f64 * res, res, res);
            let data = Array2::from_shape_fn((n, n), |(r, c)| gt.pixel_center(r, c).0 as f32);
            Ok(Grid::new(data, gt, Crs::Epsg(req.dst_epsg), None)?)
        }
    }

    fn params(pad: Option<f64>) -> Params {
        let mut p = Config::parse("schema_version = 1", "t")
            .unwrap()
            .params(None)
            .unwrap();
        p.prep.pad_m = pad;
        p.dem.target_res_m = Some(10.0);
        let c = &mut p.classify;
        (c.sat01, c.sat12, c.sat23, c.sat34, c.win_size) =
            (Some(15.0), Some(18.0), Some(28.0), Some(39.0), Some(3));
        p
    }

    /// Returns fixed coordinates in the synthetic plane's frame.
    struct FixedProjector(f64, f64);

    impl Projector for FixedProjector {
        fn lonlat_to(&self, _: f64, _: f64, _: u32) -> Result<(f64, f64), IoError> {
            Ok((self.0, self.1))
        }

        fn to_lonlat(&self, x: f64, y: f64, _: u32) -> Result<(f64, f64), IoError> {
            Ok((x, y))
        }
    }

    #[test]
    fn point_samples_the_cell_under_the_point() {
        // 11x11 window of 10 m cells from (0, 110); 5 pad cells trimmed
        // leave the cell spanning x, y in [50, 60).
        let r = point(
            &Plane,
            &FixedProjector(55.0, 55.0),
            -116.5,
            51.7,
            &params(Some(50.0)),
        )
        .unwrap();
        assert_eq!(r.epsg, 32611);
        assert!((r.slope_deg.unwrap() - 45.0).abs() < 1e-3);
        // 45 degrees, and the smoothed slope 45 > SAT34 = 39: class 4.
        assert_eq!(r.slope_class, Some(4));
        assert!(r.elevation.is_some());
        let outside = point(
            &Plane,
            &FixedProjector(5.0, 5.0),
            -116.5,
            51.7,
            &params(Some(50.0)),
        );
        assert!(matches!(outside, Err(PipelineError::PointOutside { .. })));
    }

    #[test]
    fn terrain_needs_slope_thresholds() {
        let mut p = params(Some(50.0));
        p.classify.sat34 = None;
        let err = terrain(&Plane, BBox::point(-116.5, 51.7), &p).unwrap_err();
        assert!(matches!(
            err,
            PipelineError::Config(ConfigError::Todo("classify.sat34"))
        ));
    }

    #[test]
    fn terrain_requires_pad() {
        let err = terrain(&Plane, BBox::point(-116.5, 51.7), &params(None)).unwrap_err();
        assert!(matches!(
            err,
            PipelineError::Config(ConfigError::Todo("prep.pad_m"))
        ));
    }

    #[test]
    fn terrain_trims_pad_and_uses_utm() {
        let out = terrain(&Plane, BBox::point(-116.5, 51.7), &params(Some(50.0))).unwrap();
        // 11x11 window, 5 cells of pad trimmed from each side.
        assert_eq!(out.slope_deg.data.dim(), (1, 1));
        assert_eq!(out.slope_deg.crs, Crs::Epsg(32611));
        assert!((out.slope_deg.data[[0, 0]] - 45.0).abs() < 1e-3);
        assert!((out.aspect_deg.data[[0, 0]] - 270.0).abs() < 1e-3);
        assert_eq!(out.slope_class.data.dim(), (1, 1));
        assert_eq!(out.slope_class.data[[0, 0]], 4);
    }

    #[test]
    fn compare_counts_and_wraps() {
        let gt = GeoTransform::north_up(0.0, 0.0, 1.0, 1.0);
        let g = |v: [f32; 4]| {
            Grid::new(
                Array2::from_shape_vec((2, 2), v.to_vec()).unwrap(),
                gt,
                Crs::Epsg(32611),
                Some(-9999.0),
            )
            .unwrap()
        };
        let a = compare(
            &g([359.0, 10.0, -9999.0, 5.0]),
            &g([1.0, 10.5, -9999.0, -9999.0]),
            true,
        );
        assert_eq!((a.compared, a.validity_mismatches), (2, 1));
        assert!((a.max_abs_diff - 2.0).abs() < 1e-9);
        let a = compare(
            &g([359.0, 10.0, 0.0, 0.0]),
            &g([1.0, 10.0, 0.0, 0.0]),
            false,
        );
        assert!((a.max_abs_diff - 358.0).abs() < 1e-9);
    }
}
