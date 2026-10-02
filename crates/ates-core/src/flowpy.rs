//! Avalanche runout: a port of Flow-Py with forest detrainment.
//!
//! A faithful port of AutoATES v2.0 `FlowPy_detrainment` (@ 3afcb49):
//! `flow_class.py` (the `Cell` routing) and `flow_core.py`
//! (`calculation_effect`, the run without infrastructure). Flow-Py was
//! written by Neuhauser et al. (BFW); AutoATES added forest friction and
//! detrainment.
//!
//! **Model.** Each release cell starts a path with flux 1:
//! - The flux spreads to downhill neighbours, weighted by slope (Holmgren
//!   exponent) and flow persistence.
//! - It stops where the energy line, measured with the alpha angle, meets
//!   the terrain.
//! - Forest adds friction to the alpha angle and detrains flux.
//!
//! Per cell, the outputs are:
//! - the maximum energy-line height `z_delta`;
//! - the maximum flux;
//! - the number of paths (`cell_counts`);
//! - the sum of `z_delta`;
//! - the maximum flow-path travel angle;
//! - the minimum flow-path distance. Upstream writes this as
//!   `SL_travel_angle.tif`.
//!
//! **Fidelity.** The port reproduces the Python's arithmetic exactly on a
//! float32 DEM (numpy 2 type promotion):
//! - Elevation differences and persistence are `f32`; everything else is
//!   `f64`.
//! - Output rasters are `f32`, the DEM's type.
//! - numpy's pairwise summation is reproduced (`np_sum`), as are Python's
//!   `max`/`min` tie rules.
//! - Paths are independent, because without infrastructure no release
//!   cell is removed. They run in parallel and are folded into the outputs
//!   in upstream's order, highest release cell first, so even the `f32`
//!   sums match.
//!
//! **Quirks kept on purpose:**
//! - A cell already processed in a path can be added to it again. It then
//!   counts again in `cell_counts`.
//! - Cells with nodata, or the raster edge, in their 3x3 neighbourhood are
//!   never entered.
//! - A forest value of nodata (very negative) makes detrainment huge
//!   once `z_delta` exceeds about 65 m, cutting flux to the 0.0003 floor.
//! - Distances above 10000 m are capped at 10000, the initial value of
//!   upstream's combined array.
//!
//! **Not ported:**
//! - the infrastructure back-calculation (`calculation`);
//! - `sl_gamma`, which no output uses.
//!
//! **Deviation:** NaN elevations count as nodata.

use std::collections::HashMap;

use ndarray::Array2;
use rayon::prelude::*;

use crate::classify::ClassifyError;
use crate::grid::Grid;

/// Forest friction and detrainment constants from `flow_class.py`
/// lines 58-63 (AutoATES additions to Flow-Py).
const MAX_ADDED_FRICTION_FOREST: f32 = 10.0;
const MIN_ADDED_FRICTION_FOREST: f32 = 2.0;
/// Velocity (m/s) beyond which forest friction and detrainment stop changing.
const NO_EFFECT_V: f64 = 30.0;
const MAX_ADDED_DETRAINMENT_FOREST: f32 = 0.0003;
const MIN_ADDED_DETRAINMENT_FOREST: f32 = 0.00001;
/// Minimum flux a cell keeps after detrainment (`flow_class.py` line 232).
const MIN_FLUX: f64 = 0.0003;
/// Initial value of the distance output (`main.py`), which caps it.
const DISTANCE_CAP: f32 = 10000.0;

/// Flow-Py model parameters.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FlowPyParams {
    /// Runout (alpha) angle in degrees.
    pub alpha_deg: f64,
    /// Holmgren exponent controlling lateral spread.
    pub exponent: i32,
    /// Flux below which a neighbour receives nothing.
    pub flux_threshold: f64,
    /// Maximum energy-line height (m).
    pub max_z_delta: f64,
}

impl FlowPyParams {
    pub fn validate(&self) -> Result<(), ClassifyError> {
        let bad = |m: &str| Err(ClassifyError::BadParam(m.into()));
        if !(self.alpha_deg > 0.0 && self.alpha_deg < 90.0) {
            return bad("alpha must be in (0, 90) degrees");
        }
        if self.exponent < 1 {
            return bad("exponent must be at least 1");
        }
        if !(self.flux_threshold > 0.0 && self.flux_threshold.is_finite()) {
            return bad("flux threshold must be positive");
        }
        if self.max_z_delta.is_nan() || self.max_z_delta <= 0.0 {
            return bad("max z_delta must be positive");
        }
        Ok(())
    }
}

/// Flow-Py outputs on the DEM grid, as upstream writes them (`f32`).
#[derive(Debug, Clone)]
pub struct FlowPyLayers {
    pub z_delta: Grid<f32>,
    pub flux: Grid<f32>,
    pub cell_counts: Grid<f32>,
    pub z_delta_sum: Grid<f32>,
    /// Maximum flow-path travel angle (degrees); AutoATES's `FP` input.
    pub fp_travel_angle: Grid<f32>,
    /// Minimum flow-path distance (m); upstream file `SL_travel_angle.tif`.
    pub fp_distance: Grid<f32>,
}

/// numpy `np.sum` of a short contiguous `f64` array: a plain loop below 8
/// elements, otherwise 8 pairwise accumulators plus the remainder.
fn np_sum(a: &[f64]) -> f64 {
    if a.len() < 8 {
        return a.iter().fold(0.0, |s, &x| s + x);
    }
    let mut r = [0.0; 8];
    r.copy_from_slice(&a[..8]);
    let full = a.len() - a.len() % 8;
    for chunk in a[8..full].chunks_exact(8) {
        for (acc, &x) in r.iter_mut().zip(chunk) {
            *acc += x;
        }
    }
    let mut res = ((r[0] + r[1]) + (r[2] + r[3])) + ((r[4] + r[5]) + (r[6] + r[7]));
    for &x in &a[full..] {
        res += x;
    }
    res
}

/// Python's `max(a, b)`: `b` only if it is strictly greater.
fn py_max(a: f64, b: f64) -> f64 {
    if b > a { b } else { a }
}

/// Distance factors to the 3x3 neighbours, row-major.
const SQ2: f64 = std::f64::consts::SQRT_2;
const DS_ALPHA: [f64; 9] = [SQ2, 1.0, SQ2, 1.0, 0.0, 1.0, SQ2, 1.0, SQ2];
const DS_BETA: [f64; 9] = [SQ2, 1.0, SQ2, 1.0, 1.0, 1.0, SQ2, 1.0, SQ2];

/// Result of `Cell.calc_distribution`: the neighbours that receive flux,
/// as (row, col, flux, z_delta) in row-major order, and the cell's new
/// flux, flow-path distance and travel angle.
struct Distribution {
    targets: Vec<(usize, usize, f64, f64)>,
    flux: f64,
    min_distance: f64,
    max_gamma: f64,
}

/// One `Cell` of a path.
struct Cell {
    row: usize,
    col: usize,
    altitude: f32,
    dem_ng: [f32; 9],
    forest: f32,
    flux: f64,
    z_delta: f64,
    is_start: bool,
    /// Indices of parent cells in the path's cell list.
    parents: Vec<usize>,
    min_distance: f64,
    max_gamma: f64,
}

/// What a processed cell contributes to the outputs.
#[derive(Clone, Copy)]
struct Record {
    row: usize,
    col: usize,
    z_delta: f64,
    flux: f64,
    max_gamma: f64,
    min_distance: f64,
}

struct Model<'a> {
    dem: &'a Array2<f32>,
    forest: &'a Array2<f32>,
    nodata: Option<f32>,
    cellsize: f64,
    p: FlowPyParams,
}

impl Model<'_> {
    /// The 3x3 neighbourhood, or `None` where upstream skips the cell:
    /// at the raster edge or with nodata inside.
    fn neighbourhood(&self, r: usize, c: usize) -> Option<[f32; 9]> {
        let (rows, cols) = self.dem.dim();
        if r == 0 || c == 0 || r + 1 >= rows || c + 1 >= cols {
            return None;
        }
        let mut ng = [0.0; 9];
        for (i, v) in ng.iter_mut().enumerate() {
            *v = self.dem[[r + i / 3 - 1, c + i % 3 - 1]];
            if v.is_nan() || Some(*v) == self.nodata {
                return None;
            }
        }
        Some(ng)
    }

    fn new_cell(&self, row: usize, col: usize, dem_ng: [f32; 9], flux: f64, z_delta: f64) -> Cell {
        Cell {
            row,
            col,
            altitude: dem_ng[4],
            dem_ng,
            forest: self.forest[[row, col]],
            flux,
            z_delta,
            is_start: false,
            parents: Vec::new(),
            min_distance: 0.0,
            max_gamma: 0.0,
        }
    }

    /// `Cell.calc_distribution` for `cells[idx]`. The caller stores the
    /// returned flux, distance and travel angle on the cell.
    fn distribute(&self, cells: &[Cell], idx: usize, start_alt: f32) -> Distribution {
        let cell = &cells[idx];
        let p = &self.p;
        let nfz = NO_EFFECT_V * NO_EFFECT_V / (SQ2 * 9.8);

        // calc_z_delta
        let alpha_calc = if cell.forest > 0.0 {
            if cell.z_delta < nfz {
                let rest = MAX_ADDED_FRICTION_FOREST * cell.forest;
                let slope = f64::from(rest - MIN_ADDED_FRICTION_FOREST) / (0.0 - nfz);
                let friction = py_max(
                    f64::from(MIN_ADDED_FRICTION_FOREST),
                    slope * cell.z_delta + f64::from(rest),
                );
                p.alpha_deg + py_max(0.0, friction)
            } else {
                p.alpha_deg + f64::from(MIN_ADDED_FRICTION_FOREST)
            }
        } else {
            p.alpha_deg
        };
        let tan_alpha = alpha_calc.to_radians().tan();
        let mut zdn = [0.0_f64; 9];
        for i in 0..9 {
            let z_gamma = cell.altitude - cell.dem_ng[i];
            let z_alpha = DS_ALPHA[i] * self.cellsize * tan_alpha;
            let mut v = (cell.z_delta + f64::from(z_gamma)) - z_alpha;
            if v < 0.0 {
                v = 0.0;
            }
            if v > p.max_z_delta {
                v = p.max_z_delta;
            }
            zdn[i] = v;
        }

        // calc_persistence, then `persistence *= no_flow`
        let mut pers = [0.0_f32; 9];
        let mut no_flow = [1.0_f32; 9];
        if cell.is_start || cells[cell.parents[0]].is_start {
            pers = [1.0; 9];
        } else {
            let add = |pers: &mut [f32; 9], i: usize, w: f64| {
                pers[i] = (f64::from(pers[i]) + w) as f32;
            };
            for &pi in &cell.parents {
                let parent = &cells[pi];
                let dx = parent.col as i64 - cell.col as i64;
                let dy = parent.row as i64 - cell.row as i64;
                no_flow[((dy + 1) * 3 + dx + 1) as usize] = 0.0;
                let w = parent.z_delta;
                let w7 = 0.707 * w;
                // [main, side, side] targets in row-major indices.
                let targets: Option<[usize; 3]> = match (dx, dy) {
                    (-1, -1) => Some([8, 7, 5]),
                    (-1, 0) => Some([5, 8, 2]),
                    (-1, 1) => Some([2, 1, 5]),
                    (0, -1) => Some([7, 6, 8]),
                    (0, 1) => Some([1, 0, 2]),
                    (1, -1) => Some([6, 3, 7]),
                    (1, 0) => Some([3, 0, 6]),
                    (1, 1) => Some([0, 1, 3]),
                    _ => None,
                };
                if let Some([m, s1, s2]) = targets {
                    add(&mut pers, m, w);
                    add(&mut pers, s1, w7);
                    add(&mut pers, s2, w7);
                }
            }
        }
        for i in 0..9 {
            pers[i] *= no_flow[i];
        }

        // calc_tanbeta
        let half_pi = 90.0_f64.to_radians();
        let mut tb = [0.0_f64; 9];
        for i in 0..9 {
            let d = DS_BETA[i] * self.cellsize;
            let beta = (f64::from(cell.altitude - cell.dem_ng[i]) / d).atan() + half_pi;
            tb[i] = (beta / 2.0).tan();
            if zdn[i] <= 0.0 || pers[i] <= 0.0 {
                tb[i] = 0.0;
            }
        }
        tb[4] = 0.0;
        let mut r_t = [0.0_f64; 9];
        if np_sum(&tb).abs() > 0.0 {
            let e = f64::from(p.exponent);
            let pow: Vec<f64> = tb.iter().map(|t| t.powf(e)).collect();
            let s = np_sum(&pow);
            for i in 0..9 {
                r_t[i] = pow[i] / s;
            }
        }

        // forest_detrainment
        // `0.0003 * forest` and `rest - 0.00001` stay float32 (Python
        // floats are weak), but `max(0.00001, ...)` compares Python floats.
        let rest = MAX_ADDED_DETRAINMENT_FOREST * cell.forest;
        let slope = f64::from(rest - MIN_ADDED_DETRAINMENT_FOREST) / (0.0 - nfz);
        let detrainment = py_max(0.00001, slope * cell.z_delta + f64::from(rest));

        // calc_fp_travelangle (not for the start cell)
        let (mut min_distance, mut max_gamma) = (cell.min_distance, cell.max_gamma);
        if !cell.is_start {
            let dh = start_alt - cell.altitude;
            let mut dmin = f64::INFINITY;
            for &pi in &cell.parents {
                let parent = &cells[pi];
                let dx = parent.col.abs_diff(cell.col) as f64;
                let dy = parent.row.abs_diff(cell.row) as f64;
                let d = (dx * dx + dy * dy).sqrt() * self.cellsize + parent.min_distance;
                if d < dmin {
                    dmin = d;
                }
            }
            min_distance = dmin;
            max_gamma = (f64::from(dh) / min_distance).atan().to_degrees();
        }

        let flux = py_max(MIN_FLUX, cell.flux - detrainment);
        let thr = p.flux_threshold;
        let mut dist = [0.0_f64; 9];
        if np_sum(&r_t) > 0.0 {
            let pr: Vec<f64> = (0..9).map(|i| f64::from(pers[i]) * r_t[i]).collect();
            let s = np_sum(&pr);
            for i in 0..9 {
                dist[i] = pr[i] / s * flux;
            }
        }
        let count = dist.iter().filter(|&&d| 0.0 < d && d < thr).count();
        let below: Vec<f64> = dist.iter().copied().filter(|&d| d < thr).collect();
        let mass = np_sum(&below);
        if mass > 0.0 && count > 0 {
            let add = mass / count as f64;
            for d in dist.iter_mut().filter(|d| **d > thr) {
                *d += add;
            }
            for d in dist.iter_mut().filter(|d| **d < thr) {
                *d = 0.0;
            }
        }
        if np_sum(&dist) < flux && count > 0 {
            let add = (flux - np_sum(&dist)) / count as f64;
            for d in dist.iter_mut().filter(|d| **d > thr) {
                *d += add;
            }
        }
        let out = (0..9)
            .filter(|&i| dist[i] > thr)
            .map(|i| (cell.row + i / 3 - 1, cell.col + i % 3 - 1, dist[i], zdn[i]))
            .collect();
        Distribution {
            targets: out,
            flux,
            min_distance,
            max_gamma,
        }
    }

    /// One path from a release cell (the body of `calculation_effect`'s
    /// loop). Returns the records in cell-list order.
    fn path(&self, row: usize, col: usize) -> Vec<Record> {
        let Some(ng) = self.neighbourhood(row, col) else {
            return Vec::new();
        };
        let mut start = self.new_cell(row, col, ng, 1.0, 0.0);
        start.is_start = true;
        let start_alt = start.altitude;
        let mut cells = vec![start];
        // Location -> indices in `cells`, ascending.
        let mut at: HashMap<(usize, usize), Vec<usize>> = HashMap::new();
        at.entry((row, col)).or_default().push(0);

        let mut idx = 0;
        while idx < cells.len() {
            let Distribution {
                targets: mut out,
                flux,
                min_distance: min_d,
                max_gamma: max_g,
            } = self.distribute(&cells, idx, start_alt);
            {
                let c = &mut cells[idx];
                c.flux = flux;
                c.min_distance = min_d;
                c.max_gamma = max_g;
            }
            // Upstream sorts by (z_delta, flux, row, col), ascending.
            out.sort_by(|a, b| {
                (a.3, a.2, a.0, a.1)
                    .partial_cmp(&(b.3, b.2, b.0, b.1))
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            // Neighbours already in the list (at or after idx) take the flux.
            let n_before = cells.len();
            let mut fresh = Vec::with_capacity(out.len());
            for (r, c, f, z) in out {
                let existing = at.get(&(r, c)).and_then(|v| {
                    let k = v.partition_point(|&i| i < idx);
                    v.get(k).copied().filter(|&i| i < n_before)
                });
                if let Some(i) = existing {
                    let t = &mut cells[i];
                    t.flux += f;
                    t.parents.push(idx);
                    if z > t.z_delta {
                        t.z_delta = z;
                    }
                } else {
                    fresh.push((r, c, f, z));
                }
            }
            for (r, c, f, z) in fresh {
                let Some(ng) = self.neighbourhood(r, c) else {
                    continue;
                };
                let mut child = self.new_cell(r, c, ng, f, z);
                child.parents.push(idx);
                at.entry((r, c)).or_default().push(cells.len());
                cells.push(child);
            }
            idx += 1;
        }
        cells
            .iter()
            .map(|c| Record {
                row: c.row,
                col: c.col,
                z_delta: c.z_delta,
                flux: c.flux,
                max_gamma: c.max_gamma,
                min_distance: c.min_distance,
            })
            .collect()
    }
}

/// Release cells in upstream order (`get_start_idx`): altitude descending,
/// then row and column descending.
fn start_cells(dem: &Array2<f32>, release: &Grid<f32>) -> Vec<(usize, usize)> {
    // split_release: nodata -> 0 (when nodata is non-zero); only > 0 counts.
    let nd = release.nodata.filter(|&n| n != 0.0);
    let mut starts: Vec<(f32, usize, usize)> = release
        .data
        .indexed_iter()
        .filter(|&(_, &v)| v > 0.0 && Some(f64::from(v)) != nd)
        .map(|((r, c), _)| (dem[[r, c]], r, c))
        .collect();
    starts.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
    starts.into_iter().map(|(_, r, c)| (r, c)).collect()
}

/// Run Flow-Py on `dem` from the release cells of `release` (> 0). Without
/// `forest`, the forest is 0 everywhere, as upstream does.
pub fn flowpy(
    dem: &Grid<f32>,
    release: &Grid<f32>,
    forest: Option<&Grid<f32>>,
    p: &FlowPyParams,
) -> Result<FlowPyLayers, ClassifyError> {
    p.validate()?;
    for (g, name) in [(Some(release), "release"), (forest, "forest")] {
        if let Some(g) = g
            && (g.data.dim() != dem.data.dim() || g.transform != dem.transform)
        {
            return Err(ClassifyError::Misaligned(name));
        }
    }
    let cellsize = dem.transform.ew_res();
    if cellsize != dem.transform.ns_res() {
        return Err(ClassifyError::BadParam(format!(
            "Flow-Py needs square cells (got {cellsize} x {} m)",
            dem.transform.ns_res()
        )));
    }
    let zeros;
    let forest = match forest {
        Some(f) => &f.data,
        None => {
            zeros = Array2::zeros(dem.data.dim());
            &zeros
        }
    };
    let model = Model {
        dem: &dem.data,
        forest,
        nodata: dem.nodata.map(|v| v as f32),
        cellsize,
        p: *p,
    };

    let dim = dem.data.dim();
    let mut z_delta = Array2::<f32>::zeros(dim);
    let mut flux = Array2::<f32>::zeros(dim);
    let mut counts = Array2::<f32>::zeros(dim);
    let mut z_sum = Array2::<f32>::zeros(dim);
    let mut fp_ta = Array2::<f32>::zeros(dim);
    let mut fp_dis = Array2::<f32>::from_elem(dim, 10002.0);
    // Paths in parallel, folded in upstream order so f32 sums match.
    let starts = start_cells(&dem.data, release);
    for batch in starts.chunks(256) {
        let paths: Vec<Vec<Record>> = batch.par_iter().map(|&(r, c)| model.path(r, c)).collect();
        for rec in paths.iter().flatten() {
            let ix = [rec.row, rec.col];
            if rec.z_delta > f64::from(z_delta[ix]) {
                z_delta[ix] = rec.z_delta as f32;
            }
            if rec.flux > f64::from(flux[ix]) {
                flux[ix] = rec.flux as f32;
            }
            counts[ix] += 1.0;
            z_sum[ix] = (f64::from(z_sum[ix]) + rec.z_delta) as f32;
            if rec.max_gamma > f64::from(fp_ta[ix]) {
                fp_ta[ix] = rec.max_gamma as f32;
            }
            if rec.min_distance < f64::from(fp_dis[ix]) {
                fp_dis[ix] = rec.min_distance as f32;
            }
        }
    }
    fp_dis.mapv_inplace(|d| d.min(DISTANCE_CAP));

    let on_dem = |data| Grid {
        data,
        transform: dem.transform,
        crs: dem.crs.clone(),
        nodata: Some(-9999.0),
    };
    Ok(FlowPyLayers {
        z_delta: on_dem(z_delta),
        flux: on_dem(flux),
        cell_counts: on_dem(counts),
        z_delta_sum: on_dem(z_sum),
        fp_travel_angle: on_dem(fp_ta),
        fp_distance: on_dem(fp_dis),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Crs, GeoTransform};

    const P: FlowPyParams = FlowPyParams {
        alpha_deg: 25.0,
        exponent: 8,
        flux_threshold: 0.003,
        max_z_delta: 8848.0,
    };

    #[test]
    fn np_sum_matches_numpy_order() {
        let a: Vec<f64> = (1..=9).map(|i| 0.1 * f64::from(i)).collect();
        let seq = a[..7].iter().fold(0.0, |s, &x| s + x);
        assert_eq!(np_sum(&a[..7]), seq);
        let pw = (((a[0] + a[1]) + (a[2] + a[3])) + ((a[4] + a[5]) + (a[6] + a[7]))) + a[8];
        assert_eq!(np_sum(&a), pw);
        assert_eq!(np_sum(&[]), 0.0);
    }

    #[test]
    fn py_max_keeps_the_first_on_ties_and_nan() {
        assert_eq!(py_max(2.0, 2.0), 2.0);
        assert_eq!(py_max(2.0, 3.0), 3.0);
        assert_eq!(py_max(2.0, f64::NAN), 2.0);
    }

    /// A 30 degree slope falling to the south, then flat ground.
    fn slope_then_flat() -> Grid<f32> {
        let (rows, cols) = (60, 21);
        let drop = 10.0 * 30.0_f32.to_radians().tan();
        let dem = Array2::from_shape_fn((rows, cols), |(r, _)| {
            let r = r.min(20) as f32;
            1000.0 - drop * r
        });
        Grid::new(
            dem,
            GeoTransform::north_up(0.0, 600.0, 10.0, 10.0),
            Crs::Epsg(32613),
            Some(-9999.0),
        )
        .unwrap()
    }

    #[test]
    fn runout_goes_downhill_and_stops_on_the_flat() {
        let dem = slope_then_flat();
        let mut release = dem.clone();
        release.data.fill(0.0);
        release.data[[2, 10]] = 1.0;
        let out = flowpy(&dem, &release, None, &P).unwrap();
        let counts = &out.cell_counts.data;
        assert_eq!(counts[[2, 10]], 1.0, "release cell");
        assert!(counts[[3, 10]] > 0.0, "cell below the release");
        for r in 0..2 {
            assert!(counts.row(r).iter().all(|&c| c == 0.0), "nothing uphill");
        }
        // A 30 degree slope is steeper than alpha 25, so the energy line
        // carries the flow onto the flat ground, where it then stops.
        let reached: Vec<usize> = (0..60).filter(|&r| counts[[r, 10]] > 0.0).collect();
        let last = *reached.last().unwrap();
        assert!(last > 20 && last < 59, "stops on the flat, got row {last}");
        // The travel angle at the stopping point is close to alpha.
        let ta = out.fp_travel_angle.data[[last, 10]];
        assert!((ta - 25.0).abs() < 3.0, "travel angle {ta}");
        assert_eq!(out.fp_distance.data[[2, 10]], 0.0);
        assert_eq!(out.fp_distance.data[[0, 0]], DISTANCE_CAP);
    }

    #[test]
    fn edge_and_nodata_neighbourhoods_never_start() {
        let mut dem = slope_then_flat();
        dem.data[[5, 9]] = -9999.0;
        let mut release = dem.clone();
        release.data.fill(0.0);
        release.data[[0, 10]] = 1.0; // top edge
        release.data[[5, 10]] = 1.0; // touches nodata
        let out = flowpy(&dem, &release, None, &P).unwrap();
        assert!(out.cell_counts.data.iter().all(|&c| c == 0.0));
    }

    #[test]
    fn checks_params_and_alignment() {
        let dem = slope_then_flat();
        let bad = FlowPyParams {
            alpha_deg: 95.0,
            ..P
        };
        assert!(flowpy(&dem, &dem, None, &bad).is_err());
        let mut shifted = dem.clone();
        shifted.transform = GeoTransform::north_up(5.0, 600.0, 10.0, 10.0);
        assert_eq!(
            flowpy(&dem, &shifted, None, &P).unwrap_err(),
            ClassifyError::Misaligned("release")
        );
    }
}
