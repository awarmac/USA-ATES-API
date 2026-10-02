//! Golden tests for the PRA port: windshelter, continuous PRA and binary
//! PRA must match the outputs AutoATES ships in its `PRA/` folder.
//!
//! Fixtures: `tests/fixtures/autoates_pra` (see its README). They are a clip
//! of the 801 x 801 originals that keeps the raster's top-left corner. The
//! last `HALO` rows and columns are not compared, because there the clip
//! edge changes the windshelter, the slope and the sieve.
//!
//! The ignored test `pra_matches_autoates_full` runs the same check on the
//! unclipped originals in `$ATES_PRA_ORACLE_DIR`.
#![cfg(feature = "gdal")]

use std::path::{Path, PathBuf};

use ates_core::Grid;
use ates_core::pra::{Cauchy, PraParams, pra};
use ates_io::gdal_backend::read_grid;

/// `PRA/log.txt` @ 3afcb49: stems, radius 6, prob 0.5, winddir 0,
/// windtol 180, pra_thd 0.15, sf 3, and the Cauchy parameters it logs.
const PARAMS: PraParams = PraParams {
    radius_cells: 6,
    prob: 0.5,
    wind_dir_deg: 0.0,
    wind_tol_deg: 180.0,
    threshold: 0.15,
    sieve_cells: 3,
    slope: Cauchy {
        a: 11.0,
        b: 4.0,
        c: 43.0,
    },
    windshelter: Cauchy {
        a: 3.0,
        b: 10.0,
        c: 3.0,
    },
    forest: Cauchy {
        a: 350.0,
        b: 2.0,
        c: -120.0,
    },
};

/// Rows and columns at the bottom and right of a clip that are not compared.
const HALO: usize = 24;

fn read(dir: &Path, name: &str) -> Grid<f32> {
    read_grid(&dir.join(name)).unwrap_or_else(|e| panic!("{name}: {e}"))
}

#[derive(Debug)]
struct Diff {
    differing: usize,
    max_abs: f64,
}

fn diff(ours: impl Iterator<Item = f64>, reference: impl Iterator<Item = f64>) -> Diff {
    let mut d = Diff {
        differing: 0,
        max_abs: 0.0,
    };
    for (a, b) in ours.zip(reference) {
        if a != b {
            d.differing += 1;
            d.max_abs = d.max_abs.max((a - b).abs());
        }
    }
    d
}

fn check(dir: &Path, halo: usize) {
    let dem = read(dir, "DEM.tif");
    let mut forest = read(dir, "FOREST.tif");
    // FOREST.tif is offset from the DEM by (-3.5 m, -2.3 m) at the same
    // size; AutoATES ignores georeferencing and pairs cells by index.
    forest.transform = dem.transform;
    let out = pra(&dem, Some(&forest), &PARAMS).unwrap();

    let (rows, cols) = dem.data.dim();
    let keep = |(r, c): (usize, usize)| r < rows - halo && c < cols - halo;
    let pick_f32 = |g: &Grid<f32>| -> Vec<f64> {
        g.data
            .indexed_iter()
            .filter(|&(ix, _)| keep(ix))
            .map(|(_, &v)| f64::from(v))
            .collect()
    };
    let pick_i16 = |g: &Grid<i16>| -> Vec<f64> {
        g.data
            .indexed_iter()
            .filter(|&(ix, _)| keep(ix))
            .map(|(_, &v)| f64::from(v))
            .collect()
    };
    let compared = (rows - halo) * (cols - halo);
    let ws = diff(
        pick_f32(&out.windshelter).into_iter(),
        pick_f32(&read(dir, "windshelter.tif")).into_iter(),
    );
    let cont = diff(
        pick_i16(&out.continuous).into_iter(),
        pick_f32(&read(dir, "PRA_continous.tif")).into_iter(),
    );
    let bin = diff(
        pick_i16(&out.binary).into_iter(),
        pick_f32(&read(dir, "PRA_binary.tif")).into_iter(),
    );
    let sieved = diff(
        pick_i16(&out.binary).into_iter(),
        pick_i16(&out.binary_unsieved).into_iter(),
    )
    .differing;
    eprintln!(
        "{compared} cells compared; windshelter {ws:?}; continuous {cont:?}; \
         binary {bin:?}; {sieved} cells changed by the sieve"
    );
    assert!(sieved > 0, "the sieve step should be exercised");
    assert_eq!(ws.differing, 0, "windshelter");
    assert_eq!(cont.differing, 0, "continuous");
    assert_eq!(bin.differing, 0, "binary");
}

#[test]
fn pra_matches_autoates_clip() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/autoates_pra");
    check(&dir, HALO);
}

#[test]
#[ignore = "needs the unclipped AutoATES PRA/ rasters in $ATES_PRA_ORACLE_DIR"]
fn pra_matches_autoates_full() {
    let dir = std::env::var_os("ATES_PRA_ORACLE_DIR").expect("set ATES_PRA_ORACLE_DIR");
    check(Path::new(&dir), 0);
}
