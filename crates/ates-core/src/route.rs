//! Route evaluation: how much of a route lies in each ATES class, and where.
//!
//! A route is one or more polylines in the grids' projected CRS. Each part
//! is cut into pieces no longer than `step_m`, and each piece is sampled at
//! its midpoint. Consecutive pieces with the same class become a stretch.
//!
//! For each stretch the evaluation reports:
//! - its length and elevation range;
//! - the dominant aspect, as one of 8 compass sectors;
//! - how much of it is inside a modelled release area or on a modelled
//!   avalanche path;
//! - its highest overhead exposure.
//!
//! Lengths are horizontal (map) distances in metres. The result describes
//! the **modeled terrain** along the route. It is not an avalanche forecast
//! and does not rate a route as safe.

use crate::grid::Grid;

/// Errors from route evaluation.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum RouteError {
    #[error("route has no segment with two distinct points")]
    Empty,
    #[error("layer `{0}` is not aligned with the ATES grid")]
    Misaligned(&'static str),
    #[error("step must be positive (got {0})")]
    BadStep(f64),
}

/// The 8 compass sectors used for aspect (CAIC forecasts use the same).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Aspect {
    N,
    NE,
    E,
    SE,
    S,
    SW,
    W,
    NW,
}

impl Aspect {
    pub const ALL: [Aspect; 8] = [
        Aspect::N,
        Aspect::NE,
        Aspect::E,
        Aspect::SE,
        Aspect::S,
        Aspect::SW,
        Aspect::W,
        Aspect::NW,
    ];

    /// Sector of an azimuth in degrees (0 = north, clockwise), each
    /// centred on its direction: N is [337.5, 22.5).
    pub fn from_azimuth(az: f32) -> Option<Self> {
        if !az.is_finite() || az < 0.0 {
            return None;
        }
        let i = ((f64::from(az) + 22.5) / 45.0).floor() as usize % 8;
        Some(Self::ALL[i])
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Aspect::N => "N",
            Aspect::NE => "NE",
            Aspect::E => "E",
            Aspect::SE => "SE",
            Aspect::S => "S",
            Aspect::SW => "SW",
            Aspect::W => "W",
            Aspect::NW => "NW",
        }
    }
}

/// Grids sampled along the route. Optional layers that are present must
/// share the ATES grid's shape and transform.
#[derive(Debug, Clone, Copy)]
pub struct RouteLayers<'a> {
    /// ATES classes 0-4, nodata -9999.
    pub ates: &'a Grid<i16>,
    pub dem: Option<&'a Grid<f32>>,
    /// Aspect azimuth in degrees (nodata on flat cells).
    pub aspect: Option<&'a Grid<f32>>,
    /// Binary release areas (1 = release).
    pub pra: Option<&'a Grid<i16>>,
    /// Flow-Py travel angle; > 0 marks a cell on a modelled avalanche path.
    pub fp_travel_angle: Option<&'a Grid<f32>>,
    /// Overhead exposure 0-100.
    pub overhead: Option<&'a Grid<i16>>,
}

/// What the route crosses for one class run.
#[derive(Debug, Clone, PartialEq)]
pub struct Stretch {
    /// ATES class, or `None` where the route is outside the grid or on
    /// nodata.
    pub class: Option<i16>,
    /// `true` if this stretch is outside the grid (not just nodata).
    pub outside: bool,
    /// Distance along the route where the stretch starts and ends (m).
    pub start_m: f64,
    pub end_m: f64,
    /// Index of the route part (polyline) it belongs to.
    pub part: usize,
    /// Projected points along the stretch, start to end: its boundaries and
    /// the route's own vertices in between.
    pub points: Vec<(f64, f64)>,
    pub elevation_min_m: Option<f32>,
    pub elevation_max_m: Option<f32>,
    /// Length per aspect sector, in `Aspect::ALL` order (flat ground and
    /// missing aspect are not counted).
    pub aspect_m: [f64; 8],
    /// Length inside a modelled release area.
    pub release_area_m: f64,
    /// Length on a modelled avalanche path (travel angle > 0).
    pub avalanche_path_m: f64,
    pub max_overhead: Option<i16>,
}

impl Stretch {
    pub fn length_m(&self) -> f64 {
        self.end_m - self.start_m
    }

    /// The aspect sector with the most length, if any.
    pub fn dominant_aspect(&self) -> Option<Aspect> {
        let (i, &m) = self
            .aspect_m
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(b.1))?;
        (m > 0.0).then_some(Aspect::ALL[i])
    }
}

/// Totals and stretches for a route.
#[derive(Debug, Clone, PartialEq)]
pub struct RouteReport {
    pub total_m: f64,
    /// Length in classes 0-4.
    pub class_m: [f64; 5],
    /// Length on cells with no class (nodata) inside the grid.
    pub nodata_m: f64,
    /// Length outside the grid.
    pub outside_m: f64,
    pub release_area_m: f64,
    pub avalanche_path_m: f64,
    pub stretches: Vec<Stretch>,
}

/// One sampled piece of the route.
struct Piece {
    part: usize,
    a: (f64, f64),
    b: (f64, f64),
    len: f64,
    cell: Option<(usize, usize)>,
    class: Option<i16>,
    /// The piece ends at a vertex of the input polyline.
    ends_at_vertex: bool,
}

/// Evaluate `parts` (polylines in the grids' CRS) against `layers`,
/// sampling at most every `step_m` metres.
pub fn evaluate(
    parts: &[Vec<(f64, f64)>],
    layers: &RouteLayers<'_>,
    step_m: f64,
) -> Result<RouteReport, RouteError> {
    if !(step_m > 0.0 && step_m.is_finite()) {
        return Err(RouteError::BadStep(step_m));
    }
    let ates = layers.ates;
    let aligned = |dim: (usize, usize), t: &crate::GeoTransform, name| {
        if dim == ates.data.dim() && *t == ates.transform {
            Ok(())
        } else {
            Err(RouteError::Misaligned(name))
        }
    };
    if let Some(g) = layers.dem {
        aligned(g.data.dim(), &g.transform, "dem")?;
    }
    if let Some(g) = layers.aspect {
        aligned(g.data.dim(), &g.transform, "aspect")?;
    }
    if let Some(g) = layers.pra {
        aligned(g.data.dim(), &g.transform, "pra")?;
    }
    if let Some(g) = layers.fp_travel_angle {
        aligned(g.data.dim(), &g.transform, "fp_travel_angle")?;
    }
    if let Some(g) = layers.overhead {
        aligned(g.data.dim(), &g.transform, "overhead")?;
    }

    // Cut every segment into equal pieces of at most `step_m`.
    let mut pieces = Vec::new();
    for (pi, part) in parts.iter().enumerate() {
        for w in part.windows(2) {
            let (a, b) = (w[0], w[1]);
            let len = (b.0 - a.0).hypot(b.1 - a.1);
            if len <= 0.0 || !len.is_finite() {
                continue;
            }
            let n = (len / step_m).ceil().max(1.0) as usize;
            for k in 0..n {
                let t0 = k as f64 / n as f64;
                let t1 = (k + 1) as f64 / n as f64;
                let p = |t: f64| (a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t);
                let mid = p((t0 + t1) / 2.0);
                let cell = ates.cell_at(mid.0, mid.1);
                let class = cell
                    .map(|ix| ates.data[ix])
                    .filter(|&c| (0..=4).contains(&c));
                pieces.push(Piece {
                    part: pi,
                    a: p(t0),
                    b: p(t1),
                    len: len / n as f64,
                    cell,
                    class,
                    ends_at_vertex: k + 1 == n,
                });
            }
        }
    }
    if pieces.is_empty() {
        return Err(RouteError::Empty);
    }

    let mut report = RouteReport {
        total_m: 0.0,
        class_m: [0.0; 5],
        nodata_m: 0.0,
        outside_m: 0.0,
        release_area_m: 0.0,
        avalanche_path_m: 0.0,
        stretches: Vec::new(),
    };
    let mut along = 0.0;
    for p in &pieces {
        let start = along;
        along += p.len;
        report.total_m += p.len;
        match (p.cell, p.class) {
            (None, _) => report.outside_m += p.len,
            (Some(_), None) => report.nodata_m += p.len,
            (Some(_), Some(c)) => report.class_m[c as usize] += p.len,
        }

        let continues = report.stretches.last().is_some_and(|s: &Stretch| {
            s.part == p.part && s.class == p.class && s.outside == p.cell.is_none()
        });
        if !continues {
            // Close the previous stretch at the boundary point.
            if let Some(prev) = report.stretches.last_mut()
                && prev.part == p.part
                && prev.points.last() != Some(&p.a)
            {
                prev.points.push(p.a);
            }
            report.stretches.push(Stretch {
                class: p.class,
                outside: p.cell.is_none(),
                start_m: start,
                end_m: start,
                part: p.part,
                points: vec![p.a],
                elevation_min_m: None,
                elevation_max_m: None,
                aspect_m: [0.0; 8],
                release_area_m: 0.0,
                avalanche_path_m: 0.0,
                max_overhead: None,
            });
        }
        let s = report.stretches.last_mut().expect("just pushed");
        s.end_m = along;
        // Keep only the input vertices; boundaries are added above.
        if p.ends_at_vertex {
            s.points.push(p.b);
        }
        let Some(ix) = p.cell else { continue };

        if let Some(dem) = layers.dem {
            let z = dem.data[ix];
            if !dem.is_nodata(z) {
                s.elevation_min_m = Some(s.elevation_min_m.map_or(z, |m| m.min(z)));
                s.elevation_max_m = Some(s.elevation_max_m.map_or(z, |m| m.max(z)));
            }
        }
        if let Some(asp) = layers.aspect {
            let v = asp.data[ix];
            if !asp.is_nodata(v)
                && let Some(a) = Aspect::from_azimuth(v)
            {
                s.aspect_m[a as usize] += p.len;
            }
        }
        if let Some(pra) = layers.pra
            && pra.data[ix] == 1
        {
            s.release_area_m += p.len;
            report.release_area_m += p.len;
        }
        if let Some(fp) = layers.fp_travel_angle {
            let v = fp.data[ix];
            if !fp.is_nodata(v) && v > 0.0 {
                s.avalanche_path_m += p.len;
                report.avalanche_path_m += p.len;
            }
        }
        if let Some(o) = layers.overhead {
            let v = o.data[ix];
            if (0..=100).contains(&v) {
                s.max_overhead = Some(s.max_overhead.map_or(v, |m| m.max(v)));
            }
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Crs, GeoTransform};
    use ndarray::Array2;

    /// 10 x 10 cells of 10 m from (0, 100): columns 0-4 class 1, 5-9 class 3.
    fn classes() -> Grid<i16> {
        let data = Array2::from_shape_fn((10, 10), |(_, c)| if c < 5 { 1 } else { 3 });
        Grid::new(
            data,
            GeoTransform::north_up(0.0, 100.0, 10.0, 10.0),
            Crs::Epsg(32613),
            Some(-9999.0),
        )
        .unwrap()
    }

    fn only_ates(g: &Grid<i16>) -> RouteLayers<'_> {
        RouteLayers {
            ates: g,
            dem: None,
            aspect: None,
            pra: None,
            fp_travel_angle: None,
            overhead: None,
        }
    }

    #[test]
    fn lengths_per_class_are_exact() {
        let g = classes();
        // West to east across the middle row: 50 m in class 1, 50 m in class 3.
        let r = evaluate(&[vec![(0.0, 55.0), (100.0, 55.0)]], &only_ates(&g), 5.0).unwrap();
        assert!((r.total_m - 100.0).abs() < 1e-9);
        assert!((r.class_m[1] - 50.0).abs() < 1e-9);
        assert!((r.class_m[3] - 50.0).abs() < 1e-9);
        assert_eq!(r.stretches.len(), 2);
        assert_eq!(r.stretches[1].class, Some(3));
        assert!((r.stretches[1].start_m - 50.0).abs() < 1e-9);
        // Only the boundary and the end vertex, not every 5 m sample.
        assert_eq!(r.stretches[0].points, vec![(0.0, 55.0), (50.0, 55.0)]);
        assert_eq!(r.stretches[1].points, vec![(50.0, 55.0), (100.0, 55.0)]);
    }

    #[test]
    fn outside_and_nodata_are_counted_separately() {
        let mut g = classes();
        g.data[[4, 2]] = -9999;
        // From x = -20 (outside) to x = 30 along row 4 (y in 50..60).
        let r = evaluate(&[vec![(-20.0, 55.0), (30.0, 55.0)]], &only_ates(&g), 5.0).unwrap();
        assert!((r.outside_m - 20.0).abs() < 1e-9);
        assert!((r.nodata_m - 10.0).abs() < 1e-9);
        assert!((r.class_m[1] - 20.0).abs() < 1e-9);
        assert!(r.stretches[0].outside && r.stretches[0].class.is_none());
    }

    #[test]
    fn context_layers_are_sampled() {
        let g = classes();
        let dem = Grid {
            data: Array2::from_shape_fn((10, 10), |(_, c)| 2000.0 + c as f32),
            transform: g.transform,
            crs: g.crs.clone(),
            nodata: Some(-9999.0),
        };
        let aspect = Grid {
            data: Array2::from_elem((10, 10), 270.0_f32),
            ..dem.clone()
        };
        let mut pra = g.clone();
        pra.data.fill(0);
        pra.data[[4, 7]] = 1;
        let mut ovh = g.clone();
        ovh.data.fill(0);
        ovh.data[[4, 8]] = 37;
        let layers = RouteLayers {
            ates: &g,
            dem: Some(&dem),
            aspect: Some(&aspect),
            pra: Some(&pra),
            fp_travel_angle: None,
            overhead: Some(&ovh),
        };
        let r = evaluate(&[vec![(0.0, 55.0), (100.0, 55.0)]], &layers, 5.0).unwrap();
        let s = &r.stretches[1];
        assert_eq!(
            (s.elevation_min_m, s.elevation_max_m),
            (Some(2005.0), Some(2009.0))
        );
        assert_eq!(s.dominant_aspect(), Some(Aspect::W));
        assert!((s.release_area_m - 10.0).abs() < 1e-9);
        assert_eq!(s.max_overhead, Some(37));
        assert!((r.release_area_m - 10.0).abs() < 1e-9);
    }

    #[test]
    fn parts_never_merge_and_errors() {
        let g = classes();
        let parts = [
            vec![(0.0, 55.0), (20.0, 55.0)],
            vec![(0.0, 25.0), (20.0, 25.0)],
        ];
        let r = evaluate(&parts, &only_ates(&g), 5.0).unwrap();
        assert_eq!(r.stretches.len(), 2, "same class, different parts");
        assert!((r.total_m - 40.0).abs() < 1e-9);
        assert_eq!(
            evaluate(&[vec![(1.0, 1.0)]], &only_ates(&g), 5.0),
            Err(RouteError::Empty)
        );
        assert!(evaluate(&parts, &only_ates(&g), 0.0).is_err());
    }

    #[test]
    fn aspect_sectors() {
        assert_eq!(Aspect::from_azimuth(0.0), Some(Aspect::N));
        assert_eq!(Aspect::from_azimuth(350.0), Some(Aspect::N));
        assert_eq!(Aspect::from_azimuth(22.5), Some(Aspect::NE));
        assert_eq!(Aspect::from_azimuth(180.0), Some(Aspect::S));
        assert_eq!(Aspect::from_azimuth(-9999.0), None);
    }
}
