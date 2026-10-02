//! Response types: the stable contract for API clients (the frontend).
//!
//! Every response carries `disclaimer` and `provenance`. Field names are
//! snake_case JSON. Lengths are metres, areas square metres, angles
//! degrees. `null` means "no value here" (nodata, flat ground, missing
//! layer). The route report is GeoJSON; see `ates_pipeline::route`.

use serde::Serialize;
use serde_json::Value;

/// What produced a response.
#[derive(Debug, Clone, Serialize)]
pub struct Provenance {
    pub tool_version: &'static str,
    pub model: &'static str,
    pub region: Option<String>,
    /// Fingerprint of the config the region was built with.
    pub config_fnv1a64: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Health {
    pub status: &'static str,
    pub tool_version: &'static str,
    pub regions: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct RegionInfo {
    pub name: String,
    pub bbox_wgs84: Option<[f64; 4]>,
    pub epsg: u32,
    pub cell_size_m: f64,
    pub rows: usize,
    pub cols: usize,
    /// Cells per ATES class 0-4.
    pub cells_per_class: [usize; 5],
    /// URLs of the build's Cloud-Optimized GeoTIFFs, relative to the API.
    pub files: Vec<String>,
    /// The build manifest (parameters, data sources), as JSON.
    pub manifest: Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct RegionsResponse {
    pub regions: Vec<RegionInfo>,
    pub disclaimer: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct PointResponse {
    pub lon: f64,
    pub lat: f64,
    pub epsg: u32,
    pub x: f64,
    pub y: f64,
    pub ates_class: Option<i16>,
    pub ates_class_name: Option<&'static str>,
    pub elevation_m: Option<f32>,
    pub slope_deg: Option<f32>,
    /// Azimuth (0 = north, clockwise); `null` on flat ground.
    pub aspect_deg: Option<f32>,
    /// One of N, NE, E, SE, S, SW, W, NW.
    pub aspect: Option<&'static str>,
    pub forest_canopy_pct: Option<f32>,
    pub in_release_area: Option<bool>,
    /// On a modelled avalanche path (Flow-Py travel angle > 0).
    pub on_avalanche_path: Option<bool>,
    pub fp_travel_angle_deg: Option<f32>,
    /// Overhead exposure 0-100.
    pub overhead: Option<i16>,
    pub disclaimer: &'static str,
    pub provenance: Provenance,
}

#[derive(Debug, Clone, Serialize)]
pub struct AreaResponse {
    pub total_m2: f64,
    /// Area per class, keyed "0" to "4".
    pub class_m2: serde_json::Map<String, Value>,
    /// Share of the classified area per class (0-1), keyed "0" to "4".
    pub class_share: serde_json::Map<String, Value>,
    pub class_names: [&'static str; 5],
    pub nodata_m2: f64,
    pub cells: usize,
    pub disclaimer: &'static str,
    pub provenance: Provenance,
}

#[derive(Debug, Clone, Serialize)]
pub struct ErrorResponse {
    pub error: String,
    pub disclaimer: &'static str,
}
