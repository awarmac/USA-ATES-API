//! Georeferenced raster grid.

use ndarray::Array2;
use thiserror::Error;

use crate::crs::Crs;

#[derive(Debug, Error, PartialEq)]
pub enum GridError {
    #[error("geotransform is rotated or sheared; only north-up grids are supported")]
    Rotated,
    #[error("pixel size must be finite and non-zero (got {0}, {1})")]
    BadPixelSize(f64, f64),
}

/// GDAL-style affine geotransform:
/// `x = gt[0] + col * gt[1] + row * gt[2]`, `y = gt[3] + col * gt[4] + row * gt[5]`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GeoTransform(pub [f64; 6]);

impl GeoTransform {
    /// North-up transform from the top-left corner and a positive pixel size.
    pub fn north_up(origin_x: f64, origin_y: f64, pixel_w: f64, pixel_h: f64) -> Self {
        Self([origin_x, pixel_w, 0.0, origin_y, 0.0, -pixel_h])
    }

    pub fn is_north_up(&self) -> bool {
        self.0[2] == 0.0 && self.0[4] == 0.0
    }

    /// Pixel width (east-west resolution), always positive.
    pub fn ew_res(&self) -> f64 {
        self.0[1].abs()
    }

    /// Pixel height (north-south resolution), always positive.
    pub fn ns_res(&self) -> f64 {
        self.0[5].abs()
    }

    /// Map coordinates of the centre of pixel (`row`, `col`).
    pub fn pixel_center(&self, row: usize, col: usize) -> (f64, f64) {
        let (c, r) = (col as f64 + 0.5, row as f64 + 0.5);
        let g = &self.0;
        (g[0] + c * g[1] + r * g[2], g[3] + c * g[4] + r * g[5])
    }

    /// Fractional (row, col) of a map coordinate. North-up grids only.
    pub fn to_pixel(&self, x: f64, y: f64) -> (f64, f64) {
        let g = &self.0;
        ((y - g[3]) / g[5], (x - g[0]) / g[1])
    }
}

/// A north-up raster with georeferencing and an optional nodata value.
#[derive(Debug, Clone, PartialEq)]
pub struct Grid<T> {
    pub data: Array2<T>,
    pub transform: GeoTransform,
    pub crs: Crs,
    pub nodata: Option<f64>,
}

impl<T> Grid<T> {
    pub fn new(
        data: Array2<T>,
        transform: GeoTransform,
        crs: Crs,
        nodata: Option<f64>,
    ) -> Result<Self, GridError> {
        if !transform.is_north_up() {
            return Err(GridError::Rotated);
        }
        let (w, h) = (transform.0[1], transform.0[5]);
        if !(w.is_finite() && h.is_finite()) || w == 0.0 || h == 0.0 {
            return Err(GridError::BadPixelSize(w, h));
        }
        Ok(Self {
            data,
            transform,
            crs,
            nodata,
        })
    }

    pub fn rows(&self) -> usize {
        self.data.nrows()
    }

    pub fn cols(&self) -> usize {
        self.data.ncols()
    }

    /// (row, col) of the cell containing map coordinate (`x`, `y`), if any.
    pub fn cell_at(&self, x: f64, y: f64) -> Option<(usize, usize)> {
        let (r, c) = self.transform.to_pixel(x, y);
        let inside = r >= 0.0 && c >= 0.0 && r < self.rows() as f64 && c < self.cols() as f64;
        inside.then(|| (r.floor() as usize, c.floor() as usize))
    }
}

impl<T: Clone> Grid<T> {
    /// Drop `n` cells from every side, keeping at least one row and column.
    /// The transform is shifted so remaining cells keep their positions.
    pub fn trim(&self, n: usize) -> Self {
        let n_r = n.min(self.rows().saturating_sub(1) / 2);
        let n_c = n.min(self.cols().saturating_sub(1) / 2);
        let data = self
            .data
            .slice(ndarray::s![n_r..self.rows() - n_r, n_c..self.cols() - n_c])
            .to_owned();
        let g = self.transform.0;
        let (x0, y0) = (g[0] + n_c as f64 * g[1], g[3] + n_r as f64 * g[5]);
        Self {
            data,
            transform: GeoTransform([x0, g[1], g[2], y0, g[4], g[5]]),
            crs: self.crs.clone(),
            nodata: self.nodata,
        }
    }
}

impl Grid<f32> {
    /// True if `v` is NaN or equals this grid's nodata value.
    pub fn is_nodata(&self, v: f32) -> bool {
        v.is_nan() || self.nodata.is_some_and(|nd| f64::from(v) == nd)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pixel_center_and_back() {
        let gt = GeoTransform::north_up(500_000.0, 5_700_000.0, 10.0, 10.0);
        let (x, y) = gt.pixel_center(2, 3);
        assert_eq!((x, y), (500_035.0, 5_699_975.0));
        assert_eq!(gt.to_pixel(x, y), (2.5, 3.5));
    }

    #[test]
    fn rejects_rotated_and_zero_size() {
        let crs = Crs::Epsg(32611);
        let rotated = GeoTransform([0.0, 1.0, 0.1, 0.0, 0.0, -1.0]);
        assert_eq!(
            Grid::new(Array2::<f32>::zeros((2, 2)), rotated, crs.clone(), None),
            Err(GridError::Rotated)
        );
        let zero = GeoTransform([0.0, 0.0, 0.0, 0.0, 0.0, -1.0]);
        assert!(matches!(
            Grid::new(Array2::<f32>::zeros((2, 2)), zero, crs, None),
            Err(GridError::BadPixelSize(..))
        ));
    }

    #[test]
    fn cell_lookup() {
        let gt = GeoTransform::north_up(100.0, 200.0, 10.0, 10.0);
        let g = Grid::new(Array2::<f32>::zeros((3, 4)), gt, Crs::Epsg(32611), None).unwrap();
        assert_eq!(g.cell_at(100.0, 200.0), Some((0, 0)));
        assert_eq!(g.cell_at(139.9, 170.1), Some((2, 3)));
        assert_eq!(g.cell_at(140.0, 190.0), None);
        assert_eq!(g.cell_at(99.9, 190.0), None);
        assert_eq!(g.cell_at(120.0, 170.0), None, "bottom edge is exclusive");
    }

    #[test]
    fn trim_keeps_positions() {
        let gt = GeoTransform::north_up(1000.0, 2000.0, 10.0, 10.0);
        let data = Array2::from_shape_fn((6, 8), |(r, c)| (r * 10 + c) as f32);
        let g = Grid::new(data, gt, Crs::Epsg(32611), None).unwrap();
        let t = g.trim(2);
        assert_eq!(t.data.dim(), (2, 4));
        assert_eq!(t.data[[0, 0]], 22.0);
        assert_eq!(
            t.transform.pixel_center(0, 0),
            g.transform.pixel_center(2, 2)
        );
        // Over-trimming leaves the central cell(s) rather than an empty grid.
        let t = g.trim(100);
        assert_eq!(t.data.dim(), (2, 2));
        assert_eq!(t.data[[0, 0]], 23.0);
        assert_eq!(g.trim(0), g);
    }

    #[test]
    fn nodata_detection() {
        let g = Grid::new(
            Array2::<f32>::zeros((1, 1)),
            GeoTransform::north_up(0.0, 0.0, 1.0, 1.0),
            Crs::Epsg(32611),
            Some(-9999.0),
        )
        .unwrap();
        assert!(g.is_nodata(-9999.0));
        assert!(g.is_nodata(f32::NAN));
        assert!(!g.is_nodata(0.0));
    }
}
