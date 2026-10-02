//! Class areas inside polygons.
//!
//! A cell counts when its centre is inside any of the polygons. Each polygon
//! is a list of rings, outer first and holes after, tested with the
//! even-odd rule. Coordinates are in the grid's projected CRS.

use crate::grid::Grid;

/// A polygon: its outer ring followed by any holes.
pub type Polygon = Vec<Vec<(f64, f64)>>;

/// Area per ATES class inside the polygons.
#[derive(Debug, Clone, PartialEq)]
pub struct AreaStats {
    /// Area in classes 0-4 (m^2).
    pub class_m2: [f64; 5],
    /// Area of cells with no class (m^2).
    pub nodata_m2: f64,
    /// Cells whose centre is inside.
    pub cells: usize,
}

impl AreaStats {
    pub fn total_m2(&self) -> f64 {
        self.class_m2.iter().sum::<f64>() + self.nodata_m2
    }
}

/// Even-odd test over all rings of one polygon.
fn inside(polygon: &Polygon, x: f64, y: f64) -> bool {
    let mut odd = false;
    for ring in polygon {
        let n = ring.len();
        if n < 3 {
            continue;
        }
        let mut j = n - 1;
        for i in 0..n {
            let (xi, yi) = ring[i];
            let (xj, yj) = ring[j];
            if (yi > y) != (yj > y) && x < (xj - xi) * (y - yi) / (yj - yi) + xi {
                odd = !odd;
            }
            j = i;
        }
    }
    odd
}

/// Area per class of `classes` inside `polygons`. Cells outside the grid
/// are not counted.
pub fn class_areas(classes: &Grid<i16>, polygons: &[Polygon]) -> AreaStats {
    let mut stats = AreaStats {
        class_m2: [0.0; 5],
        nodata_m2: 0.0,
        cells: 0,
    };
    let cell_m2 = classes.transform.ew_res() * classes.transform.ns_res();
    for polygon in polygons {
        let pts = polygon.iter().flatten();
        let (mut x0, mut y0, mut x1, mut y1) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
        for &(x, y) in pts {
            (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x), y1.max(y));
        }
        if x0 > x1 {
            continue;
        }
        // Rows and columns whose centres can fall inside the bounding box.
        let (r_a, c_a) = classes.transform.to_pixel(x0, y1);
        let (r_b, c_b) = classes.transform.to_pixel(x1, y0);
        let clamp = |v: f64, n: usize| v.floor().clamp(0.0, n as f64) as usize;
        let (rows, cols) = (classes.rows(), classes.cols());
        let (r0, r1) = (clamp(r_a.min(r_b), rows), clamp(r_a.max(r_b) + 1.0, rows));
        let (c0, c1) = (clamp(c_a.min(c_b), cols), clamp(c_a.max(c_b) + 1.0, cols));
        for r in r0..r1 {
            for c in c0..c1 {
                let (x, y) = classes.transform.pixel_center(r, c);
                // Count each cell once, even where polygons overlap.
                let earlier = polygons
                    .iter()
                    .take_while(|p| !std::ptr::eq(*p, polygon))
                    .any(|p| inside(p, x, y));
                if earlier || !inside(polygon, x, y) {
                    continue;
                }
                stats.cells += 1;
                match classes.data[[r, c]] {
                    v @ 0..=4 => stats.class_m2[v as usize] += cell_m2,
                    _ => stats.nodata_m2 += cell_m2,
                }
            }
        }
    }
    stats
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Crs, GeoTransform};
    use ndarray::Array2;

    /// 10 x 10 cells of 10 m from (0, 100): columns 0-4 class 1, 5-9 class 3.
    fn grid() -> Grid<i16> {
        Grid::new(
            Array2::from_shape_fn((10, 10), |(_, c)| if c < 5 { 1 } else { 3 }),
            GeoTransform::north_up(0.0, 100.0, 10.0, 10.0),
            Crs::Epsg(32613),
            Some(-9999.0),
        )
        .unwrap()
    }

    fn rect(x0: f64, y0: f64, x1: f64, y1: f64) -> Vec<(f64, f64)> {
        vec![(x0, y0), (x1, y0), (x1, y1), (x0, y1), (x0, y0)]
    }

    #[test]
    fn rectangle_over_two_classes() {
        let s = class_areas(&grid(), &[vec![rect(0.0, 0.0, 100.0, 40.0)]]);
        assert_eq!(s.cells, 40);
        assert_eq!(s.class_m2[1], 2000.0);
        assert_eq!(s.class_m2[3], 2000.0);
        assert_eq!(s.total_m2(), 4000.0);
    }

    #[test]
    fn holes_overlaps_and_outside() {
        // A 60 x 60 square with a 20 x 20 hole: 36 - 4 = 32 cells.
        let holed = vec![rect(0.0, 0.0, 60.0, 60.0), rect(20.0, 20.0, 40.0, 40.0)];
        assert_eq!(class_areas(&grid(), std::slice::from_ref(&holed)).cells, 32);
        // The same polygon twice still counts each cell once.
        assert_eq!(class_areas(&grid(), &[holed.clone(), holed]).cells, 32);
        // Entirely outside the grid.
        let away = vec![rect(500.0, 500.0, 600.0, 600.0)];
        assert_eq!(class_areas(&grid(), &[away]).cells, 0);
    }
}
