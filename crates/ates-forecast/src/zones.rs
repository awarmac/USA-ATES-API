//! Forecast zone polygons and point-in-polygon lookup (WGS 84 lon/lat).

/// One ring: closed or not, lon/lat vertices.
pub type Ring = Vec<(f64, f64)>;

/// A zone: one or more polygons, each an outer ring and its holes.
#[derive(Debug, Clone, PartialEq)]
pub struct Zone {
    pub id: String,
    pub name: Option<String>,
    pub polygons: Vec<Vec<Ring>>,
}

impl Zone {
    /// `true` if the point is inside any polygon (inside its outer ring and
    /// outside its holes). Points exactly on an edge may fall either way.
    pub fn contains(&self, lon: f64, lat: f64) -> bool {
        self.polygons.iter().any(|rings| {
            let mut it = rings.iter();
            it.next()
                .is_some_and(|outer| in_ring(outer, lon, lat) && !it.any(|h| in_ring(h, lon, lat)))
        })
    }
}

/// Even-odd ray casting.
fn in_ring(ring: &[(f64, f64)], x: f64, y: f64) -> bool {
    let n = ring.len();
    if n < 3 {
        return false;
    }
    let mut inside = false;
    let mut j = n - 1;
    for i in 0..n {
        let (xi, yi) = ring[i];
        let (xj, yj) = ring[j];
        if (yi > y) != (yj > y) && x < (xj - xi) * (y - yi) / (yj - yi) + xi {
            inside = !inside;
        }
        j = i;
    }
    inside
}

#[cfg(test)]
mod tests {
    use super::*;

    fn square(x0: f64, y0: f64, s: f64) -> Ring {
        vec![
            (x0, y0),
            (x0 + s, y0),
            (x0 + s, y0 + s),
            (x0, y0 + s),
            (x0, y0),
        ]
    }

    #[test]
    fn holes_and_multipolygons() {
        let z = Zone {
            id: "a".into(),
            name: None,
            polygons: vec![
                vec![square(0.0, 0.0, 10.0), square(4.0, 4.0, 2.0)],
                vec![square(20.0, 0.0, 1.0)],
            ],
        };
        assert!(z.contains(1.0, 1.0));
        assert!(!z.contains(5.0, 5.0), "in the hole");
        assert!(z.contains(20.5, 0.5), "second polygon");
        assert!(!z.contains(15.0, 5.0));
    }
}
