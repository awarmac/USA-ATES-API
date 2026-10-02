//! Potential release areas (PRA).
//!
//! A faithful port of AutoATES v2.0 `PRA/PRA_AutoATES-v2.0.py` (@ 3afcb49).
//! That script reimplements the fuzzy-logic PRA model of Veitinger et al.
//! (2016) and Sharp (2018). The steps, with the script's line numbers:
//!
//! | function | step | AutoATES output |
//! |---|---|---|
//! | [`gradient_slope_deg`] | slope from `np.gradient` (lines 201-213) | not written |
//! | [`windshelter`] | windshelter index (lines 95-233) | `windshelter.tif` |
//! | [`memberships`] | Cauchy memberships (lines 241-306) | not written |
//! | [`fuzzy_pra`] | fuzzy operator, thresholds (lines 314-335) | `PRA_continous.tif` |
//! | [`crate::sieve::sieve_filter`] | `gdal.SieveFilter` (lines 338-341) | `PRA_binary.tif` |
//!
//! Numeric types follow the script running on the float32 DEM and forest
//! rasters AutoATES ships:
//! - Slope, memberships and the fuzzy operator run in `f32`.
//! - The windshelter runs in `f64` and is stored as `f32`.
//!
//! Quirks reproduced on purpose:
//! - Slope uses its own `np.gradient` slope, not Horn. Cells below -100
//!   become -9999 first, so nodata produces steep artificial slopes.
//! - The windshelter treats elevation 0 like nodata and works in radians.
//!   Its output is -9999 in a border of `radius` cells.
//! - Forest memberships at or below 1e-5 become 1 (line 302).
//! - In the binary output, a value exactly equal to the threshold keeps its
//!   continuous value instead of becoming 0 or 1.
//!
//! Deviations: upstream behaviour is undefined for NaN inputs, so NaN is
//! treated like -9999. Upstream never loads the forest raster for `bav`
//! and `sen2cc` and fails; here their cited parameters apply.

use ndarray::{Array2, Zip};

use crate::classify::ClassifyError;
use crate::grid::Grid;
use crate::sieve::{Connectedness, sieve_filter};

/// Nodata written into the windshelter and PRA rasters.
pub const PRA_NODATA: f32 = -9999.0;

/// Forest memberships at or below this become 1 (line 302).
const FOREST_MEMBERSHIP_FLOOR: f32 = 0.00001;

/// Generalised bell ("Cauchy") membership `1 / (1 + ((x - c) / a)^(2b))`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Cauchy {
    pub a: f32,
    pub b: f32,
    pub c: f32,
}

impl Cauchy {
    /// Evaluated in `f32`, like numpy on a float32 array.
    pub fn eval(&self, x: f32) -> f32 {
        1.0 / (1.0 + ((x - self.c) / self.a).powf(2.0 * self.b))
    }
}

/// PRA model parameters (AutoATES names in brackets).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PraParams {
    /// Windshelter radius in cells [`radius`].
    pub radius_cells: usize,
    /// Quantile of the windshelter angles [`prob`].
    pub prob: f64,
    /// Prevailing wind direction in degrees [`winddir`].
    pub wind_dir_deg: f64,
    /// Degrees either side of the wind direction [`windtol`].
    pub wind_tol_deg: f64,
    /// Cut-off for the binary PRA, 0-1 [`pra_thd`].
    pub threshold: f64,
    /// Release areas with at most this many cells are removed [`sf`].
    pub sieve_cells: usize,
    pub slope: Cauchy,
    pub windshelter: Cauchy,
    /// Forest membership for the forest type in use. Without a forest
    /// raster AutoATES uses the `pcc` function on a forest of 0.
    pub forest: Cauchy,
}

impl PraParams {
    pub fn validate(&self) -> Result<(), ClassifyError> {
        let bad = |m: &str| Err(ClassifyError::BadParam(m.into()));
        if self.radius_cells == 0 {
            return bad("windshelter radius must be at least one cell");
        }
        if !(0.0..=1.0).contains(&self.prob) {
            return bad("windshelter prob must be in [0, 1]");
        }
        if !(0.0..=1.0).contains(&self.threshold) {
            return bad("PRA threshold must be in [0, 1]");
        }
        if [self.slope, self.windshelter, self.forest]
            .iter()
            .any(|f| f.a == 0.0 || !(f.a.is_finite() && f.b.is_finite() && f.c.is_finite()))
        {
            return bad("Cauchy parameters must be finite with a != 0");
        }
        Ok(())
    }
}

/// Outputs of [`pra`], all on the DEM grid.
#[derive(Debug, Clone)]
pub struct PraLayers {
    /// Windshelter index in radians; -9999 at the border and where undefined.
    pub windshelter: Grid<f32>,
    /// PRA likelihood 0-100 (`PRA_continous.tif`).
    pub continuous: Grid<i16>,
    /// Release areas before the sieve (AutoATES overwrites this in place).
    pub binary_unsieved: Grid<i16>,
    /// Release areas, 1 = release, after the sieve (`PRA_binary.tif`).
    pub binary: Grid<i16>,
}

/// numpy `np.round(x, 5)` on float32: `rint(x * 1e5) / 1e5` in `f32`.
fn round5(x: f32) -> f32 {
    (x * 100_000.0).round_ties_even() / 100_000.0
}

/// Slope in degrees from `np.gradient(dem, cell_size)` (lines 201-213).
///
/// Values below -100 (and NaN) become -9999 first. Interior cells use
/// central differences and edges one-sided ones, all in `f32`.
pub fn gradient_slope_deg(dem: &Array2<f32>, cell_size: f64) -> Result<Array2<f32>, ClassifyError> {
    let (rows, cols) = dem.dim();
    if rows < 2 || cols < 2 {
        return Err(ClassifyError::BadParam(
            "np.gradient needs at least 2 rows and 2 columns".into(),
        ));
    }
    let z = dem.mapv(|v| if v < -100.0 || v.is_nan() { -9999.0 } else { v });
    // `2. * dx` and `dx` are Python floats, applied to a float32 array.
    let (two_dx, dx) = ((2.0 * cell_size) as f32, cell_size as f32);
    let grad = |a: f32, b: f32, d: f32| (a - b) / d;
    let d_axis = |n: usize, i: usize, get: &dyn Fn(usize) -> f32| -> f32 {
        if i == 0 {
            grad(get(1), get(0), dx)
        } else if i == n - 1 {
            grad(get(n - 1), get(n - 2), dx)
        } else {
            grad(get(i + 1), get(i - 1), two_dx)
        }
    };
    // numpy `degrees` on float32 multiplies by 180.0f / NPY_PIf.
    const RAD2DEG: f32 = 180.0 / std::f32::consts::PI;
    Ok(Array2::from_shape_fn((rows, cols), |(r, c)| {
        let px = d_axis(rows, r, &|i| z[[i, c]]);
        let py = d_axis(cols, c, &|j| z[[r, j]]);
        (px * px + py * py).sqrt().atan() * RAD2DEG
    }))
}

/// The sector of cells around the centre that the windshelter uses
/// (`windshelter_prep` and `sector_mask`, lines 119-160), with distances
/// in map units. Returns (row offset, col offset, distance) for every
/// cell in the sector except the centre.
fn windshelter_sector(p: &PraParams, cell_size: f64) -> Vec<(usize, usize, f64)> {
    let r = p.radius_cells;
    // AutoATES passes (winddir - windtol + 270, winddir + windtol + 270).
    let tmin = (p.wind_dir_deg - p.wind_tol_deg + 270.0).to_radians();
    let mut tmax = (p.wind_dir_deg + p.wind_tol_deg + 270.0).to_radians();
    if tmax < tmin {
        tmax += 2.0 * std::f64::consts::PI;
    }
    let span = tmax - tmin;
    let two_pi = 2.0 * std::f64::consts::PI;
    // numpy `%` (np.remainder): the result takes the divisor's sign.
    let remainder = |a: f64| {
        let m = a % two_pi;
        if m < 0.0 { m + two_pi } else { m }
    };
    let (ri, rr) = (r as i64, (r * r) as i64);
    let mut cells = Vec::new();
    for x in 0..=2 * r {
        for y in 0..=2 * r {
            let (dx, dy) = (x as i64 - ri, y as i64 - ri);
            if dx == 0 && dy == 0 {
                continue; // the centre is in the mask but excluded later
            }
            let theta = remainder((dx as f64).atan2(dy as f64) - tmin);
            if dx * dx + dy * dy <= rr && theta <= span {
                let dist = ((dx * dx + dy * dy) as f64).sqrt() * cell_size;
                cells.push((x, y, dist));
            }
        }
    }
    cells
}

/// numpy `nanquantile(values, q)` with the default linear method, on
/// values that are already NaN-free. Sorts `v` in place.
fn quantile_linear(v: &mut [f64], q: f64) -> f64 {
    let n = v.len();
    if n == 0 {
        return f64::NAN;
    }
    v.sort_unstable_by(f64::total_cmp);
    let virt = (n - 1) as f64 * q;
    if virt >= (n - 1) as f64 {
        return v[n - 1];
    }
    if virt < 0.0 {
        return v[0];
    }
    let lo = virt.floor();
    let (a, b, t) = (v[lo as usize], v[lo as usize + 1], virt - lo);
    // numpy `_lerp`.
    let d = b - a;
    if t >= 0.5 {
        b - d * (1.0 - t)
    } else {
        a + d * t
    }
}

/// Windshelter index (lines 162-233): for each cell, the `prob` quantile
/// of `atan((z - z_centre) / distance)` over the sector within `radius`.
///
/// Cells equal to `nodata` or to 0 are ignored, and an ignored centre gives
/// -9999. The result is -9999 within `radius` cells of the edge.
pub fn windshelter(
    dem: &Array2<f32>,
    nodata: Option<f64>,
    cell_size: f64,
    p: &PraParams,
) -> Array2<f32> {
    let (rows, cols) = dem.dim();
    let r = p.radius_cells;
    let n = 2 * r + 1;
    let mut out = Array2::from_elem((rows, cols), PRA_NODATA);
    if rows < n || cols < n {
        return out;
    }
    let z = dem.mapv(|v| {
        let v = f64::from(v);
        if Some(v) == nodata || v == 0.0 {
            f64::NAN
        } else {
            v
        }
    });
    let sector = windshelter_sector(p, cell_size);
    let mut buf = Vec::with_capacity(sector.len());
    for i in 0..=rows - n {
        for j in 0..=cols - n {
            let centre = z[[i + r, j + r]];
            let q = if centre.is_nan() {
                f64::NAN
            } else {
                buf.clear();
                buf.extend(sector.iter().filter_map(|&(u, v, d)| {
                    let x = z[[i + u, j + v]];
                    (!x.is_nan()).then(|| ((x - centre) / d).atan())
                }));
                quantile_linear(&mut buf, p.prob)
            };
            out[[i + r, j + r]] = if q.is_nan() { PRA_NODATA } else { q as f32 };
        }
    }
    out
}

/// The three memberships, each rounded to 5 decimals (lines 241-306).
/// `forest` is the forest raster, or for "no forest" the values AutoATES
/// substitutes (0 where the DEM is above -100, the DEM value elsewhere).
pub fn memberships(
    slope_deg: &Array2<f32>,
    windshelter: &Array2<f32>,
    forest: &Array2<f32>,
    p: &PraParams,
) -> (Array2<f32>, Array2<f32>, Array2<f32>) {
    let slope = slope_deg.mapv(|s| round5(p.slope.eval(s)));
    let ws = windshelter.mapv(|w| round5(p.windshelter.eval(w)));
    let forest = forest.mapv(|f| {
        let m = p.forest.eval(f);
        // `<=` is false for NaN; NaN forest is treated like nodata (-> 1).
        round5(if m <= FOREST_MEMBERSHIP_FLOOR || m.is_nan() {
            1.0
        } else {
            m
        })
    });
    (slope, ws, forest)
}

/// Fuzzy operator and binary threshold (lines 314-335). Returns
/// (continuous 0-100, binary before the sieve).
pub fn fuzzy_pra(
    slope_c: &Array2<f32>,
    ws_c: &Array2<f32>,
    forest_c: &Array2<f32>,
    threshold: f64,
) -> (Array2<i16>, Array2<i16>) {
    // `pra_thd * 100` is a Python float, compared with a float32 array.
    let thd = (threshold * 100.0) as f32;
    let mut cont = Array2::zeros(slope_c.dim());
    let mut bin = Array2::zeros(slope_c.dim());
    Zip::from(&mut cont)
        .and(&mut bin)
        .and(slope_c)
        .and(ws_c)
        .and(forest_c)
        .for_each(|cont, bin, &s, &w, &f| {
            // np.minimum, evaluated pairwise as upstream.
            let m = s.min(w).min(f);
            let pra = round5((1.0 - m) * m + m * (s + w + f) / 3.0) * 100.0;
            // `astype('int16')` truncates toward zero.
            *cont = pra as i16;
            let b = if (0.0..thd).contains(&pra) {
                0.0
            } else if thd < pra && pra <= 100.0 {
                1.0
            } else {
                pra
            };
            *bin = b as i16;
        });
    (cont, bin)
}

/// The full PRA model on a DEM and an optional forest raster on the same
/// grid. Without a forest raster this is AutoATES's `no_forest` mode.
pub fn pra(
    dem: &Grid<f32>,
    forest: Option<&Grid<f32>>,
    p: &PraParams,
) -> Result<PraLayers, ClassifyError> {
    p.validate()?;
    if let Some(f) = forest
        && (f.data.dim() != dem.data.dim() || f.transform != dem.transform)
    {
        return Err(ClassifyError::Misaligned("forest"));
    }
    let cell = dem.transform.ew_res();
    if cell != dem.transform.ns_res() {
        return Err(ClassifyError::BadParam(format!(
            "PRA needs square cells (got {cell} x {} m)",
            dem.transform.ns_res()
        )));
    }
    let slope = gradient_slope_deg(&dem.data, cell)?;
    let ws = windshelter(&dem.data, dem.nodata, cell, p);
    let forest_values = match forest {
        Some(f) => f.data.clone(),
        None => dem.data.mapv(|z| if z > -100.0 { 0.0 } else { z }),
    };
    let (sc, wc, fc) = memberships(&slope, &ws, &forest_values, p);
    let (continuous, unsieved) = fuzzy_pra(&sc, &wc, &fc, p.threshold);
    // AutoATES calls SieveFilter with threshold sf + 1, 8-connected.
    let binary = sieve_filter(&unsieved, None, p.sieve_cells + 1, Connectedness::Eight);

    Ok(PraLayers {
        windshelter: on_grid(dem, ws),
        continuous: on_grid(dem, continuous),
        binary_unsieved: on_grid(dem, unsieved),
        binary: on_grid(dem, binary),
    })
}

fn on_grid<T>(dem: &Grid<f32>, data: Array2<T>) -> Grid<T> {
    Grid {
        data,
        transform: dem.transform,
        crs: dem.crs.clone(),
        nodata: Some(f64::from(PRA_NODATA)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Crs, GeoTransform};
    use ndarray::array;

    /// PRA_AutoATES-v2.0.py defaults (log.txt, `stems` run).
    fn params() -> PraParams {
        PraParams {
            radius_cells: 1,
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
        }
    }

    #[test]
    fn gradient_matches_numpy() {
        // np.gradient([[0,10,30],[0,10,30]], 10) -> d/dcol = [1, 1.5, 2].
        let z = array![[0.0_f32, 10.0, 30.0], [0.0, 10.0, 30.0]];
        let s = gradient_slope_deg(&z, 10.0).unwrap();
        let deg = |t: f32| t.atan() * (180.0 / std::f32::consts::PI);
        assert_eq!(s.row(0).to_vec(), vec![deg(1.0), deg(1.5), deg(2.0)]);
        assert!(gradient_slope_deg(&array![[1.0_f32, 2.0]], 10.0).is_err());
    }

    #[test]
    fn full_circle_sector_is_a_disc() {
        let mut p = params();
        p.radius_cells = 2;
        // Radius 2 disc: 13 cells, minus the centre.
        assert_eq!(windshelter_sector(&p, 10.0).len(), 12);
        // A 90 degree sector keeps fewer cells.
        p.wind_tol_deg = 45.0;
        assert!(windshelter_sector(&p, 10.0).len() < 12);
    }

    #[test]
    fn quantile_matches_numpy_linear() {
        assert_eq!(quantile_linear(&mut [3.0, 1.0, 2.0], 0.5), 2.0);
        assert_eq!(quantile_linear(&mut [4.0, 1.0, 2.0, 3.0], 0.5), 2.5);
        assert_eq!(quantile_linear(&mut [7.0], 0.5), 7.0);
        assert!(quantile_linear(&mut [], 0.5).is_nan());
    }

    #[test]
    fn windshelter_ignores_zero_and_nodata() {
        let p = params();
        // A pit: the centre is 10 m below every neighbour 10 m away.
        let mut z = Array2::from_elem((3, 3), 110.0_f32);
        z[[1, 1]] = 100.0;
        let ws = windshelter(&z, Some(-9999.0), 10.0, &p);
        // A radius-1 disc holds only the 4 edge neighbours (diagonals have
        // r^2 = 2 > 1), each 10 m up over 10 m: atan(1).
        let expect = std::f64::consts::FRAC_PI_4 as f32;
        assert_eq!(ws[[1, 1]], expect);
        assert_eq!(ws[[0, 0]], PRA_NODATA, "border");
        z[[1, 1]] = 0.0;
        assert_eq!(windshelter(&z, Some(-9999.0), 10.0, &p)[[1, 1]], PRA_NODATA);
    }

    #[test]
    fn binary_keeps_values_equal_to_the_threshold() {
        let one = Array2::from_elem((1, 1), 1.0_f32);
        let m = |v: f32| Array2::from_elem((1, 1), v);
        // All memberships 1 -> PRA 100 -> binary 1.
        let (c, b) = fuzzy_pra(&one, &one, &one, 0.15);
        assert_eq!((c[[0, 0]], b[[0, 0]]), (100, 1));
        // All 0 -> 0.
        let (c, b) = fuzzy_pra(&m(0.0), &m(0.0), &m(0.0), 0.15);
        assert_eq!((c[[0, 0]], b[[0, 0]]), (0, 0));
        // m = 0.1, rest 1: (0.9 * 0.1) + 0.1 * 2.1 / 3 = 0.16 -> 16 -> 1.
        let (c, b) = fuzzy_pra(&m(0.1), &one, &one, 0.15);
        assert_eq!((c[[0, 0]], b[[0, 0]]), (16, 1));
        // With threshold 0.16 the value 16 is neither below nor above.
        let (_, b) = fuzzy_pra(&m(0.1), &one, &one, 0.16);
        assert_eq!(b[[0, 0]], 16);
    }

    #[test]
    fn forest_floor_turns_tiny_memberships_into_one() {
        let p = params();
        let (_, _, f) = memberships(&m1(30.0), &m1(0.0), &m1(-9999.0), &p);
        assert_eq!(f[[0, 0]], 1.0);
        let (_, _, f) = memberships(&m1(30.0), &m1(0.0), &m1(0.0), &p);
        assert!(f[[0, 0]] < 1.0 && f[[0, 0]] > 0.9);
    }

    fn m1(v: f32) -> Array2<f32> {
        Array2::from_elem((1, 1), v)
    }

    #[test]
    fn pra_checks_forest_alignment_and_square_cells() {
        let gt = GeoTransform::north_up(0.0, 50.0, 10.0, 10.0);
        let grid = |gt| {
            Grid::new(
                Array2::from_elem((5, 5), 100.0_f32),
                gt,
                Crs::Epsg(32613),
                None,
            )
            .unwrap()
        };
        let dem = grid(gt);
        let shifted = grid(GeoTransform::north_up(5.0, 50.0, 10.0, 10.0));
        assert_eq!(
            pra(&dem, Some(&shifted), &params()).unwrap_err(),
            ClassifyError::Misaligned("forest")
        );
        let rect = grid(GeoTransform::north_up(0.0, 50.0, 10.0, 20.0));
        assert!(matches!(
            pra(&rect, None, &params()),
            Err(ClassifyError::BadParam(_))
        ));
        let out = pra(&dem, None, &params()).unwrap();
        // Flat terrain: no release anywhere.
        assert!(out.binary.data.iter().all(|&v| v == 0));
        assert_eq!(out.continuous.data.dim(), (5, 5));
    }
}
