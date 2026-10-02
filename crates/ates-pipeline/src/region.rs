//! Region builds: the full AutoATES v2.0 chain on one padded window.
//!
//! ```text
//! DEM window (UTM, padded) ─┬─► PRA ─► Flow-Py ─► overhead ─┐
//! forest on the same grid ──┴───────────────────────────────┴─► classify ─► trim pad
//! ```
//!
//! The pad must hold every release area whose avalanches can reach the
//! region. Without a sourced runout length (`prep.pad_m` is a TODO), the
//! build measures it: after each run it takes the longest modelled
//! flow-path distance anywhere in the window. If that is not shorter than
//! the pad, it widens the pad and runs again.
//!
//! This is a heuristic, not a proof:
//! - A path from outside the window could in principle be longer than any
//!   path inside it.
//! - It relies on the region itself containing release areas. A window
//!   with none reports no runout, so any pad looks sufficient.
//!
//! Overhead exposure normalises cell counts by their maximum over the
//! whole padded window, as AutoATES does over its input raster.

use std::time::{Duration, Instant};

use ates_core::autoates::{FillNodata, OutputMode};
use ates_core::crs::utm_epsg_for;
use ates_core::overhead::overhead;
use ates_core::{BBox, Grid};
use ates_io::{GridSource, RasterSource, WindowRequest};

use crate::config::FlowPyForest;
use crate::{AutoAtesInputs, Params, PipelineError, run_autoates, run_flowpy, run_pra};

/// First pad tried when neither the config nor the caller sets one. It is
/// only a starting point: the build widens it until it exceeds the longest
/// modelled runout.
pub const INITIAL_PAD_M: f64 = 1000.0;

/// Largest pad tried. Flow-Py caps its distance output at 10 000 m.
pub const MAX_PAD_M: f64 = 10_000.0;

/// Where a region build reads its data.
#[derive(Clone, Copy)]
pub struct RegionSources<'a> {
    pub dem: &'a dyn RasterSource,
    /// Percent canopy cover (or the configured forest type), delivered on
    /// the DEM window's grid.
    pub forest: &'a dyn GridSource,
}

/// All layers of a region build, trimmed to the region.
#[derive(Debug, Clone)]
pub struct RegionLayers {
    pub dem: Grid<f32>,
    pub forest: Grid<f32>,
    pub pra_binary: Grid<i16>,
    pub pra_continuous: Grid<i16>,
    pub fp_travel_angle: Grid<f32>,
    pub cell_counts: Grid<f32>,
    pub z_delta: Grid<f32>,
    pub overhead: Grid<i16>,
    /// ATES classes 0-4, nodata -9999 (Product mode).
    pub ates: Grid<i16>,
}

/// One attempt with a given pad.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PadAttempt {
    pub pad_m: f64,
    /// Longest flow-path distance in the window (m).
    pub max_runout_m: f64,
}

#[derive(Debug, Clone)]
pub struct RegionBuild {
    pub layers: RegionLayers,
    pub attempts: Vec<PadAttempt>,
    /// Whether the final pad exceeds the longest modelled runout.
    pub pad_sufficient: bool,
    /// Release cells in the padded window.
    pub release_cells: usize,
    pub timings: Vec<(&'static str, Duration)>,
}

/// Run the full chain for `bbox_wgs84`.
pub fn build_region(
    sources: RegionSources<'_>,
    bbox_wgs84: BBox,
    params: &Params,
    filler: &dyn FillNodata,
    initial_pad_m: f64,
) -> Result<RegionBuild, PipelineError> {
    let (lon, lat) = bbox_wgs84.center();
    let epsg = utm_epsg_for(lon, lat)?;
    let max_z = params.flowpy_params()?.max_z_delta;
    let mut pad = initial_pad_m;
    let mut attempts = Vec::new();
    loop {
        let mut timings = Vec::new();
        let mut timed = |name, t: Instant| timings.push((name, t.elapsed()));

        let t = Instant::now();
        let req = WindowRequest {
            bbox_wgs84,
            pad_m: pad,
            dst_epsg: epsg,
            target_res_m: params.dem.target_res_m,
            resampling: params.dem.resampling,
        };
        let dem = sources.dem.read_window(&req)?;
        timed("read DEM", t);
        let t = Instant::now();
        let forest = sources.forest.read_on(&dem)?;
        timed("read forest", t);

        let t = Instant::now();
        let pra = run_pra(&dem, Some(&forest), params)?;
        timed("PRA", t);
        let release = to_f32(&pra.binary);
        let release_cells = release.data.iter().filter(|&&v| v > 0.0).count();

        let t = Instant::now();
        let fp_forest = match params.flowpy.forest {
            FlowPyForest::None => None,
            FlowPyForest::PccFraction => Some(Grid {
                data: forest.data.mapv(|v| {
                    if forest.is_nodata(v) {
                        0.0
                    } else {
                        (v / 100.0).clamp(0.0, 1.0)
                    }
                }),
                ..forest.clone()
            }),
        };
        let flow = run_flowpy(&dem, &release, fp_forest.as_ref(), params)?;
        timed("Flow-Py", t);

        let max_runout_m = flow
            .fp_distance
            .data
            .iter()
            .zip(flow.cell_counts.data.iter())
            .filter(|&(_, &c)| c > 0.0)
            .map(|(&d, _)| f64::from(d))
            .fold(0.0, f64::max);
        attempts.push(PadAttempt {
            pad_m: pad,
            max_runout_m,
        });
        let pad_sufficient = max_runout_m < pad;
        if !pad_sufficient && pad < MAX_PAD_M {
            // Widen to 1.5x the longest runout, in whole 500 m steps.
            pad = ((max_runout_m * 1.5 / 500.0).ceil() * 500.0).clamp(pad + 500.0, MAX_PAD_M);
            continue;
        }

        let t = Instant::now();
        let ovh = overhead(&flow.cell_counts.data, &flow.z_delta.data, max_z, None)?;
        let ovh = Grid {
            data: ovh,
            transform: dem.transform,
            crs: dem.crs.clone(),
            nodata: None,
        };
        let inputs = AutoAtesInputs {
            dem: &dem,
            forest: &forest,
            flowpy_fp: &flow.fp_travel_angle,
            cell_count: &to_f32(&ovh),
            pra: &release,
        };
        let classes = run_autoates(inputs, params, filler, OutputMode::Product)?;
        timed("classify", t);

        let n = (pad / dem.transform.ew_res()).floor() as usize;
        let layers = RegionLayers {
            dem: dem.trim(n),
            forest: forest.trim(n),
            pra_binary: pra.binary.trim(n),
            pra_continuous: pra.continuous.trim(n),
            fp_travel_angle: flow.fp_travel_angle.trim(n),
            cell_counts: flow.cell_counts.trim(n),
            z_delta: flow.z_delta.trim(n),
            overhead: ovh.trim(n),
            ates: classes.ates.trim(n),
        };
        return Ok(RegionBuild {
            layers,
            attempts,
            pad_sufficient,
            release_cells,
            timings,
        });
    }
}

fn to_f32(g: &Grid<i16>) -> Grid<f32> {
    Grid {
        data: g.data.mapv(f32::from),
        transform: g.transform,
        crs: g.crs.clone(),
        nodata: g.nodata,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Config;
    use ates_core::{Crs, GeoTransform};
    use ates_io::IoError;
    use ndarray::Array2;

    /// A 40 degree cone 300 m in radius on flat ground, centred on the
    /// request, on whatever grid is asked for.
    struct Cone;

    impl RasterSource for Cone {
        fn describe(&self) -> String {
            "synthetic cone".into()
        }

        fn read_window(&self, req: &WindowRequest) -> Result<Grid<f32>, IoError> {
            let res = req.target_res_m.unwrap();
            // The region itself is 600 m across, plus the pad on each side.
            let n = 2 * ((req.pad_m + 300.0) / res).ceil() as usize + 1;
            let half = n as f64 * res / 2.0;
            let gt = GeoTransform::north_up(-half, half, res, res);
            let tan40 = 40.0_f64.to_radians().tan();
            let data = Array2::from_shape_fn((n, n), |(r, c)| {
                let (x, y) = gt.pixel_center(r, c);
                let d = x.hypot(y).min(300.0);
                (1000.0 + tan40 * (300.0 - d)) as f32
            });
            Ok(Grid::new(data, gt, Crs::Epsg(req.dst_epsg), Some(-9999.0))?)
        }
    }

    /// No forest anywhere.
    struct Open;

    impl GridSource for Open {
        fn describe(&self) -> String {
            "no forest".into()
        }

        fn read_on(&self, like: &Grid<f32>) -> Result<Grid<f32>, IoError> {
            Ok(Grid {
                data: Array2::zeros(like.data.dim()),
                ..like.clone()
            })
        }
    }

    struct KeepValues;

    impl FillNodata for KeepValues {
        fn fill(&self, values: &Grid<i16>, _: &Array2<u8>, _: f64) -> Result<Array2<i16>, String> {
            Ok(values.data.clone())
        }
    }

    fn params() -> Params {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/default.toml");
        let mut p = Config::load(&path)
            .unwrap()
            .params(Some("cameron_pass"))
            .unwrap();
        p.dem.target_res_m = Some(20.0);
        p
    }

    #[test]
    fn chain_runs_and_pad_grows_past_the_runout() {
        let sources = RegionSources {
            dem: &Cone,
            forest: &Open,
        };
        let b = build_region(
            sources,
            BBox::point(-105.875, 40.515),
            &params(),
            &KeepValues,
            100.0,
        )
        .unwrap();
        assert!(b.release_cells > 0, "the cone's flanks release");
        assert!(b.attempts.len() >= 2, "100 m is shorter than the runout");
        let last = b.attempts.last().unwrap();
        assert!(b.pad_sufficient && last.max_runout_m < last.pad_m);
        assert!(last.max_runout_m > 0.0);
        // Trimmed back to the 600 m region.
        assert_eq!(b.layers.ates.rows(), 31);
        assert!(b.layers.ates.data.iter().any(|&c| c >= 3), "steep cone");
        assert_eq!(b.layers.ates.crs, Crs::Epsg(32613));
    }
}
