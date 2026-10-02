//! Golden tests for the Flow-Py port: all six outputs must match Flow-Py
//! (AutoATES `FlowPy_detrainment` @ 3afcb49) run on its own test data.
//!
//! Fixtures: `tests/fixtures/flowpy` (see its README for how the reference
//! runs were produced). `ATES_FLOWPY_ORACLE_DIR` can point at another
//! directory with the same layout.
#![cfg(feature = "gdal")]

use std::path::PathBuf;

use ates_core::Grid;
use ates_core::flowpy::{FlowPyLayers, FlowPyParams, flowpy};
use ates_io::gdal_backend::read_grid;

fn dir() -> PathBuf {
    std::env::var_os("ATES_FLOWPY_ORACLE_DIR").map_or_else(
        || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/flowpy"),
        PathBuf::from,
    )
}

fn read(name: &str) -> Grid<f32> {
    let p = dir().join(name);
    read_grid(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

fn check(case: &str, release: &str, forest: Option<&str>, p: FlowPyParams) {
    let dem = read("dem.tif");
    let release = read(release);
    let forest = forest.map(read);
    let out = flowpy(&dem, &release, forest.as_ref(), &p).unwrap();
    let FlowPyLayers {
        z_delta,
        flux,
        cell_counts,
        z_delta_sum,
        fp_travel_angle,
        fp_distance,
    } = out;
    let mut failures = Vec::new();
    for (name, ours) in [
        ("z_delta", z_delta),
        ("flux", flux),
        ("cell_counts", cell_counts),
        ("z_delta_sum", z_delta_sum),
        ("FP_travel_angle", fp_travel_angle),
        ("SL_travel_angle", fp_distance),
    ] {
        let r = read(&format!("{case}/{name}.tif"));
        assert_eq!(ours.data.dim(), r.data.dim(), "{case}/{name}: shape");
        let mut differing = 0;
        let mut max_abs = 0.0_f64;
        let mut first = None;
        for ((ix, &a), &b) in ours.data.indexed_iter().zip(r.data.iter()) {
            if a.to_bits() != b.to_bits() {
                differing += 1;
                max_abs = max_abs.max((f64::from(a) - f64::from(b)).abs());
                first.get_or_insert((ix, a, b));
            }
        }
        let touched = cell_counts_nonzero(&read(&format!("{case}/cell_counts.tif")));
        eprintln!(
            "{case}/{name}: {differing} differing cells (max abs {max_abs:e}); \
             {touched} cells on paths"
        );
        if differing > 0 {
            failures.push(format!("{name}: {differing} differ, first {first:?}"));
        }
    }
    assert!(failures.is_empty(), "{case}: {failures:?}");
}

fn cell_counts_nonzero(g: &Grid<f32>) -> usize {
    g.data.iter().filter(|&&v| v > 0.0).count()
}

/// `main.py`'s own example arguments (alpha 23, exponent 8, flux 0.003,
/// max_z 270, `forest2.tif`), without the infrastructure layer.
const EXAMPLE: FlowPyParams = FlowPyParams {
    alpha_deg: 23.0,
    exponent: 8,
    flux_threshold: 0.003,
    max_z_delta: 270.0,
};

#[test]
fn pra2_with_forest() {
    check("pra2_forest", "pra2.tif", Some("forest2.tif"), EXAMPLE);
}

#[test]
fn pra_with_forest() {
    check("pra_forest", "pra.tif", Some("forest2.tif"), EXAMPLE);
}

/// The GUI defaults (alpha 25, exponent 8, flux 0.003, max_z 8848), no forest.
#[test]
fn pra_without_forest() {
    let p = FlowPyParams {
        alpha_deg: 25.0,
        max_z_delta: 8848.0,
        ..EXAMPLE
    };
    check("pra_noforest", "pra.tif", None, p);
}
