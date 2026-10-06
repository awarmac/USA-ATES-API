//! Overhead exposure: the classifier's `cell_count` input, from Flow-Py.
//!
//! Toft et al. (2024), Sect. 2.3.2: "We utilize the cell count and z_delta
//! layer by scaling the two layers from 0–100 and taking their average
//! value, which represents the overhead exposure layer." The paper doesn't
//! give the scaling, and the AutoATES repository has no code for this step.
//!
//! The formula below was reconstructed from the only published run with
//! all three rasters, the Bow Summit run in the Sykes et al. (2023) OSF
//! archive (`ALOS30m_final/flowpy/`). It reproduces that run's
//! `Overhead.tif` on all 52,756 cells. The same `Overhead.tif` is the
//! `cell_count` input in AutoATES's own `test-data/Bow Summit`.
//!
//! ```text
//! cc_scaled = 100 * ln(cell_counts) / ln(max cell_counts)   (0 where cell_counts <= 0)
//! zd_scaled = 100 * z_delta / max_z                         (max_z: Flow-Py's max_z_delta)
//! overhead  = trunc((cc_scaled + zd_scaled) / 2)            (int16)
//! ```
//!
//! The cell counts are normalised by their maximum over the whole raster,
//! so the result depends on the extent processed. Region builds therefore
//! normalise over the full region window, as AutoATES does over its input
//! raster. `cc_max` lets a caller fix the reference instead.

use ndarray::{Array2, Zip};

use crate::classify::ClassifyError;

/// The default normalisation reference: the largest finite cell count.
pub fn max_cell_count(cell_counts: &Array2<f32>) -> f64 {
    cell_counts
        .iter()
        .copied()
        .filter(|v| v.is_finite())
        .fold(0.0_f32, f32::max)
        .into()
}

/// Overhead exposure 0-100 from Flow-Py `cell_counts` and `z_delta`.
/// `cc_max` defaults to the largest cell count in `cell_counts`.
pub fn overhead(
    cell_counts: &Array2<f32>,
    z_delta: &Array2<f32>,
    max_z: f64,
    cc_max: Option<f64>,
) -> Result<Array2<i16>, ClassifyError> {
    if cell_counts.dim() != z_delta.dim() {
        return Err(ClassifyError::Misaligned("z_delta"));
    }
    if max_z.is_nan() || max_z <= 0.0 {
        return Err(ClassifyError::BadParam("max_z must be positive".into()));
    }
    let cc_max = cc_max.unwrap_or_else(|| max_cell_count(cell_counts));
    // With at most one path anywhere, ln(cc_max) is 0: the counts carry no
    // ranking, so they contribute 0.
    let ln_max = if cc_max > 1.0 {
        cc_max.ln()
    } else {
        f64::INFINITY
    };
    let mut out = Array2::zeros(cell_counts.dim());
    Zip::from(&mut out)
        .and(cell_counts)
        .and(z_delta)
        .for_each(|o, &cc, &zd| {
            let cc = f64::from(cc);
            let cc_s = if cc > 0.0 {
                cc.ln() / ln_max * 100.0
            } else {
                0.0
            };
            let zd_s = f64::from(zd) / max_z * 100.0;
            *o = ((cc_s + zd_s) / 2.0) as i16;
        });
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ndarray::array;

    #[test]
    fn scales_and_averages() {
        let cc = array![[0.0_f32, 1.0, 10.0, 100.0]];
        let zd = array![[0.0_f32, 27.0, 135.0, 270.0]];
        let o = overhead(&cc, &zd, 270.0, None).unwrap();
        // cc 10 of max 100: ln 10 / ln 100 = 50; zd 135 / 270 = 50.
        assert_eq!(o, array![[0, 5, 50, 100]]);
    }

    #[test]
    fn single_path_counts_contribute_nothing() {
        let o = overhead(&array![[1.0_f32]], &array![[54.0_f32]], 270.0, None).unwrap();
        assert_eq!(o[[0, 0]], 10);
    }

    #[test]
    fn fixed_reference_and_errors() {
        let cc = array![[10.0_f32]];
        let zd = array![[0.0_f32]];
        assert_eq!(overhead(&cc, &zd, 270.0, Some(100.0)).unwrap()[[0, 0]], 25);
        assert!(overhead(&cc, &array![[0.0_f32, 0.0]], 270.0, None).is_err());
        assert!(overhead(&cc, &zd, 0.0, None).is_err());
    }
}
