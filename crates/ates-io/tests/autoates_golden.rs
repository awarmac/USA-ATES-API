//! Golden tests for the AutoATES v2.0 port: every intermediate raster and
//! the final `ates_gen.tif` must match AutoATES's own Bow Summit outputs
//! cell for cell, using AutoATES's own PRA and Flow-Py inputs.
//!
//! Fixtures: `tests/fixtures/bow_summit/autoates` (see its README).
//! Requires the GDAL library (feature `gdal`): reading the rasters and the
//! `GDALFillNodata` cleanup step both go through GDAL.
#![cfg(feature = "gdal")]

use std::path::{Path, PathBuf};

use ates_core::Grid;
use ates_core::autoates::{self, AutoAtesParams, ForestThresholds, OutputMode};
use ates_core::classify::{CLASS_NODATA, SlopeThresholds, TerrainLayers};
use ates_core::terrain::slope_deg;
use ates_io::gdal_backend::{GdalFill, read_grid};
use ndarray::Array2;

/// AutoATES_classifier.py @ 3afcb49 defaults with forest_type 'bav', as
/// recorded in `outputs/inputpara.csv`. `config/default.toml` holds the same
/// values (checked by the ates-pipeline config tests).
const PARAMS: AutoAtesParams = AutoAtesParams {
    slope: SlopeThresholds {
        sat01: 15.0,
        sat12: 18.0,
        sat23: 28.0,
        sat34: 39.0,
        win_size: 3,
    },
    aat1: 18.0,
    aat2: 24.0,
    aat3: 33.0,
    forest: ForestThresholds {
        tree1: 10.0,
        tree2: 20.0,
        tree3: 25.0,
    },
    cc1: 5.0,
    cc2: 40.0,
    isl_size_m2: 30000.0,
};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/bow_summit")
        .join(name)
}

fn read(name: &str) -> Grid<f32> {
    read_grid(&fixture(name)).unwrap_or_else(|e| panic!("{name}: {e}"))
}

fn assert_same(name: &str, ours: &Array2<i16>, reference: &str) {
    let r = read(reference);
    assert_eq!(ours.dim(), r.data.dim(), "{name}: shape");
    let bad: Vec<_> = ours
        .indexed_iter()
        .filter(|&(ix, &v)| f32::from(v) != r.data[ix])
        .take(5)
        .collect();
    let count = ours
        .iter()
        .zip(r.data.iter())
        .filter(|&(&a, &b)| f32::from(a) != b)
        .count();
    assert_eq!(count, 0, "{name}: {count} cells differ, first: {bad:?}");
}

fn run(mode: OutputMode) -> autoates::AutoAtesLayers {
    let dem = read("dem.tif");
    let slope = slope_deg(&dem, false);
    let forest = read("autoates/forest.tif");
    let fp = read("autoates/FP_int16.tif");
    let cc = read("autoates/Overhead.tif");
    let pra = read("autoates/pra_binary.tif");
    let layers = TerrainLayers {
        dem: Some(&dem),
        slope_deg: Some(&slope),
        forest: Some(&forest),
        flowpy_fp: Some(&fp),
        cell_count: Some(&cc),
        pra: Some(&pra),
    };
    autoates::classify(&layers, &PARAMS, &GdalFill, mode).unwrap()
}

#[test]
fn every_intermediate_matches_autoates() {
    let o = run(OutputMode::OracleParity);
    assert_same("slope", &o.slope_class.data, "autoates/out_slope.tif");
    assert_same(
        "slope_smooth",
        &o.slope_smooth.data,
        "autoates/out_slope_smooth.tif",
    );
    assert_same("flowpy", &o.flowpy, "autoates/out_flowpy.tif");
    assert_same(
        "cellcount",
        &o.cellcount,
        "autoates/out_cellcount_reclass.tif",
    );
    assert_same("forest", &o.forest, "autoates/out_forest_reclass.tif");
    assert_same("pra", &o.pra, "autoates/out_SZ_reclass.tif");
    assert_same("merge_new", &o.merge_new, "autoates/out_merge_new.tif");
    assert_same("merge_all", &o.merge_all, "autoates/out_merge_all.tif");
}

#[test]
fn final_classes_match_ates_gen() {
    let o = run(OutputMode::OracleParity);
    // 30000 m^2 / (25.74 m x 25.79 m) = 45.2 -> 45 cells; 1095 cells refilled.
    assert_eq!(o.cleanup_mask.iter().filter(|&&m| m == 0).count(), 1095);
    assert_same("ates_gen", &o.ates.data, "autoates/out_ates_gen.tif");
}

#[test]
fn product_mode_differs_only_in_class0_and_masking() {
    let parity = run(OutputMode::OracleParity).ates;
    let product = run(OutputMode::Product).ates;
    let (dem, forest) = (read("dem.tif"), read("autoates/forest.tif"));
    let mut masked = 0;
    for (ix, &q) in product.data.indexed_iter() {
        let p = parity.data[ix];
        if dem.is_nodata(dem.data[ix]) || forest.is_nodata(forest.data[ix]) {
            assert_eq!(q, CLASS_NODATA, "{ix:?} must be masked");
            masked += 1;
        } else if p == CLASS_NODATA {
            // AutoATES writes class 0 as nodata; we keep the 0.
            assert_eq!(q, 0, "{ix:?}");
        } else {
            assert_eq!(q, p, "{ix:?}");
        }
    }
    assert!(masked >= 21_574, "forest nodata alone covers 21574 cells");
}

/// Overhead exposure from the Flow-Py run that produced AutoATES's own
/// `Overhead.tif` and `FP_int16.tif` (Sykes et al. 2023 OSF archive, see
/// `autoates/osf_flowpy/`): every cell must match.
#[test]
fn overhead_and_fp_inputs_match_their_flowpy_run() {
    let cc = read("autoates/osf_flowpy/cell_counts.tif");
    let zd = read("autoates/osf_flowpy/z_delta.tif");
    // max_z 270, from that run's log (`log_20230425_170518.txt`).
    let ours = ates_core::overhead::overhead(&cc.data, &zd.data, 270.0, None).unwrap();
    assert_same("overhead", &ours, "autoates/Overhead.tif");
    // FP_int16.tif is the travel angle truncated to int16.
    let fp = read("autoates/osf_flowpy/FP_travel_angle.tif");
    assert_same(
        "FP_int16",
        &fp.data.mapv(|v| v.trunc() as i16),
        "autoates/FP_int16.tif",
    );
}
