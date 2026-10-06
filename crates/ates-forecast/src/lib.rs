//! Avalanche forecast context for ATES route reports.
//!
//! ATES is static terrain; a forecast changes daily. This crate keeps the
//! two apart: it reads a forecast, finds the forecast zone of each stretch
//! of an evaluated route, and attaches **forecast context** to it:
//! - the zone and forecast issue/expiry times;
//! - the danger rating for the stretch's elevation band(s)
//!   (`alp` / `tln` / `btl`);
//! - whether the stretch's aspects and bands are listed for each avalanche
//!   problem.
//!
//! It never changes a stretch's ATES class or rates a route as safe.
//! Unknowns are reported as unknown: a missing treeline elevation, an
//! aspect/elevation code it cannot decode, or an expired forecast.
//!
//! **Data access.** CAIC publishes no API. Its website's terms of use
//! prohibit robots and data mining and require written permission to
//! reproduce its material, so this crate has **no network client**.
//! [`CaicFiles`] reads forecast files saved by hand; a fetching provider
//! waits for CAIC's permission (see `docs/ARCHITECTURE.md`).

pub mod caic;
pub mod context;
pub mod model;
pub mod time;
pub mod zones;

use std::path::PathBuf;

pub use context::{ContextOptions, annotate_route_report};
pub use model::{Band, Danger, DayForecast, Problem, Treeline, ZoneForecast};
pub use zones::Zone;

/// Shown with every piece of forecast context.
pub const FORECAST_NOTICE: &str = "Forecast context only, shown next to the modeled terrain. \
It does not change any ATES class and does not rate a route as safe. Read the full \
forecast from its source before travelling.";

/// Errors reading or parsing a forecast.
#[derive(Debug, thiserror::Error)]
pub enum ForecastError {
    #[error("cannot read {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("invalid JSON in {what}: {source}")]
    Json {
        what: String,
        source: serde_json::Error,
    },
    #[error("unexpected forecast format: {0}")]
    Format(String),
}

/// Who published a forecast, for attribution.
#[derive(Debug, Clone, PartialEq)]
pub struct Source {
    pub name: String,
    pub url: String,
    /// When the files were saved, as given by whoever saved them.
    pub retrieved: Option<String>,
}

/// Forecasts and the zones they cover.
#[derive(Debug, Clone)]
pub struct Forecasts {
    pub source: Source,
    pub forecasts: Vec<ZoneForecast>,
    pub zones: Vec<Zone>,
}

impl Forecasts {
    /// Ids of the zone polygons containing a lon/lat point.
    pub fn zone_ids_at(&self, lon: f64, lat: f64) -> Vec<&str> {
        self.zones
            .iter()
            .filter(|z| z.contains(lon, lat))
            .map(|z| z.id.as_str())
            .collect()
    }

    /// The forecast covering a zone polygon id (a forecast lists its
    /// polygons) or with that area id.
    pub fn forecast_for(&self, zone_id: &str) -> Option<&ZoneForecast> {
        self.forecasts
            .iter()
            .find(|f| f.polygons.iter().any(|p| p == zone_id))
            .or_else(|| self.forecasts.iter().find(|f| f.area_id == zone_id))
    }

    /// The forecasts covering a lon/lat point (normally one).
    pub fn forecasts_at(&self, lon: f64, lat: f64) -> Vec<&ZoneForecast> {
        let mut out: Vec<&ZoneForecast> = Vec::new();
        for id in self.zone_ids_at(lon, lat) {
            if let Some(f) = self.forecast_for(id)
                && !out.iter().any(|o| o.id == f.id)
            {
                out.push(f);
            }
        }
        out
    }
}

/// Something that can supply forecasts.
pub trait ForecastProvider {
    fn load(&self) -> Result<Forecasts, ForecastError>;
}

/// CAIC forecast files saved by hand: the `products/all` JSON and the
/// `products/all/area` GeoJSON (see [`caic`]).
#[derive(Debug, Clone)]
pub struct CaicFiles {
    pub products: PathBuf,
    pub areas: PathBuf,
    /// When the files were saved, if known.
    pub retrieved: Option<String>,
}

impl ForecastProvider for CaicFiles {
    fn load(&self) -> Result<Forecasts, ForecastError> {
        let read = |p: &PathBuf| {
            std::fs::read_to_string(p).map_err(|source| ForecastError::Read {
                path: p.clone(),
                source,
            })
        };
        Ok(Forecasts {
            source: caic::source(self.retrieved.clone()),
            forecasts: caic::parse_products(&read(&self.products)?)?,
            zones: caic::parse_areas(&read(&self.areas)?)?,
        })
    }
}
