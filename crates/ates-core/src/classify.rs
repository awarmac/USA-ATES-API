//! Classification of terrain into ATES-style classes.
//!
//! [`Classifier`] is the swappable interface. Two implementations exist:
//! [`SlopeBandClassifier`] (a crude slope-only proxy, Milestone 2) and
//! `autoates::AutoAtesClassifier` (the AutoATES v2.0 rules).
//!
//! The slope-band step is a faithful port of `AutoATES_classifier.py`
//! (AutoATES-v2.0 @ 3afcb49, lines 99-124), quirks included, so it can be
//! golden-tested against AutoATES's own outputs:
//!
//! - slope is truncated to an integer (`astype('int16')`);
//! - for smoothing, nodata (negative) slope is set to 0;
//! - smoothing is `scipy.ndimage.uniform_filter(size=WIN_SIZE,
//!   mode='nearest')` on int16, which truncates after each axis pass
//!   (rows first). Verified cell-for-cell against `slope_smooth.tif`;
//! - class 4 is assigned wherever the *smoothed* slope exceeds SAT34, even on
//!   cells whose own slope is nodata.

use ndarray::{Array2, Axis, Zip};
use thiserror::Error;

use crate::grid::Grid;
use crate::terrain::OUTPUT_NODATA;

/// Nodata value of class rasters.
pub const CLASS_NODATA: i16 = -9999;

/// A raster of class codes.
pub type ClassGrid = Grid<i16>;

#[derive(Debug, Error, PartialEq)]
pub enum ClassifyError {
    #[error("classifier needs the `{0}` layer")]
    MissingLayer(&'static str),
    #[error("layer `{0}` is not aligned with the DEM grid")]
    Misaligned(&'static str),
    #[error("invalid parameter: {0}")]
    BadParam(String),
    #[error("post-processing failed: {0}")]
    PostProcess(String),
}

/// Inputs available to a classifier, all on one grid.
#[derive(Debug, Clone, Default)]
pub struct TerrainLayers<'a> {
    pub dem: Option<&'a Grid<f32>>,
    /// Slope in degrees, as produced by [`crate::terrain::slope_deg`].
    pub slope_deg: Option<&'a Grid<f32>>,
    /// Forest density in the units of the configured forest type.
    pub forest: Option<&'a Grid<f32>>,
    /// Flow-Py flow-path travel angle (degrees), e.g. AutoATES `FP_int16.tif`.
    pub flowpy_fp: Option<&'a Grid<f32>>,
    /// Flow-Py cell counts (AutoATES uses its overhead/cell-count raster).
    pub cell_count: Option<&'a Grid<f32>>,
    /// Potential release areas, 1 = release, 0 = not.
    pub pra: Option<&'a Grid<f32>>,
}

impl<'a> TerrainLayers<'a> {
    pub fn require(
        layer: Option<&'a Grid<f32>>,
        name: &'static str,
    ) -> Result<&'a Grid<f32>, ClassifyError> {
        layer.ok_or(ClassifyError::MissingLayer(name))
    }
}

/// Anything that turns terrain layers into a class raster.
pub trait Classifier {
    /// Short identifier stamped into provenance.
    fn name(&self) -> &'static str;

    fn classify(&self, layers: &TerrainLayers<'_>) -> Result<ClassGrid, ClassifyError>;
}

/// Slope-angle thresholds (degrees) and the smoothing window used for class 4.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SlopeThresholds {
    pub sat01: f64,
    pub sat12: f64,
    pub sat23: f64,
    pub sat34: f64,
    /// Odd window size of the mean filter applied before the class-4 test.
    pub win_size: usize,
}

impl SlopeThresholds {
    pub fn validate(&self) -> Result<(), ClassifyError> {
        let t = [self.sat01, self.sat12, self.sat23, self.sat34];
        if t.iter().any(|v| !v.is_finite()) || !t.windows(2).all(|w| w[0] <= w[1]) {
            return Err(ClassifyError::BadParam(format!(
                "slope thresholds must be finite and ascending, got {t:?}"
            )));
        }
        if self.win_size.is_multiple_of(2) {
            return Err(ClassifyError::BadParam(format!(
                "win_size must be odd, got {}",
                self.win_size
            )));
        }
        Ok(())
    }
}

/// Slope classes and the smoothed integer slope used for class 4.
#[derive(Debug, Clone)]
pub struct SlopeClasses {
    pub classes: ClassGrid,
    pub smoothed: Grid<i16>,
}

/// Slope truncated to whole degrees, as AutoATES does; nodata becomes
/// [`CLASS_NODATA`].
pub fn slope_int(slope: &Grid<f32>) -> Array2<i16> {
    slope.data.mapv(|v| {
        if slope.is_nodata(v) || v == OUTPUT_NODATA {
            CLASS_NODATA
        } else {
            v.trunc() as i16
        }
    })
}

/// Port of `scipy.ndimage.uniform_filter(a, size, mode='nearest')` for a
/// non-negative int16 array: a separable mean over rows then columns, with
/// edge replication and truncation to an integer after each pass.
pub fn uniform_filter_trunc(a: &Array2<i16>, size: usize) -> Array2<i16> {
    assert!(!size.is_multiple_of(2), "size must be odd");
    let half = (size / 2) as isize;
    let pass = |src: &Array2<i16>, axis: Axis| {
        let n = src.len_of(axis) as isize;
        let mut out = src.clone();
        for (mut o, s) in out.lanes_mut(axis).into_iter().zip(src.lanes(axis)) {
            for i in 0..n {
                let sum: i32 = (-half..=half)
                    .map(|d| i32::from(s[(i + d).clamp(0, n - 1) as usize]))
                    .sum();
                o[i as usize] = (sum / size as i32) as i16;
            }
        }
        out
    };
    pass(&pass(a, Axis(0)), Axis(1))
}

/// Faithful port of the AutoATES slope reclassification.
pub fn slope_classes(
    slope: &Grid<f32>,
    t: &SlopeThresholds,
) -> Result<SlopeClasses, ClassifyError> {
    t.validate()?;
    let s = slope_int(slope);
    let smoothed = uniform_filter_trunc(&s.mapv(|v| v.max(0)), t.win_size);
    let mut classes = s.clone();
    Zip::from(&mut classes)
        .and(&s)
        .and(&smoothed)
        .for_each(|c, &v, &sm| *c = slope_class_value(v, sm, t));
    let wrap = |data| Grid {
        data,
        transform: slope.transform,
        crs: slope.crs.clone(),
        nodata: Some(f64::from(CLASS_NODATA)),
    };
    Ok(SlopeClasses {
        classes: wrap(classes),
        smoothed: wrap(smoothed),
    })
}

/// The per-cell rules, applied in the same order as the numpy in-place
/// assignments so arbitrary thresholds behave identically.
fn slope_class_value(slope: i16, smoothed: i16, t: &SlopeThresholds) -> i16 {
    let mut v = slope;
    let f = |x: i16| f64::from(x);
    for (lo, hi, class) in [
        (0.0, t.sat01, 0),
        (t.sat01, t.sat12, 1),
        (t.sat12, t.sat23, 2),
        (t.sat23, 100.0, 3),
    ] {
        if lo < f(v) && f(v) <= hi {
            v = class;
        }
    }
    if t.sat34 < f(smoothed) && f(smoothed) <= 100.0 {
        v = 4;
    }
    v
}

/// Slope-only proxy: classes 0-4 from slope bands alone. This ignores
/// release areas, runout and forest, so it is **not** an ATES rating.
#[derive(Debug, Clone, Copy)]
pub struct SlopeBandClassifier {
    pub thresholds: SlopeThresholds,
}

impl Classifier for SlopeBandClassifier {
    fn name(&self) -> &'static str {
        "slope-band proxy (not ATES)"
    }

    fn classify(&self, layers: &TerrainLayers<'_>) -> Result<ClassGrid, ClassifyError> {
        let slope = TerrainLayers::require(layers.slope_deg, "slope_deg")?;
        Ok(slope_classes(slope, &self.thresholds)?.classes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Crs, GeoTransform};
    use ndarray::array;

    /// AutoATES v2.0 defaults (AutoATES_classifier.py @ 3afcb49, lines 23-35).
    const AUTOATES: SlopeThresholds = SlopeThresholds {
        sat01: 15.0,
        sat12: 18.0,
        sat23: 28.0,
        sat34: 39.0,
        win_size: 3,
    };

    fn grid(data: Array2<f32>) -> Grid<f32> {
        Grid::new(
            data,
            GeoTransform::north_up(0.0, 0.0, 10.0, 10.0),
            Crs::Epsg(32611),
            Some(-9999.0),
        )
        .unwrap()
    }

    #[test]
    fn uniform_filter_matches_hand_computation() {
        let a = array![[0_i16, 3, 6], [9, 12, 15], [18, 21, 24]];
        // Rows first: column 0 becomes [trunc(9/3), trunc(27/3), trunc(45/3)].
        let f = uniform_filter_trunc(&a, 3);
        // Then columns with edge replication.
        assert_eq!(f[[0, 0]], ((3 + 3 + 6) / 3) as i16);
        assert_eq!(f[[1, 1]], 12);
        assert_eq!(f[[2, 2]], 20);
        assert_eq!(uniform_filter_trunc(&a, 1), a);
    }

    #[test]
    fn uniform_filter_truncates_between_passes() {
        // At the centre the exact 3x3 mean is 18/9 = 2. Column sums are
        // 5, 5, 8, which the first pass truncates to 1, 1, 2, so the second
        // pass gives trunc(4/3) = 1, which is what scipy returns on int16.
        let a = array![[2_i16, 2, 3], [2, 2, 3], [1, 1, 2]];
        assert_eq!(uniform_filter_trunc(&a, 3)[[1, 1]], 1);
    }

    #[test]
    fn slope_bands_follow_autoates_thresholds() {
        let s = grid(array![
            [0.0, 10.0, 15.0, 15.9],
            [16.0, 18.0, 18.5, 28.0],
            [29.0, 38.0, 39.0, 60.0]
        ]);
        let c = slope_classes(
            &s,
            &SlopeThresholds {
                win_size: 1,
                ..AUTOATES
            },
        )
        .unwrap();
        // 15.9 truncates to 15 -> class 0; 18.5 -> 18 -> class 1.
        assert_eq!(
            c.classes.data,
            array![[0_i16, 0, 0, 0], [1, 1, 1, 2], [3, 3, 3, 4]]
        );
    }

    #[test]
    fn smoothed_steep_neighbourhood_marks_class4_even_on_nodata() {
        let mut d = Array2::from_elem((3, 3), 45.0_f32);
        d[[1, 1]] = -9999.0;
        let c = slope_classes(&grid(d), &AUTOATES).unwrap();
        // Centre: (8*45 + 0)/9 via two truncating passes = 40 > 39.
        assert_eq!(c.smoothed.data[[1, 1]], 40);
        assert_eq!(c.classes.data[[1, 1]], 4);
    }

    #[test]
    fn nodata_stays_nodata_when_not_steep() {
        let mut d = Array2::from_elem((3, 3), 10.0_f32);
        d[[1, 1]] = f32::NAN;
        let c = slope_classes(&grid(d), &AUTOATES).unwrap();
        assert_eq!(c.classes.data[[1, 1]], CLASS_NODATA);
        assert_eq!(c.classes.data[[0, 0]], 0);
    }

    #[test]
    fn rejects_bad_thresholds() {
        let bad = SlopeThresholds {
            sat12: 10.0,
            ..AUTOATES
        };
        assert!(bad.validate().is_err());
        let even = SlopeThresholds {
            win_size: 2,
            ..AUTOATES
        };
        assert!(even.validate().is_err());
    }

    #[test]
    fn classifier_requires_slope() {
        let c = SlopeBandClassifier {
            thresholds: AUTOATES,
        };
        assert_eq!(
            c.classify(&TerrainLayers::default()).unwrap_err(),
            ClassifyError::MissingLayer("slope_deg")
        );
    }
}
