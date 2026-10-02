//! Terrain derivatives computed with Horn's 3x3 method.
//!
//! These mirror `gdaldem slope` / `gdaldem aspect` (default Horn algorithm,
//! degrees, azimuth aspect) so results can be validated against GDAL:
//!
//! - Without `compute_edges`, the outermost ring of cells is nodata, and any
//!   cell whose 3x3 window touches nodata is nodata.
//! - With `compute_edges`, out-of-grid neighbours are linearly extrapolated
//!   as gdaldem does (see [`neighbour`]), then any remaining nodata
//!   neighbour is replaced by the centre value; a nodata centre stays nodata.
//! - NaN is always treated as nodata.
//! - Output nodata is [`OUTPUT_NODATA`]; flat cells have aspect
//!   [`OUTPUT_NODATA`], matching gdaldem.
//!
//! Inputs must be in a projected CRS with linear units equal to elevation
//! units (e.g. metres in UTM) and a north-up transform.

use ndarray::Array2;

use crate::grid::Grid;

/// Nodata value written by slope and aspect, as in gdaldem.
pub const OUTPUT_NODATA: f32 = -9999.0;

/// Horn's x and y differences, summed in `f32` in exactly the order gdaldem
/// uses (`GDALSlopeHornAlg` / `GDALAspectAlg` sum a `float` window, then
/// widen to `double`). Matching the precision matters on near-flat terrain,
/// where aspect is very sensitive to rounding.
fn horn_sums(w: &[f32; 9]) -> (f64, f64) {
    let east = w[2] + w[5] + w[5] + w[8];
    let west = w[0] + w[3] + w[3] + w[6];
    let south = w[6] + w[7] + w[7] + w[8];
    let north = w[0] + w[1] + w[1] + w[2];
    (f64::from(east - west), f64::from(south - north))
}

/// Slope in degrees (0 = flat, 90 = vertical).
pub fn slope_deg(dem: &Grid<f32>, compute_edges: bool) -> Grid<f32> {
    let ewres = dem.transform.ew_res();
    let nsres = dem.transform.ns_res();
    horn_3x3(dem, compute_edges, |w| {
        let (dx, dy) = horn_sums(w);
        let (dx, dy) = (dx / ewres, dy / nsres);
        ((dx * dx + dy * dy).sqrt() / 8.0).atan().to_degrees() as f32
    })
}

/// Aspect as an azimuth in degrees: the compass direction the slope faces
/// (0 = north, 90 = east, clockwise). Flat cells are [`OUTPUT_NODATA`].
///
/// Like gdaldem, this ignores pixel size and so assumes square pixels, and
/// converts to an azimuth in `f32`.
pub fn aspect_deg(dem: &Grid<f32>, compute_edges: bool) -> Grid<f32> {
    horn_3x3(dem, compute_edges, |w| {
        let (dx, dy) = horn_sums(w);
        if dx == 0.0 && dy == 0.0 {
            return OUTPUT_NODATA;
        }
        let a = dy.atan2(-dx).to_degrees() as f32;
        let az = if a > 90.0 { 450.0 - a } else { 90.0 - a };
        if az == 360.0 { 0.0 } else { az }
    })
}

/// Apply `f` to the 3x3 window around every cell. The window is row-major,
/// with index 0 the north-west neighbour and 4 the centre.
fn horn_3x3(dem: &Grid<f32>, compute_edges: bool, f: impl Fn(&[f32; 9]) -> f32) -> Grid<f32> {
    let (rows, cols) = (dem.rows(), dem.cols());
    let mut out = Array2::from_elem((rows, cols), OUTPUT_NODATA);
    // gdaldem only computes edges when the grid is at least 2x2.
    let compute_edges = compute_edges && rows >= 2 && cols >= 2;

    for r in 0..rows {
        for c in 0..cols {
            let on_edge = r == 0 || c == 0 || r + 1 == rows || c + 1 == cols;
            if on_edge && !compute_edges {
                continue;
            }
            let centre = dem.data[[r, c]];
            if dem.is_nodata(centre) {
                continue;
            }
            let mut w = [0.0_f32; 9];
            let mut has_gap = false;
            for (k, slot) in w.iter_mut().enumerate() {
                let (dr, dc) = (k as isize / 3 - 1, k as isize % 3 - 1);
                let v = neighbour(dem, r, c, dr, dc).filter(|&v| !dem.is_nodata(v));
                *slot = v.unwrap_or_else(|| {
                    has_gap = true;
                    centre
                });
            }
            if has_gap && !compute_edges {
                continue;
            }
            out[[r, c]] = f(&w);
        }
    }

    Grid {
        data: out,
        transform: dem.transform,
        crs: dem.crs.clone(),
        nodata: Some(f64::from(OUTPUT_NODATA)),
    }
}

/// Value of the neighbour at offset (`dr`, `dc`) from (`r`, `c`), following
/// gdaldem's `-compute_edges` rules for positions outside the grid:
///
/// - Above the first row / below the last row, the missing row is linearly
///   extrapolated from the two nearest rows (`2a - b`); on those rows,
///   columns past the left/right edge are clamped to the edge column.
/// - Left of the first / right of the last column (other rows), the missing
///   column is extrapolated the same way.
///
/// `None` means the value is unavailable (an extrapolation touched nodata);
/// the caller substitutes the centre value, as gdaldem does.
fn neighbour(dem: &Grid<f32>, r: usize, c: usize, dr: isize, dc: isize) -> Option<f32> {
    let (rows, cols) = (dem.rows() as isize, dem.cols() as isize);
    let (rr, cc) = (r as isize + dr, c as isize + dc);
    let at = |r: isize, c: isize| dem.data[[r as usize, c as usize]];
    let interp = |a: f32, b: f32| {
        if dem.is_nodata(a) || dem.is_nodata(b) {
            return None;
        }
        let v = 2.0 * a - b;
        // gdaldem nudges an extrapolated value off the nodata value.
        Some(if dem.is_nodata(v) {
            v * (1.0 + 3.0 * f32::EPSILON)
        } else {
            v
        })
    };
    let first_or_last_row = r == 0 || r as isize == rows - 1;
    let cc_clamped = if first_or_last_row {
        cc.clamp(0, cols - 1)
    } else {
        cc
    };

    if rr < 0 {
        interp(at(0, cc_clamped), at(1, cc_clamped))
    } else if rr >= rows {
        interp(at(rows - 1, cc_clamped), at(rows - 2, cc_clamped))
    } else if cc_clamped < 0 {
        interp(at(rr, 0), at(rr, 1))
    } else if cc_clamped >= cols {
        interp(at(rr, cols - 1), at(rr, cols - 2))
    } else {
        Some(at(rr, cc_clamped))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Crs, GeoTransform};

    const RES: f64 = 10.0;

    /// Build a DEM from `z(x, y)` evaluated at cell centres, with x east and
    /// y north (both in metres).
    fn dem_from(rows: usize, cols: usize, z: impl Fn(f64, f64) -> f64) -> Grid<f32> {
        let gt = GeoTransform::north_up(0.0, rows as f64 * RES, RES, RES);
        let data = Array2::from_shape_fn((rows, cols), |(r, c)| {
            let (x, y) = gt.pixel_center(r, c);
            z(x, y) as f32
        });
        Grid::new(data, gt, Crs::Epsg(32611), Some(-9999.0)).unwrap()
    }

    fn assert_close(a: f32, b: f32, tol: f32) {
        assert!((a - b).abs() <= tol, "{a} vs {b}");
    }

    #[test]
    fn tilted_plane_slope() {
        for deg in [0.0_f64, 10.0, 30.0, 45.0, 60.0] {
            let t = deg.to_radians().tan();
            // Rising towards the north-east diagonal.
            let (ux, uy) = (0.6, 0.8);
            let dem = dem_from(7, 9, |x, y| 1000.0 + t * (ux * x + uy * y));
            let s = slope_deg(&dem, false);
            assert_close(s.data[[3, 4]], deg as f32, 1e-3);
        }
    }

    #[test]
    fn aspect_cardinals() {
        type Surface = fn(f64, f64) -> f64;
        // Surface rising to the north faces south (180), etc.
        let cases: [(&str, Surface, f32); 4] = [
            ("rise north", |_, y| y, 180.0),
            ("rise south", |_, y| -y, 0.0),
            ("rise east", |x, _| x, 270.0),
            ("rise west", |x, _| -x, 90.0),
        ];
        for (name, z, expected) in cases {
            let a = aspect_deg(&dem_from(5, 5, z), false);
            let got = a.data[[2, 2]];
            assert!((got - expected).abs() < 1e-4, "{name}: {got} vs {expected}");
        }
        // Rising to the south-west faces north-east.
        let a = aspect_deg(&dem_from(5, 5, |x, y| -x - y), false);
        assert_close(a.data[[2, 2]], 45.0, 1e-4);
    }

    #[test]
    fn flat_terrain() {
        let dem = dem_from(5, 5, |_, _| 1500.0);
        assert_eq!(slope_deg(&dem, false).data[[2, 2]], 0.0);
        assert_eq!(aspect_deg(&dem, false).data[[2, 2]], OUTPUT_NODATA);
    }

    #[test]
    fn edges_are_nodata_without_compute_edges() {
        let dem = dem_from(4, 5, |x, _| x);
        let s = slope_deg(&dem, false);
        for c in 0..5 {
            assert_eq!(s.data[[0, c]], OUTPUT_NODATA);
            assert_eq!(s.data[[3, c]], OUTPUT_NODATA);
        }
        assert_eq!(s.data[[1, 0]], OUTPUT_NODATA);
        assert_eq!(s.data[[1, 4]], OUTPUT_NODATA);
        assert_close(s.data[[1, 1]], 45.0, 1e-3);
    }

    #[test]
    fn compute_edges_fills_border() {
        let dem = dem_from(4, 5, |x, _| x);
        let s = slope_deg(&dem, true);
        assert!(s.data.iter().all(|&v| v != OUTPUT_NODATA));
        // Extrapolation reproduces a plane exactly on non-corner edges.
        for (r, c) in [(0, 2), (3, 2), (1, 0), (2, 4)] {
            assert_close(s.data[[r, c]], 45.0, 1e-3);
        }
        // Corners clamp the out-of-grid column, halving the x-gradient.
        assert_close(s.data[[0, 0]], 0.5_f64.atan().to_degrees() as f32, 1e-3);
        assert_close(s.data[[1, 2]], 45.0, 1e-3);
    }

    #[test]
    fn nodata_propagates() {
        let mut dem = dem_from(5, 5, |x, _| x);
        dem.data[[2, 2]] = -9999.0;
        dem.data[[1, 3]] = f32::NAN;
        let s = slope_deg(&dem, false);
        // Every interior cell touches one of the two gaps.
        for r in 1..4 {
            for c in 1..4 {
                assert_eq!(s.data[[r, c]], OUTPUT_NODATA, "({r},{c})");
            }
        }
        let s = slope_deg(&dem, true);
        assert_eq!(s.data[[2, 2]], OUTPUT_NODATA, "nodata centre stays nodata");
        assert_ne!(s.data[[3, 3]], OUTPUT_NODATA, "neighbour gap is filled");
        assert_eq!(s.nodata, Some(-9999.0));
    }
}
