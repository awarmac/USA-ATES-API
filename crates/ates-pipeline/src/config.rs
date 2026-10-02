//! TOML configuration with named region presets.
//!
//! A config file has a `[defaults]` table and optional `[regions.<name>]`
//! tables with the same layout; a region is deep-merged over the defaults.
//! Values that are not yet sourced from the literature are left out of the
//! file (marked `TODO` in comments) and surface as [`ConfigError::Todo`]
//! when a run needs them.

use std::path::Path;

use ates_core::autoates::{AutoAtesParams, ForestThresholds, ForestType};
use ates_core::classify::SlopeThresholds;
use ates_core::flowpy::FlowPyParams;
use ates_core::pra::{Cauchy, PraParams};
use ates_io::Resampling;
use ates_io::provenance::fnv1a64_hex;
use serde::Deserialize;
use thiserror::Error;
use toml::{Table, Value};

pub const SCHEMA_VERSION: i64 = 1;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("reading {path}: {source}")]
    Read {
        path: String,
        source: std::io::Error,
    },
    #[error("parsing {source_name}: {message}")]
    Parse {
        source_name: String,
        message: String,
    },
    #[error("unsupported schema_version {0} (expected {SCHEMA_VERSION})")]
    Schema(i64),
    #[error("unknown region '{name}' (available: {available})")]
    UnknownRegion { name: String, available: String },
    #[error(
        "`{0}` is not set; it is still a TODO in the config. Set it in the config or on the command line."
    )]
    Todo(&'static str),
}

/// Resolved parameters for one run.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Params {
    #[serde(default)]
    pub dem: DemParams,
    #[serde(default)]
    pub prep: PrepParams,
    #[serde(default)]
    pub terrain: TerrainParams,
    #[serde(default)]
    pub classify: ClassifyParams,
    #[serde(default)]
    pub pra: PraConfig,
    #[serde(default)]
    pub flowpy: FlowPyConfig,
    #[serde(default)]
    pub site: SiteParams,
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DemParams {
    /// Analysis cell size in metres; `None` keeps a projected source's resolution.
    pub target_res_m: Option<f64>,
    #[serde(default)]
    pub resampling: Resampling,
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrepParams {
    /// Margin around a request, in metres, so upslope release areas and
    /// their runout are inside the processed window.
    pub pad_m: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerrainParams {
    /// Mirror of `gdaldem -compute_edges`.
    #[serde(default)]
    pub compute_edges: bool,
}

/// Classifier parameters. Every value must cite its source in the config
/// file; unset values are TODOs and fail when a classifier needs them.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClassifyParams {
    pub sat01: Option<f64>,
    pub sat12: Option<f64>,
    pub sat23: Option<f64>,
    pub sat34: Option<f64>,
    pub win_size: Option<usize>,
    pub aat1: Option<f64>,
    pub aat2: Option<f64>,
    pub aat3: Option<f64>,
    pub cc1: Option<f64>,
    pub cc2: Option<f64>,
    pub isl_size_m2: Option<f64>,
    /// Which forest density measure the forest raster holds.
    pub forest_type: Option<ForestTypeName>,
    /// TREE1/2/3 per forest type.
    #[serde(default)]
    pub forest: ForestTables,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ForestTypeName {
    Bav,
    Pcc,
    Stems,
    Sen2ccc,
}

impl From<ForestTypeName> for ForestType {
    fn from(f: ForestTypeName) -> Self {
        match f {
            ForestTypeName::Bav => ForestType::Bav,
            ForestTypeName::Pcc => ForestType::Pcc,
            ForestTypeName::Stems => ForestType::Stems,
            ForestTypeName::Sen2ccc => ForestType::Sen2ccc,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForestTables {
    pub bav: Option<[f64; 3]>,
    pub pcc: Option<[f64; 3]>,
    pub stems: Option<[f64; 3]>,
    pub sen2ccc: Option<[f64; 3]>,
}

/// Potential-release-area model parameters. Every value must cite its
/// source in the config file. Cauchy functions are `[a, b, c]`.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PraConfig {
    /// Windshelter radius in metres, rounded to whole cells at run time.
    pub windshelter_radius_m: Option<f64>,
    pub windshelter_prob: Option<f64>,
    pub wind_dir_deg: Option<f64>,
    pub wind_tol_deg: Option<f64>,
    /// Cut-off for the binary PRA (0-1).
    pub threshold: Option<f64>,
    /// Release areas with at most this many cells are removed.
    pub sieve_cells: Option<usize>,
    pub slope_cauchy: Option<[f32; 3]>,
    pub windshelter_cauchy: Option<[f32; 3]>,
    #[serde(default)]
    pub forest_cauchy: ForestCauchy,
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForestCauchy {
    pub bav: Option<[f32; 3]>,
    pub pcc: Option<[f32; 3]>,
    pub stems: Option<[f32; 3]>,
    pub sen2ccc: Option<[f32; 3]>,
}

/// Flow-Py runout parameters. Every value must cite its source in the
/// config file.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FlowPyConfig {
    pub alpha_deg: Option<f64>,
    pub exponent: Option<i32>,
    pub flux_threshold: Option<f64>,
    pub max_z_delta: Option<f64>,
    /// How region builds derive Flow-Py's 0-1 forest layer.
    #[serde(default)]
    pub forest: FlowPyForest,
}

/// Flow-Py's forest layer in region builds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FlowPyForest {
    /// No forest effect on runout.
    #[default]
    None,
    /// Percent canopy cover / 100, clamped to 0-1; nodata becomes 0.
    PccFraction,
}

/// Descriptive information about a region preset.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SiteParams {
    /// west, south, east, north in WGS 84 degrees.
    pub bbox: Option<[f64; 4]>,
    pub dem_source: Option<String>,
    pub forest_source: Option<String>,
    /// CAIC forecast zone / area id, for forecast context (later milestone).
    pub caic_zone: Option<String>,
    /// DEM to read for region builds: any GDAL path, including
    /// `/vsicurl/https://...` for a remote Cloud-Optimized GeoTIFF.
    pub dem_path: Option<String>,
    /// ArcGIS ImageServer URL of the forest raster for region builds.
    pub forest_service: Option<String>,
    /// `where` clause selecting the forest raster (e.g. a year).
    pub forest_where: Option<String>,
    /// Forest pixel values that mean no data.
    #[serde(default)]
    pub forest_invalid: Vec<u8>,
}

impl Params {
    pub fn pad_m(&self) -> Result<f64, ConfigError> {
        self.prep.pad_m.ok_or(ConfigError::Todo("prep.pad_m"))
    }

    pub fn slope_thresholds(&self) -> Result<SlopeThresholds, ConfigError> {
        let c = &self.classify;
        Ok(SlopeThresholds {
            sat01: c.sat01.ok_or(ConfigError::Todo("classify.sat01"))?,
            sat12: c.sat12.ok_or(ConfigError::Todo("classify.sat12"))?,
            sat23: c.sat23.ok_or(ConfigError::Todo("classify.sat23"))?,
            sat34: c.sat34.ok_or(ConfigError::Todo("classify.sat34"))?,
            win_size: c.win_size.ok_or(ConfigError::Todo("classify.win_size"))?,
        })
    }

    pub fn forest_type(&self) -> Result<ForestTypeName, ConfigError> {
        self.classify
            .forest_type
            .ok_or(ConfigError::Todo("classify.forest_type"))
    }

    /// Full AutoATES parameter set for the configured forest type.
    pub fn autoates(&self) -> Result<AutoAtesParams, ConfigError> {
        let c = &self.classify;
        let tables = &c.forest;
        let (tree, key) = match self.forest_type()? {
            ForestTypeName::Bav => (tables.bav, "classify.forest.bav"),
            ForestTypeName::Pcc => (tables.pcc, "classify.forest.pcc"),
            ForestTypeName::Stems => (tables.stems, "classify.forest.stems"),
            ForestTypeName::Sen2ccc => (tables.sen2ccc, "classify.forest.sen2ccc"),
        };
        let [tree1, tree2, tree3] = tree.ok_or(ConfigError::Todo(key))?;
        Ok(AutoAtesParams {
            slope: self.slope_thresholds()?,
            aat1: c.aat1.ok_or(ConfigError::Todo("classify.aat1"))?,
            aat2: c.aat2.ok_or(ConfigError::Todo("classify.aat2"))?,
            aat3: c.aat3.ok_or(ConfigError::Todo("classify.aat3"))?,
            forest: ForestThresholds {
                tree1,
                tree2,
                tree3,
            },
            cc1: c.cc1.ok_or(ConfigError::Todo("classify.cc1"))?,
            cc2: c.cc2.ok_or(ConfigError::Todo("classify.cc2"))?,
            isl_size_m2: c
                .isl_size_m2
                .ok_or(ConfigError::Todo("classify.isl_size_m2"))?,
        })
    }
}

impl Params {
    /// PRA parameters for a grid with `cell_size_m` cells. With a forest
    /// raster, the forest function follows `classify.forest_type`. Without
    /// one, AutoATES's `no_forest` mode uses the `pcc` function.
    pub fn pra_params(
        &self,
        cell_size_m: f64,
        with_forest: bool,
    ) -> Result<PraParams, ConfigError> {
        let p = &self.pra;
        let cauchy = |v: Option<[f32; 3]>, key| {
            v.map(|[a, b, c]| Cauchy { a, b, c })
                .ok_or(ConfigError::Todo(key))
        };
        let fc = &p.forest_cauchy;
        let forest = match with_forest.then(|| self.forest_type()).transpose()? {
            Some(ForestTypeName::Bav) => cauchy(fc.bav, "pra.forest_cauchy.bav")?,
            Some(ForestTypeName::Stems) => cauchy(fc.stems, "pra.forest_cauchy.stems")?,
            Some(ForestTypeName::Sen2ccc) => cauchy(fc.sen2ccc, "pra.forest_cauchy.sen2ccc")?,
            Some(ForestTypeName::Pcc) | None => cauchy(fc.pcc, "pra.forest_cauchy.pcc")?,
        };
        let radius_m = p
            .windshelter_radius_m
            .ok_or(ConfigError::Todo("pra.windshelter_radius_m"))?;
        Ok(PraParams {
            radius_cells: (radius_m / cell_size_m).round() as usize,
            prob: p
                .windshelter_prob
                .ok_or(ConfigError::Todo("pra.windshelter_prob"))?,
            wind_dir_deg: p
                .wind_dir_deg
                .ok_or(ConfigError::Todo("pra.wind_dir_deg"))?,
            wind_tol_deg: p
                .wind_tol_deg
                .ok_or(ConfigError::Todo("pra.wind_tol_deg"))?,
            threshold: p.threshold.ok_or(ConfigError::Todo("pra.threshold"))?,
            sieve_cells: p.sieve_cells.ok_or(ConfigError::Todo("pra.sieve_cells"))?,
            slope: cauchy(p.slope_cauchy, "pra.slope_cauchy")?,
            windshelter: cauchy(p.windshelter_cauchy, "pra.windshelter_cauchy")?,
            forest,
        })
    }
}

impl Params {
    /// Flow-Py parameters; unset values are TODOs.
    pub fn flowpy_params(&self) -> Result<FlowPyParams, ConfigError> {
        let f = &self.flowpy;
        Ok(FlowPyParams {
            alpha_deg: f.alpha_deg.ok_or(ConfigError::Todo("flowpy.alpha_deg"))?,
            exponent: f.exponent.ok_or(ConfigError::Todo("flowpy.exponent"))?,
            flux_threshold: f
                .flux_threshold
                .ok_or(ConfigError::Todo("flowpy.flux_threshold"))?,
            max_z_delta: f
                .max_z_delta
                .ok_or(ConfigError::Todo("flowpy.max_z_delta"))?,
        })
    }
}

/// A parsed config file, kept as TOML so presets can be merged.
#[derive(Debug, Clone)]
pub struct Config {
    defaults: Table,
    regions: Table,
    /// Path or label the config was loaded from.
    pub source: String,
    /// FNV-1a digest of the file bytes, for provenance.
    pub hash: String,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.display().to_string(),
            source,
        })?;
        Self::parse(&text, &path.display().to_string())
    }

    pub fn parse(text: &str, source_name: &str) -> Result<Self, ConfigError> {
        let parse_err = |message: String| ConfigError::Parse {
            source_name: source_name.to_owned(),
            message,
        };
        let mut root: Table = text
            .parse()
            .map_err(|e: toml::de::Error| parse_err(e.to_string()))?;
        match root.remove("schema_version") {
            Some(Value::Integer(SCHEMA_VERSION)) => {}
            Some(Value::Integer(v)) => return Err(ConfigError::Schema(v)),
            _ => return Err(parse_err("missing integer `schema_version`".into())),
        }
        let mut take_table = |key: &str| match root.remove(key) {
            None => Ok(Table::new()),
            Some(Value::Table(t)) => Ok(t),
            Some(_) => Err(parse_err(format!("`{key}` must be a table"))),
        };
        let defaults = take_table("defaults")?;
        let regions = take_table("regions")?;
        if let Some(key) = root.keys().next() {
            return Err(parse_err(format!("unknown top-level key `{key}`")));
        }
        let cfg = Self {
            defaults,
            regions,
            source: source_name.to_owned(),
            hash: fnv1a64_hex(text.as_bytes()),
        };
        // Validate every preset up front so typos fail at load time.
        cfg.params(None)?;
        for name in cfg.region_names() {
            cfg.params(Some(name))?;
        }
        Ok(cfg)
    }

    pub fn region_names(&self) -> impl Iterator<Item = &str> {
        self.regions.keys().map(String::as_str)
    }

    /// Defaults, with the named region preset merged over them.
    pub fn params(&self, region: Option<&str>) -> Result<Params, ConfigError> {
        let mut merged = self.defaults.clone();
        if let Some(name) = region {
            let Some(Value::Table(preset)) = self.regions.get(name) else {
                return Err(ConfigError::UnknownRegion {
                    name: name.to_owned(),
                    available: self.region_names().collect::<Vec<_>>().join(", "),
                });
            };
            let mut preset = preset.clone();
            preset.remove("description");
            merge(&mut merged, preset);
        }
        Value::Table(merged)
            .try_into()
            .map_err(|e: toml::de::Error| ConfigError::Parse {
                source_name: format!("{} [{}]", self.source, region.unwrap_or("defaults")),
                message: e.to_string(),
            })
    }
}

/// Recursively overlay `top` onto `base`.
fn merge(base: &mut Table, top: Table) {
    for (k, v) in top {
        match (base.get_mut(&k), v) {
            (Some(Value::Table(b)), Value::Table(t)) => merge(b, t),
            (_, v) => {
                base.insert(k, v);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
        schema_version = 1
        [defaults.dem]
        resampling = "bilinear"
        [defaults.terrain]
        compute_edges = false
        [regions.alpha]
        description = "test region"
        dem.target_res_m = 10.0
        prep.pad_m = 2500.0
        [regions.beta.terrain]
        compute_edges = true
    "#;

    #[test]
    fn defaults_leave_todos_unset() {
        let cfg = Config::parse(SAMPLE, "sample").unwrap();
        let p = cfg.params(None).unwrap();
        assert_eq!(p.dem.target_res_m, None);
        assert!(matches!(p.pad_m(), Err(ConfigError::Todo("prep.pad_m"))));
        assert!(!p.terrain.compute_edges);
    }

    #[test]
    fn region_overrides_merge() {
        let cfg = Config::parse(SAMPLE, "sample").unwrap();
        let a = cfg.params(Some("alpha")).unwrap();
        assert_eq!(a.dem.target_res_m, Some(10.0));
        assert_eq!(a.dem.resampling, Resampling::Bilinear);
        assert_eq!(a.pad_m().unwrap(), 2500.0);
        let b = cfg.params(Some("beta")).unwrap();
        assert!(b.terrain.compute_edges);
        assert_eq!(b.dem.target_res_m, None);
        assert_eq!(cfg.region_names().collect::<Vec<_>>(), ["alpha", "beta"]);
    }

    #[test]
    fn unknown_region_lists_available() {
        let cfg = Config::parse(SAMPLE, "sample").unwrap();
        let err = cfg.params(Some("gamma")).unwrap_err().to_string();
        assert!(err.contains("alpha, beta"), "{err}");
    }

    #[test]
    fn rejects_typos_and_bad_schema() {
        let typo = "schema_version = 1\n[defaults.dem]\ntarget_res = 10.0\n";
        assert!(matches!(
            Config::parse(typo, "t"),
            Err(ConfigError::Parse { .. })
        ));
        let bad_region = "schema_version = 1\n[regions.x.prep]\npadm = 1.0\n";
        assert!(matches!(
            Config::parse(bad_region, "t"),
            Err(ConfigError::Parse { .. })
        ));
        assert!(matches!(
            Config::parse("schema_version = 2", "t"),
            Err(ConfigError::Schema(2))
        ));
        assert!(Config::parse("[defaults]", "t").is_err());
        assert!(Config::parse("schema_version = 1\nextra = 1", "t").is_err());
    }

    #[test]
    fn hash_tracks_content() {
        let a = Config::parse(SAMPLE, "a").unwrap();
        let b = Config::parse(&format!("{SAMPLE}\n# changed"), "b").unwrap();
        assert_ne!(a.hash, b.hash);
        assert_eq!(a.hash.len(), 16);
    }

    #[test]
    fn shipped_default_config_is_valid() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/default.toml");
        let cfg = Config::load(&path).unwrap();
        let p = cfg.params(None).unwrap();
        // No scientific value may be invented in the defaults.
        assert!(
            p.prep.pad_m.is_none(),
            "pad_m must stay a TODO until sourced"
        );
        // AutoATES v2.0 defaults (AutoATES_classifier.py @ 3afcb49).
        let t = p.slope_thresholds().unwrap();
        assert_eq!(
            (t.sat01, t.sat12, t.sat23, t.sat34, t.win_size),
            (15.0, 18.0, 28.0, 39.0, 3)
        );
        assert!(matches!(
            p.autoates(),
            Err(ConfigError::Todo("classify.forest_type"))
        ));
        let bow = cfg.params(Some("bow_summit")).unwrap().autoates().unwrap();
        assert_eq!(
            (bow.forest.tree1, bow.forest.tree2, bow.forest.tree3),
            (10.0, 20.0, 25.0)
        );
        assert_eq!(
            (
                bow.aat1,
                bow.aat2,
                bow.aat3,
                bow.cc1,
                bow.cc2,
                bow.isl_size_m2
            ),
            (18.0, 24.0, 33.0, 5.0, 40.0, 30000.0)
        );
        // PRA_AutoATES-v2.0.py @ 3afcb49 defaults, as in PRA/log.txt.
        let pra = p.pra_params(10.0, false).unwrap();
        assert_eq!(
            (
                pra.radius_cells,
                pra.prob,
                pra.wind_dir_deg,
                pra.wind_tol_deg
            ),
            (6, 0.5, 0.0, 180.0)
        );
        assert_eq!((pra.threshold, pra.sieve_cells), (0.15, 3));
        let cauchy = |a, b, c| Cauchy { a, b, c };
        assert_eq!(pra.slope, cauchy(11.0, 4.0, 43.0));
        assert_eq!(pra.windshelter, cauchy(3.0, 10.0, 3.0));
        assert_eq!(pra.forest, cauchy(40.0, 3.5, -15.0), "no_forest uses pcc");
        assert!(matches!(
            p.pra_params(10.0, true),
            Err(ConfigError::Todo("classify.forest_type"))
        ));
        let mut stems = p.clone();
        stems.classify.forest_type = Some(ForestTypeName::Stems);
        assert_eq!(
            stems.pra_params(10.0, true).unwrap().forest,
            cauchy(350.0, 2.0, -120.0)
        );
        // Flow-Py: the Sykes et al. (2023) Bow Summit run.
        let fp = p.flowpy_params().unwrap();
        assert_eq!(
            (fp.alpha_deg, fp.exponent, fp.flux_threshold, fp.max_z_delta),
            (24.0, 8, 0.003, 270.0)
        );
        assert_eq!(p.flowpy.forest, FlowPyForest::None);
        let cam = cfg.params(Some("cameron_pass")).unwrap();
        assert_eq!(cam.forest_type().unwrap(), ForestTypeName::Pcc);
        // Toft et al. (2024) Table 2 canopy-cover thresholds.
        let f = cam.autoates().unwrap().forest;
        assert_eq!((f.tree1, f.tree2, f.tree3), (20.0, 55.0, 75.0));
        assert_eq!(cam.flowpy.forest, FlowPyForest::PccFraction);
        assert!(
            cam.site
                .dem_path
                .as_deref()
                .unwrap()
                .starts_with("/vsicurl/https://")
        );
        assert_eq!(cam.site.forest_where.as_deref(), Some("beginyear=2024"));
        assert_eq!(cam.dem.target_res_m, Some(10.0));
        for name in cfg.region_names() {
            cfg.params(Some(name)).unwrap();
        }
    }
}
