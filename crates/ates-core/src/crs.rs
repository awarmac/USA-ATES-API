//! Coordinate reference system helpers.

use thiserror::Error;

/// A coordinate reference system, identified either by EPSG code or WKT.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Crs {
    Epsg(u32),
    Wkt(String),
}

#[derive(Debug, Error, PartialEq)]
pub enum CrsError {
    #[error("latitude {0} is outside the UTM range [-80, 84]")]
    OutsideUtm(f64),
    #[error("coordinate ({0}, {1}) is not a finite lon/lat")]
    BadLonLat(f64, f64),
}

/// EPSG code of the WGS 84 / UTM zone containing (`lon`, `lat`):
/// 326xx in the northern hemisphere, 327xx in the southern.
///
/// Uses the regular 6° zones and ignores the Norway/Svalbard exceptions;
/// any projected metric CRS is adequate for slope computation.
pub fn utm_epsg_for(lon: f64, lat: f64) -> Result<u32, CrsError> {
    if !(lon.is_finite() && lat.is_finite() && (-180.0..=180.0).contains(&lon)) {
        return Err(CrsError::BadLonLat(lon, lat));
    }
    if !(-80.0..=84.0).contains(&lat) {
        return Err(CrsError::OutsideUtm(lat));
    }
    let zone = (((lon + 180.0) / 6.0).floor() as u32 + 1).min(60);
    Ok(if lat >= 0.0 {
        32600 + zone
    } else {
        32700 + zone
    })
}

/// Axis-aligned bounding box. For request extents this is in WGS 84
/// lon/lat degrees (`min_x` = west, `max_y` = north).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BBox {
    pub min_x: f64,
    pub min_y: f64,
    pub max_x: f64,
    pub max_y: f64,
}

impl BBox {
    pub fn new(min_x: f64, min_y: f64, max_x: f64, max_y: f64) -> Self {
        Self {
            min_x: min_x.min(max_x),
            min_y: min_y.min(max_y),
            max_x: min_x.max(max_x),
            max_y: min_y.max(max_y),
        }
    }

    /// Degenerate box around a single point.
    pub fn point(x: f64, y: f64) -> Self {
        Self::new(x, y, x, y)
    }

    pub fn center(&self) -> (f64, f64) {
        (
            (self.min_x + self.max_x) / 2.0,
            (self.min_y + self.max_y) / 2.0,
        )
    }

    /// Grow the box by `d` on every side (same units as the box).
    pub fn buffered(&self, d: f64) -> Self {
        Self::new(
            self.min_x - d,
            self.min_y - d,
            self.max_x + d,
            self.max_y + d,
        )
    }

    /// Smallest box covering every point in `pts`, or `None` if empty.
    pub fn from_points(pts: impl IntoIterator<Item = (f64, f64)>) -> Option<Self> {
        pts.into_iter().fold(None, |acc, (x, y)| {
            Some(match acc {
                None => Self::point(x, y),
                Some(b) => Self::new(
                    b.min_x.min(x),
                    b.min_y.min(y),
                    b.max_x.max(x),
                    b.max_y.max(y),
                ),
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utm_zone_examples() {
        // Bow Summit, Alberta (~116.5 W) -> zone 11N.
        assert_eq!(utm_epsg_for(-116.5, 51.7), Ok(32611));
        // Wasatch, Utah (~111.6 W) -> zone 12N.
        assert_eq!(utm_epsg_for(-111.6, 40.6), Ok(32612));
        // Southern hemisphere.
        assert_eq!(utm_epsg_for(170.1, -43.6), Ok(32759));
    }

    #[test]
    fn utm_zone_boundaries() {
        assert_eq!(utm_epsg_for(-180.0, 10.0), Ok(32601));
        assert_eq!(utm_epsg_for(-174.0, 10.0), Ok(32602));
        assert_eq!(utm_epsg_for(-174.000_001, 10.0), Ok(32601));
        assert_eq!(utm_epsg_for(0.0, 0.0), Ok(32631));
        assert_eq!(utm_epsg_for(180.0, 10.0), Ok(32660));
    }

    #[test]
    fn utm_rejects_bad_input() {
        assert_eq!(utm_epsg_for(0.0, 85.0), Err(CrsError::OutsideUtm(85.0)));
        assert_eq!(utm_epsg_for(0.0, -81.0), Err(CrsError::OutsideUtm(-81.0)));
        assert!(utm_epsg_for(f64::NAN, 0.0).is_err());
        assert!(utm_epsg_for(181.0, 0.0).is_err());
    }

    #[test]
    fn bbox_ops() {
        let b = BBox::new(2.0, 3.0, 0.0, 1.0);
        assert_eq!(b, BBox::new(0.0, 1.0, 2.0, 3.0));
        assert_eq!(b.center(), (1.0, 2.0));
        assert_eq!(b.buffered(1.0), BBox::new(-1.0, 0.0, 3.0, 4.0));
        assert_eq!(
            BBox::from_points([(1.0, 5.0), (-1.0, 2.0)]),
            Some(BBox::new(-1.0, 2.0, 1.0, 5.0))
        );
        assert_eq!(BBox::from_points([]), None);
    }
}
