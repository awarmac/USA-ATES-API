//! AutoATES v2.0 classification rules.
//!
//! A faithful port of `AutoATES_classifier.py` (AutoATES-v2.0 @ 3afcb49),
//! lines 132-350. Each step is a separate function that reproduces one of
//! the intermediate rasters AutoATES writes, so each is golden-tested:
//!
//! | function | AutoATES output |
//! |---|---|
//! | [`crate::classify::slope_classes`] | `slope.tif`, `slope_smooth.tif` |
//! | [`flowpy_classes`] | `flowpy.tif` |
//! | [`cellcount_classes`] | `cellcount_reclass.tif` |
//! | [`forest_codes`] | `forest_reclass.tif` |
//! | [`pra_codes`] | `SZ_reclass.tif` |
//! | [`merge_max`] | `merge_new.tif` |
//! | [`combine_lookup`] | `merge_all.tif` |
//! | [`classify`] (with fill) | `ates_gen.tif` |
//!
//! Each step applies the numpy in-place assignments in order, per cell,
//! which is equivalent because every assignment is elementwise. The quirks
//! below are deliberate, because AutoATES behaves this way:
//!
//! - AAT1 is not used: every flow-path value in [0, 90) becomes class 1
//!   (upstream comment: "we are not using Non-Avalanche Terrain").
//! - Cell-count nodata (-9999) becomes 0, then class 1.
//! - Sums missing from the lookup table pass through unchanged, and
//!   negative sums become 0.
//! - Cluster labels are cast to int16, so label ids that are multiples of
//!   65536 count as "fill".
//! - In `ates_gen.tif`, class 0 is written as nodata. That is reproduced
//!   only by [`OutputMode::OracleParity`].

use ndarray::{Array2, Zip};

use crate::classify::{
    CLASS_NODATA, ClassGrid, Classifier, ClassifyError, SlopeThresholds, TerrainLayers,
    slope_classes,
};
use crate::grid::Grid;

/// Forest density measures supported by AutoATES (`forest_type`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForestType {
    /// Basal area per hectare (m^2/ha).
    Bav,
    /// Percent canopy cover.
    Pcc,
    /// Stems per hectare.
    Stems,
    /// Sentinel-2 canopy cover (%).
    Sen2ccc,
}

/// Forest density thresholds TREE1 < TREE2 < TREE3 (open, sparse, dense).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ForestThresholds {
    pub tree1: f64,
    pub tree2: f64,
    pub tree3: f64,
}

/// All AutoATES classifier parameters.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AutoAtesParams {
    pub slope: SlopeThresholds,
    pub aat1: f64,
    pub aat2: f64,
    pub aat3: f64,
    pub forest: ForestThresholds,
    pub cc1: f64,
    pub cc2: f64,
    /// Minimum cluster area in m^2; smaller clusters are refilled from
    /// their surroundings.
    pub isl_size_m2: f64,
}

impl AutoAtesParams {
    pub fn validate(&self) -> Result<(), ClassifyError> {
        self.slope.validate()?;
        let asc = |name: &str, v: &[f64]| {
            if v.iter().all(|x| x.is_finite()) && v.windows(2).all(|w| w[0] <= w[1]) {
                Ok(())
            } else {
                Err(ClassifyError::BadParam(format!(
                    "{name} must be finite and ascending: {v:?}"
                )))
            }
        };
        asc("AAT", &[self.aat1, self.aat2, self.aat3])?;
        asc(
            "TREE",
            &[self.forest.tree1, self.forest.tree2, self.forest.tree3],
        )?;
        asc("CC", &[self.cc1, self.cc2])?;
        if !(self.isl_size_m2.is_finite() && self.isl_size_m2 >= 0.0) {
            return Err(ClassifyError::BadParam(format!(
                "isl_size_m2 = {}",
                self.isl_size_m2
            )));
        }
        Ok(())
    }
}

/// How to encode the final raster.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OutputMode {
    /// Classes 0-4; nodata where the DEM or forest is nodata.
    #[default]
    Product,
    /// Exactly AutoATES `ates_gen.tif`: class 0 written as -9999.
    OracleParity,
}

/// Interpolates masked cells from their neighbours (AutoATES calls GDAL's
/// `GDALFillNodata` via `rasterio.fill.fillnodata`). It lives behind a trait
/// so this crate stays free of I/O.
pub trait FillNodata {
    /// Replace cells where `mask == 0` by inverse-distance interpolation of
    /// cells where `mask != 0`, searching up to `max_search_px` pixels.
    /// Cells with nothing in range keep their value. No smoothing.
    fn fill(
        &self,
        values: &Grid<i16>,
        mask: &Array2<u8>,
        max_search_px: f64,
    ) -> Result<Array2<i16>, String>;
}

/// Every AutoATES intermediate plus the result.
#[derive(Debug, Clone)]
pub struct AutoAtesLayers {
    pub slope_class: ClassGrid,
    pub slope_smooth: Grid<i16>,
    pub flowpy: Array2<i16>,
    pub cellcount: Array2<i16>,
    pub forest: Array2<i16>,
    pub pra: Array2<i16>,
    pub merge_new: Array2<i16>,
    pub merge_all: Array2<i16>,
    /// Cells refilled by the small-cluster cleanup (`mask == 0`).
    pub cleanup_mask: Array2<u8>,
    pub ates: ClassGrid,
}

/// Flow-path travel angle to runout classes 1-3 (`flowpy.tif`).
pub fn flowpy_classes(fp: &Array2<i16>, p: &AutoAtesParams) -> Array2<i16> {
    fp.mapv(|v| {
        let f = f64::from(v);
        let c18 = if (0.0..90.0).contains(&f) { 1 } else { v };
        let mut c25 = v;
        if f < p.aat2 {
            c25 = 0;
        }
        if f64::from(c25) >= p.aat2 && f64::from(c25) < 90.0 {
            c25 = 2;
        }
        let mut c38 = v;
        if f < p.aat3 {
            c38 = 0;
        }
        if f64::from(c38) >= p.aat3 && f64::from(c38) < 90.0 {
            c38 = 3;
        }
        c18.max(c25).max(c38)
    })
}

/// Flow-Py cell counts to classes 1-3 (`cellcount_reclass.tif`).
pub fn cellcount_classes(cc: &Array2<i16>, p: &AutoAtesParams) -> Array2<i16> {
    cc.mapv(|v| {
        let mut v = v;
        if v == -9999 {
            v = 0;
        }
        let f = |x: i16| f64::from(x);
        if 0.0 <= f(v) && f(v) <= p.cc1 {
            v = 1;
        }
        if p.cc1 < f(v) && f(v) <= p.cc2 {
            v = 2;
        }
        if p.cc2 < f(v) && f(v) <= 20000.0 {
            v = 3;
        }
        v
    })
}

/// Forest density to codes -1 (nodata), 10 (open), 20 (sparse), 30 (dense)
/// and 40 (very dense) (`forest_reclass.tif`).
pub fn forest_codes(forest: &Array2<f64>, t: &ForestThresholds) -> Array2<i16> {
    forest.mapv(|v| {
        let open = if v > t.tree1 {
            -1.0
        } else if v >= 0.0 {
            10.0
        } else {
            v
        };
        let sparse = if v > t.tree1 && v <= t.tree2 {
            20.0
        } else {
            -1.0
        };
        let dense = if v > t.tree2 && v <= t.tree3 {
            30.0
        } else {
            -1.0
        };
        let vdense = if v < t.tree3 { -1.0 } else { 40.0 };
        open.max(sparse).max(dense).max(vdense) as i16
    })
}

/// Release areas to codes 0 / 100 (`SZ_reclass.tif`); other values pass
/// through.
pub fn pra_codes(pra: &Array2<i16>) -> Array2<i16> {
    pra.mapv(|v| if v == 1 { 100 } else { v })
}

/// Cell-wise maximum of slope, runout and cell-count classes
/// (`merge_new.tif`).
pub fn merge_max(
    slope: &Array2<i16>,
    flowpy: &Array2<i16>,
    cellcount: &Array2<i16>,
) -> Array2<i16> {
    let mut out = slope.clone();
    Zip::from(&mut out)
        .and(flowpy)
        .and(cellcount)
        .for_each(|o, &f, &c| *o = (*o).max(f).max(c));
    out
}

/// AutoATES lookup: `merge_new + forest_code + pra_code` to class.
/// Index = sum; tens = forest code (1..4) and PRA (+100), units = class.
const LOOKUP: [(i32, i16); 40] = [
    (10, 0),
    (11, 1),
    (12, 2),
    (13, 3),
    (14, 4),
    (20, 0),
    (21, 1),
    (22, 1),
    (23, 2),
    (24, 3),
    (30, 0),
    (31, 1),
    (32, 1),
    (33, 1),
    (34, 3),
    (40, 0),
    (41, 1),
    (42, 1),
    (43, 1),
    (44, 2),
    (110, 0),
    (111, 1),
    (112, 2),
    (113, 3),
    (114, 4),
    (120, 0),
    (121, 1),
    (122, 1),
    (123, 2),
    (124, 3),
    (130, 0),
    (131, 1),
    (132, 1),
    (133, 2),
    (134, 3),
    (140, 0),
    (141, 1),
    (142, 1),
    (143, 2),
    (144, 2),
];

/// Forest and release-area adjustment (`merge_all.tif`).
pub fn combine_lookup(
    merge_new: &Array2<i16>,
    forest: &Array2<i16>,
    pra: &Array2<i16>,
) -> Array2<i16> {
    let mut out = merge_new.clone();
    Zip::from(&mut out)
        .and(forest)
        .and(pra)
        .for_each(|o, &f, &p| {
            let sum = i32::from(*o) + i32::from(f) + i32::from(p);
            let v = LOOKUP
                .iter()
                .find(|(k, _)| *k == sum)
                .map_or(sum, |&(_, c)| i32::from(c));
            *o = v.max(0) as i16;
        });
    out
}

/// Label 8-connected regions of equal value, like
/// `skimage.measure.label(a, connectivity=2)`. Value 0 is background
/// (label 0). Labels start at 1 in raster scan order of first appearance.
pub fn label_same_value_8conn(a: &Array2<i16>) -> (Array2<u32>, u32) {
    let (rows, cols) = a.dim();
    let mut labels = Array2::<u32>::zeros((rows, cols));
    let mut next = 0;
    let mut stack = Vec::new();
    for r in 0..rows {
        for c in 0..cols {
            let v = a[[r, c]];
            if v == 0 || labels[[r, c]] != 0 {
                continue;
            }
            next += 1;
            labels[[r, c]] = next;
            stack.push((r, c));
            while let Some((cr, cc)) = stack.pop() {
                for dr in -1_isize..=1 {
                    for dc in -1_isize..=1 {
                        let (nr, nc) = (cr as isize + dr, cc as isize + dc);
                        if nr < 0 || nc < 0 || nr as usize >= rows || nc as usize >= cols {
                            continue;
                        }
                        let (nr, nc) = (nr as usize, nc as usize);
                        if labels[[nr, nc]] == 0 && a[[nr, nc]] == v {
                            labels[[nr, nc]] = next;
                            stack.push((nr, nc));
                        }
                    }
                }
            }
        }
    }
    (labels, next)
}

/// Cells to keep (1) or refill (0): clusters smaller than `num_cells` are
/// refilled. Mirrors AutoATES, including the int16 cast of label ids.
pub fn small_cluster_mask(labels: &Array2<u32>, count: u32, num_cells: f64) -> Array2<u8> {
    let mut sizes = vec![0_u64; count as usize + 1];
    for &l in labels {
        sizes[l as usize] += 1;
    }
    labels.mapv(|l| {
        let kept = l != 0 && (sizes[l as usize] as f64) >= num_cells;
        // `lab.astype('int16')`: ids that wrap to 0 become "fill".
        u8::from(kept && (l as i32 as i16) != 0)
    })
}

/// Number of cells in `isl_size_m2`, rounded like `np.around` (half to even).
pub fn cluster_min_cells(isl_size_m2: f64, pixel_w: f64, pixel_h: f64) -> f64 {
    (isl_size_m2 / (pixel_w * pixel_h)).round_ties_even()
}

/// Run the whole AutoATES classification on aligned layers.
pub fn classify(
    layers: &TerrainLayers<'_>,
    p: &AutoAtesParams,
    filler: &dyn FillNodata,
    mode: OutputMode,
) -> Result<AutoAtesLayers, ClassifyError> {
    p.validate()?;
    let dem = TerrainLayers::require(layers.dem, "dem")?;
    let slope = TerrainLayers::require(layers.slope_deg, "slope_deg")?;
    let forest = TerrainLayers::require(layers.forest, "forest")?;
    let fp = TerrainLayers::require(layers.flowpy_fp, "flowpy_fp")?;
    let cc = TerrainLayers::require(layers.cell_count, "cell_count")?;
    let pra = TerrainLayers::require(layers.pra, "pra")?;
    for (g, name) in [
        (slope, "slope_deg"),
        (forest, "forest"),
        (fp, "flowpy_fp"),
        (cc, "cell_count"),
        (pra, "pra"),
    ] {
        if g.data.dim() != dem.data.dim() || g.transform != dem.transform {
            return Err(ClassifyError::Misaligned(name));
        }
    }
    // AutoATES reads these as int16 (`astype('int16')` truncates).
    let as_i16 = |g: &Grid<f32>| {
        g.data
            .mapv(|v| if v.is_nan() { -9999 } else { v.trunc() as i16 })
    };

    let sc = slope_classes(slope, &p.slope)?;
    let flowpy = flowpy_classes(&as_i16(fp), p);
    let cellcount = cellcount_classes(&as_i16(cc), p);
    let forest_c = forest_codes(&forest.data.mapv(f64::from), &p.forest);
    let pra_c = pra_codes(&as_i16(pra));
    let merge_new = merge_max(&sc.classes.data, &flowpy, &cellcount);
    let merge_all = combine_lookup(&merge_new, &forest_c, &pra_c);

    // Small-cluster cleanup on merge_all + 1 (so class 0 is not background).
    let shifted = merge_all.mapv(|v| v.saturating_add(1));
    let (labels, count) = label_same_value_8conn(&shifted);
    let num_cells = cluster_min_cells(
        p.isl_size_m2,
        dem.transform.ew_res(),
        dem.transform.ns_res(),
    );
    let mask = small_cluster_mask(&labels, count, num_cells);
    let shifted_grid = Grid {
        data: shifted,
        transform: dem.transform,
        crs: dem.crs.clone(),
        nodata: None,
    };
    let filled = filler
        .fill(&shifted_grid, &mask, num_cells / 4.0)
        .map_err(ClassifyError::PostProcess)?;

    let mut ates = filled.mapv(|v| v - 1);
    match mode {
        OutputMode::OracleParity => ates.mapv_inplace(|v| if v == 0 { CLASS_NODATA } else { v }),
        OutputMode::Product => {
            Zip::from(&mut ates)
                .and(&dem.data)
                .and(&forest.data)
                .for_each(|a, &d, &f| {
                    if dem.is_nodata(d) || forest.is_nodata(f) {
                        *a = CLASS_NODATA;
                    }
                });
        }
    }

    Ok(AutoAtesLayers {
        slope_class: sc.classes,
        slope_smooth: sc.smoothed,
        flowpy,
        cellcount,
        forest: forest_c,
        pra: pra_c,
        merge_new,
        merge_all,
        cleanup_mask: mask,
        ates: Grid {
            data: ates,
            transform: dem.transform,
            crs: dem.crs.clone(),
            nodata: Some(f64::from(CLASS_NODATA)),
        },
    })
}

/// AutoATES rules as a [`Classifier`].
pub struct AutoAtesClassifier<'f> {
    pub params: AutoAtesParams,
    pub filler: &'f dyn FillNodata,
    pub mode: OutputMode,
}

impl Classifier for AutoAtesClassifier<'_> {
    fn name(&self) -> &'static str {
        "AutoATES v2.0 rules"
    }

    fn classify(&self, layers: &TerrainLayers<'_>) -> Result<ClassGrid, ClassifyError> {
        Ok(classify(layers, &self.params, self.filler, self.mode)?.ates)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terrain::slope_deg;
    use crate::{Crs, GeoTransform};
    use ndarray::array;

    /// AutoATES_classifier.py @ 3afcb49, lines 23-86, forest_type 'bav'.
    pub(crate) const AUTOATES_BAV: AutoAtesParams = AutoAtesParams {
        slope: SlopeThresholds {
            sat01: 15.0,
            sat12: 18.0,
            sat23: 28.0,
            sat34: 39.0,
            win_size: 3,
        },
        aat1: 18.0,
        aat2: 24.0,
        aat3: 33.0,
        forest: ForestThresholds {
            tree1: 10.0,
            tree2: 20.0,
            tree3: 25.0,
        },
        cc1: 5.0,
        cc2: 40.0,
        isl_size_m2: 30000.0,
    };

    /// Leaves values unchanged; for tests that do not exercise the fill.
    struct NoFill;
    impl FillNodata for NoFill {
        fn fill(&self, v: &Grid<i16>, _: &Array2<u8>, _: f64) -> Result<Array2<i16>, String> {
            Ok(v.data.clone())
        }
    }

    #[test]
    fn flowpy_thresholds() {
        let fp = array![[0_i16, 17, 23, 24, 32, 33, 89, 90, -9999]];
        assert_eq!(
            flowpy_classes(&fp, &AUTOATES_BAV),
            array![[1_i16, 1, 1, 2, 2, 3, 3, 90, 0]]
        );
    }

    #[test]
    fn cellcount_thresholds() {
        let cc = array![[-9999_i16, 0, 5, 6, 40, 41, 20000, 20001]];
        assert_eq!(
            cellcount_classes(&cc, &AUTOATES_BAV),
            array![[1_i16, 1, 1, 2, 2, 3, 3, 20001]]
        );
    }

    #[test]
    fn forest_bav_codes() {
        let f = array![[-9999.0, 0.0, 10.0, 11.0, 20.0, 21.0, 25.0, 26.0]];
        assert_eq!(
            forest_codes(&f, &AUTOATES_BAV.forest),
            // 25 is both "dense" (<= TREE3) and "very dense" (>= TREE3); max wins.
            array![[-1_i16, 10, 10, 20, 20, 30, 40, 40]]
        );
    }

    #[test]
    fn lookup_and_passthrough() {
        let merge = array![[1_i16, 4, 3, 1, 2]];
        let forest = array![[10_i16, 40, 20, -1, -1]];
        let pra = array![[0_i16, 100, 100, 0, 100]];
        // 11->1, 144->2, 123->2, 0 passes through, 101 is unlisted.
        assert_eq!(
            combine_lookup(&merge, &forest, &pra),
            array![[1_i16, 2, 2, 0, 101]]
        );
    }

    #[test]
    fn labels_are_same_value_8_connected() {
        let a = array![[1_i16, 1, 2], [2, 1, 2], [0, 0, 1]];
        let (l, n) = label_same_value_8conn(&a);
        assert_eq!(n, 3);
        // The 1s at (0,0),(0,1),(1,1),(2,2) are one region via the diagonal.
        assert_eq!(l[[2, 2]], l[[0, 0]]);
        // The 2s at (0,2),(1,2) are one region; the 2 at (1,0) touches no 2.
        assert_eq!(l[[0, 2]], l[[1, 2]]);
        assert_ne!(l[[1, 0]], l[[0, 2]]);
        assert_eq!(l[[2, 0]], 0, "zero is background");
    }

    #[test]
    fn small_clusters_are_masked() {
        let a = array![[1_i16, 1, 1], [1, 2, 1], [1, 1, 1]];
        let (l, n) = label_same_value_8conn(&a);
        let m = small_cluster_mask(&l, n, 2.0);
        assert_eq!(m[[1, 1]], 0);
        assert_eq!(m[[0, 0]], 1);
    }

    #[test]
    fn min_cells_rounds_half_to_even() {
        assert_eq!(
            cluster_min_cells(30000.0, 25.741_554_488_994_527, 25.786_796_182_763_478),
            45.0
        );
        assert_eq!(cluster_min_cells(2.5, 1.0, 1.0), 2.0);
        assert_eq!(cluster_min_cells(3.5, 1.0, 1.0), 4.0);
    }

    fn g(data: Array2<f32>) -> Grid<f32> {
        Grid::new(
            data,
            GeoTransform::north_up(0.0, 50.0, 10.0, 10.0),
            Crs::Epsg(32611),
            Some(-9999.0),
        )
        .unwrap()
    }

    #[test]
    fn end_to_end_on_tiny_grid() {
        let dem = g(Array2::from_shape_fn((5, 5), |(_, c)| {
            1000.0 + 10.0 * c as f32
        }));
        let slope = slope_deg(&dem, true);
        let mut forest = g(Array2::from_elem((5, 5), 0.0));
        forest.data[[0, 0]] = -9999.0;
        let zeros = g(Array2::zeros((5, 5)));
        let layers = TerrainLayers {
            dem: Some(&dem),
            slope_deg: Some(&slope),
            forest: Some(&forest),
            flowpy_fp: Some(&zeros),
            cell_count: Some(&zeros),
            pra: Some(&zeros),
        };
        let p = AutoAtesParams {
            isl_size_m2: 0.0,
            ..AUTOATES_BAV
        };
        let out = classify(&layers, &p, &NoFill, OutputMode::Product).unwrap();
        // 45 degree slope -> class 3 (and the smoothed slope 45 > 39 -> 4),
        // flowpy/cellcount 1, open forest (10): 14 -> 4.
        assert_eq!(out.ates.data[[2, 2]], 4);
        assert_eq!(
            out.ates.data[[0, 0]],
            CLASS_NODATA,
            "forest nodata masked in product mode"
        );
        let parity = classify(&layers, &p, &NoFill, OutputMode::OracleParity).unwrap();
        // Forest nodata gives code -1, so the sum is merge_new - 1, which is
        // not in the lookup table and passes through unchanged. (The corner
        // slope is atan(0.5) ~ 26.6 degrees: class 2, so the sum is 1.)
        assert_eq!(parity.merge_new[[0, 0]], 2);
        assert_eq!(parity.merge_all[[0, 0]], 1);
    }

    #[test]
    fn misaligned_layers_are_rejected() {
        let dem = g(Array2::zeros((5, 5)));
        let small = g(Array2::zeros((4, 5)));
        let layers = TerrainLayers {
            dem: Some(&dem),
            slope_deg: Some(&dem),
            forest: Some(&dem),
            flowpy_fp: Some(&small),
            cell_count: Some(&dem),
            pra: Some(&dem),
        };
        let err = classify(&layers, &AUTOATES_BAV, &NoFill, OutputMode::Product).unwrap_err();
        assert_eq!(err, ClassifyError::Misaligned("flowpy_fp"));
    }
}
