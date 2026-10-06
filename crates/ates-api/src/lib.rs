//! HTTP API for the ATES estimator.
//!
//! It serves precomputed region builds (`ates build-region`) and never
//! computes ATES on request. Builds are loaded into memory at startup.
//!
//! | Method | Path | Body / query | Returns |
//! |---|---|---|---|
//! | GET | `/v1/health` | | status, version |
//! | GET | `/v1/regions` | | regions, parameters, file URLs |
//! | GET | `/v1/point` | `lon`, `lat`, optional `region` | layers at the point |
//! | POST | `/v1/area` | GeoJSON (Multi)Polygon; optional `?region=` | area per class |
//! | POST | `/v1/route/evaluate` | GeoJSON line or GPX; optional `?region=`, `?forecast_day=` | GeoJSON report, with forecast context when forecasts are loaded |
//! | GET | `/v1/forecast` | | loaded forecast zones (404 without `--caic-products`) |
//! | GET | `/v1/forecast/{zone}` | | one zone forecast, by product, area or polygon id |
//! | GET | `/v1/files/{region}/{file}` | | the build's COGs and `ates.pmtiles` map tiles (range requests) |
//! | GET | `/` | | the built frontend, with `--web-dir` |
//!
//! Every JSON response states that results are a modeled terrain
//! classification, not an avalanche forecast. CORS allows any origin for
//! GET and POST, so a browser frontend on another port can call it; tighten
//! this before any public deployment.

pub mod regions;
pub mod types;

use std::path::PathBuf;
use std::sync::Arc;

use ates_core::Grid;
use ates_core::area::{Polygon, class_areas};
use ates_core::route::Aspect;
use ates_forecast::time::now_unix;
use ates_forecast::{ContextOptions, FORECAST_NOTICE, Forecasts, Treeline, annotate_route_report};
use ates_io::gdal_backend::GdalProjector;
use ates_io::route_file::{parse_geojson, parse_geojson_polygons, parse_gpx};
use ates_io::{DISCLAIMER, Projector};
use ates_pipeline::TOOL_VERSION;
use ates_pipeline::route::{ATES_CLASS_NAMES, evaluate_route, report_geojson};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tower_http::cors::{Any, CorsLayer};
use tower_http::services::ServeDir;
use tower_http::set_header::SetResponseHeader;

pub use regions::Region;
use types::{
    AreaResponse, ErrorResponse, Health, PointResponse, Provenance, RegionInfo, RegionsResponse,
};

const MODEL: &str = "AutoATES v2.0 (Rust port: PRA, Flow-Py, classifier)";

/// Shared, read-only server state.
#[derive(Debug)]
pub struct AppState {
    pub regions: Vec<Region>,
    /// Directory the region builds were loaded from (served under
    /// `/v1/files`).
    pub data_dir: PathBuf,
    /// Built frontend (`web/dist`) served at `/`, if any.
    pub web_dir: Option<PathBuf>,
    /// Forecasts loaded from saved files at startup, if any. They are
    /// shown as context and never change an ATES class.
    pub forecast: Option<ForecastState>,
}

/// Loaded forecasts and how to place stretches in elevation bands.
#[derive(Debug)]
pub struct ForecastState {
    pub forecasts: Forecasts,
    /// Treeline for every loaded zone; `None` shows all bands.
    pub treeline: Option<Treeline>,
}

/// An error with an HTTP status, returned as JSON.
#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub message: String,
}

impl ApiError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, message)
    }

    fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, message)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = ErrorResponse {
            error: self.message,
            disclaimer: DISCLAIMER,
        };
        (self.status, Json(body)).into_response()
    }
}

type ApiResult<T> = Result<T, ApiError>;

/// The application router.
pub fn router(state: Arc<AppState>) -> Router {
    // Range and the response headers PMTiles clients read, so map tiles
    // also load cross-origin.
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([Method::GET, Method::HEAD, Method::POST])
        .allow_headers([header::CONTENT_TYPE, header::RANGE, header::IF_MATCH])
        .expose_headers([header::CONTENT_RANGE, header::CONTENT_LENGTH, header::ETAG]);
    let mut app = Router::new()
        .route("/v1/health", get(health))
        .route("/v1/regions", get(list_regions))
        .route("/v1/point", get(point))
        .route("/v1/area", post(area))
        .route("/v1/route/evaluate", post(route_evaluate))
        .route("/v1/forecast", get(list_forecasts))
        .route("/v1/forecast/{zone}", get(zone_forecast))
        .nest_service("/v1/files", ServeDir::new(&state.data_dir));
    if let Some(web) = &state.web_dir {
        // Revalidate the app on every load so a rebuilt frontend is never
        // served stale from the browser cache (hashed assets still hit
        // 304 Not Modified cheaply).
        let web = SetResponseHeader::if_not_present(
            ServeDir::new(web),
            header::CACHE_CONTROL,
            header::HeaderValue::from_static("no-cache"),
        );
        app = app.fallback_service(web);
    }
    app.layer(cors).with_state(state)
}

fn provenance(region: &Region) -> Provenance {
    Provenance {
        tool_version: TOOL_VERSION,
        model: MODEL,
        region: Some(region.name.clone()),
        config_fnv1a64: region.config_hash(),
    }
}

pub async fn health(State(state): State<Arc<AppState>>) -> Json<Health> {
    Json(Health {
        status: "ok",
        tool_version: TOOL_VERSION,
        regions: state.regions.len(),
    })
}

pub async fn list_regions(State(state): State<Arc<AppState>>) -> ApiResult<Json<RegionsResponse>> {
    let mut out = Vec::new();
    for r in &state.regions {
        let ates = &r.grids.ates;
        let mut cells = [0_usize; 5];
        for &v in &ates.data {
            if (0..=4).contains(&v) {
                cells[v as usize] += 1;
            }
        }
        let mut files = Vec::new();
        for file in [
            "ates.pmtiles",
            "ates.tif",
            "dem.tif",
            "forest.tif",
            "pra.tif",
            "fp_travel_angle.tif",
            "overhead.tif",
            "cell_counts.tif",
            "z_delta.tif",
        ] {
            if state.data_dir.join(&r.name).join(file).exists() {
                files.push(format!("/v1/files/{}/{file}", r.name));
            }
        }
        out.push(RegionInfo {
            name: r.name.clone(),
            bbox_wgs84: r.bbox_wgs84(),
            epsg: r.epsg,
            cell_size_m: ates.transform.ew_res(),
            rows: ates.rows(),
            cols: ates.cols(),
            cells_per_class: cells,
            files,
            manifest: serde_json::to_value(&r.manifest)
                .map_err(|e| ApiError::internal(e.to_string()))?,
        });
    }
    Ok(Json(RegionsResponse {
        regions: out,
        disclaimer: DISCLAIMER,
    }))
}

/// Query of `/v1/point`.
#[derive(Debug, Clone, Deserialize)]
pub struct PointQuery {
    pub lon: f64,
    pub lat: f64,
    pub region: Option<String>,
}

/// A point found in a region build.
struct Located<'a> {
    region: &'a Region,
    /// Projected coordinates.
    xy: (f64, f64),
    /// (row, col) in the region grids.
    cell: (usize, usize),
}

/// Find the region (named, or the first containing the point) and the cell.
fn locate<'a>(
    state: &'a AppState,
    name: Option<&str>,
    lon: f64,
    lat: f64,
) -> ApiResult<Located<'a>> {
    let candidates: Vec<&Region> = match name {
        Some(n) => vec![
            state
                .regions
                .iter()
                .find(|r| r.name == n)
                .ok_or_else(|| ApiError::not_found(format!("no region named `{n}`")))?,
        ],
        None => state.regions.iter().collect(),
    };
    for r in candidates {
        let (x, y) = GdalProjector
            .lonlat_to(lon, lat, r.epsg)
            .map_err(|e| ApiError::internal(e.to_string()))?;
        if let Some(cell) = r.grids.ates.cell_at(x, y) {
            return Ok(Located {
                region: r,
                xy: (x, y),
                cell,
            });
        }
    }
    Err(ApiError::not_found(format!(
        "({lon}, {lat}) is outside every region build"
    )))
}

/// The region for a request body: the named one; else the first region
/// containing any of `points` (lon, lat); else the only region.
fn pick_region<'a>(
    state: &'a AppState,
    name: Option<&str>,
    points: &[(f64, f64)],
) -> ApiResult<&'a Region> {
    if let Some(n) = name {
        return state
            .regions
            .iter()
            .find(|r| r.name == n)
            .ok_or_else(|| ApiError::not_found(format!("no region named `{n}`")));
    }
    if points.is_empty() {
        return Err(ApiError::bad_request("the request has no coordinates"));
    }
    for &(lon, lat) in points {
        if let Ok(found) = locate(state, None, lon, lat) {
            return Ok(found.region);
        }
    }
    match state.regions.as_slice() {
        [only] => Ok(only),
        _ => Err(ApiError::not_found(
            "no region build contains this geometry; pass ?region=",
        )),
    }
}

fn value_at<T: Copy + Into<f64>>(g: Option<&Grid<T>>, ix: (usize, usize)) -> Option<T> {
    let g = g?;
    let v = g.data[ix];
    let f: f64 = v.into();
    (!(f.is_nan() || Some(f) == g.nodata)).then_some(v)
}

pub async fn point(
    State(state): State<Arc<AppState>>,
    Query(q): Query<PointQuery>,
) -> ApiResult<Json<PointResponse>> {
    if !(q.lon.is_finite() && q.lat.is_finite() && q.lat.abs() <= 90.0 && q.lon.abs() <= 180.0) {
        return Err(ApiError::bad_request("lon/lat must be WGS 84 degrees"));
    }
    let Located {
        region: r,
        xy: (x, y),
        cell: ix,
    } = locate(&state, q.region.as_deref(), q.lon, q.lat)?;
    let class = value_at(Some(&r.grids.ates), ix).filter(|c| (0..=4).contains(c));
    let aspect = value_at(r.grids.aspect.as_ref(), ix);
    let fp = value_at(r.grids.fp_travel_angle.as_ref(), ix);
    Ok(Json(PointResponse {
        lon: q.lon,
        lat: q.lat,
        epsg: r.epsg,
        x,
        y,
        ates_class: class,
        ates_class_name: class.map(|c| ATES_CLASS_NAMES[c as usize]),
        elevation_m: value_at(r.grids.dem.as_ref(), ix),
        slope_deg: value_at(r.slope.as_ref(), ix),
        aspect_deg: aspect,
        aspect: aspect.and_then(Aspect::from_azimuth).map(Aspect::as_str),
        forest_canopy_pct: value_at(r.forest.as_ref(), ix),
        in_release_area: value_at(r.grids.pra.as_ref(), ix).map(|v| v == 1),
        on_avalanche_path: fp.map(|v| v > 0.0),
        fp_travel_angle_deg: fp,
        overhead: value_at(r.grids.overhead.as_ref(), ix),
        disclaimer: DISCLAIMER,
        provenance: provenance(r),
    }))
}

/// Optional `?region=` on POST endpoints, and `?forecast_day=` (0 is the
/// issue day) on route evaluation.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RegionQuery {
    pub region: Option<String>,
    pub forecast_day: Option<usize>,
}

pub async fn area(
    State(state): State<Arc<AppState>>,
    Query(q): Query<RegionQuery>,
    body: String,
) -> ApiResult<Json<AreaResponse>> {
    let polygons =
        parse_geojson_polygons(&body).map_err(|e| ApiError::bad_request(e.to_string()))?;
    let points: Vec<(f64, f64)> = polygons.iter().flatten().flatten().copied().collect();
    if points.is_empty() {
        return Err(ApiError::bad_request(
            "body needs a GeoJSON Polygon or MultiPolygon",
        ));
    }
    let region = pick_region(&state, q.region.as_deref(), &points)?;
    let state2 = Arc::clone(&state);
    let name = region.name.clone();
    let (stats, prov) = tokio::task::spawn_blocking(move || -> ApiResult<_> {
        let r = state2
            .regions
            .iter()
            .find(|r| r.name == name)
            .expect("picked above");
        let projected: Vec<Polygon> = polygons
            .iter()
            .map(|p| {
                p.iter()
                    .map(|ring| GdalProjector.lonlat_to_many(ring, r.epsg))
                    .collect::<Result<Vec<_>, _>>()
            })
            .collect::<Result<_, _>>()
            .map_err(|e| ApiError::internal(e.to_string()))?;
        Ok((class_areas(&r.grids.ates, &projected), provenance(r)))
    })
    .await
    .map_err(|e| ApiError::internal(e.to_string()))??;

    let classified: f64 = stats.class_m2.iter().sum();
    let keyed = |f: &dyn Fn(f64) -> f64| -> Map<String, Value> {
        stats
            .class_m2
            .iter()
            .enumerate()
            .map(|(c, &m)| (c.to_string(), json!(f(m))))
            .collect()
    };
    Ok(Json(AreaResponse {
        total_m2: stats.total_m2(),
        class_m2: keyed(&|m| m),
        class_share: keyed(&|m| {
            if classified > 0.0 {
                m / classified
            } else {
                0.0
            }
        }),
        class_names: ATES_CLASS_NAMES,
        nodata_m2: stats.nodata_m2,
        cells: stats.cells,
        disclaimer: DISCLAIMER,
        provenance: prov,
    }))
}

pub async fn route_evaluate(
    State(state): State<Arc<AppState>>,
    Query(q): Query<RegionQuery>,
    headers: HeaderMap,
    body: String,
) -> ApiResult<Json<Value>> {
    let is_xml = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.contains("xml"))
        || body.trim_start().starts_with('<');
    let parts = if is_xml {
        parse_gpx(&body)
    } else {
        parse_geojson(&body)
    }
    .map_err(|e| ApiError::bad_request(e.to_string()))?;
    if parts.iter().all(|p| p.len() < 2) {
        return Err(ApiError::bad_request(
            "body needs a GPX track/route or a GeoJSON LineString with at least two points",
        ));
    }
    let points: Vec<(f64, f64)> = parts.iter().flatten().copied().collect();
    let name = pick_region(&state, q.region.as_deref(), &points)?
        .name
        .clone();
    let state2 = Arc::clone(&state);
    let report = tokio::task::spawn_blocking(move || -> ApiResult<Value> {
        let r = state2
            .regions
            .iter()
            .find(|r| r.name == name)
            .expect("picked above");
        let result = evaluate_route(&parts, &r.grids, &GdalProjector)
            .map_err(|e| ApiError::bad_request(e.to_string()))?;
        let mut meta = Map::new();
        meta.insert("region".into(), json!(r.name));
        meta.insert(
            "provenance".into(),
            serde_json::to_value(provenance(r)).map_err(|e| ApiError::internal(e.to_string()))?,
        );
        let mut gj = report_geojson(&result, &GdalProjector, meta)
            .map_err(|e| ApiError::internal(e.to_string()))?;
        if let Some(fc) = &state2.forecast {
            let opts = ContextOptions {
                day: q.forecast_day.unwrap_or(0),
                treeline: fc.treeline,
            };
            annotate_route_report(&mut gj, &fc.forecasts, &opts, now_unix());
        }
        Ok(gj)
    })
    .await
    .map_err(|e| ApiError::internal(e.to_string()))??;
    Ok(Json(report))
}

fn loaded_forecast(state: &AppState) -> ApiResult<&ForecastState> {
    state.forecast.as_ref().ok_or_else(|| {
        ApiError::not_found(
            "no forecasts loaded; start ates-api with --caic-products and --caic-areas",
        )
    })
}

fn forecast_meta(fc: &ForecastState) -> Value {
    let s = &fc.forecasts.source;
    json!({
        "source": {"name": s.name, "url": s.url, "retrieved": s.retrieved},
        "notice": FORECAST_NOTICE,
        "treeline_m": fc.treeline.map(|t| [t.lower_m, t.upper_m]),
        "disclaimer": DISCLAIMER,
    })
}

/// The loaded forecast zones, without their days.
pub async fn list_forecasts(State(state): State<Arc<AppState>>) -> ApiResult<Json<Value>> {
    let fc = loaded_forecast(&state)?;
    let now = now_unix();
    let mut v = forecast_meta(fc);
    v["forecasts"] = fc
        .forecasts
        .forecasts
        .iter()
        .map(|f| f.header_json(now))
        .collect();
    Ok(Json(v))
}

/// One zone forecast with its days, by product id, area id or zone
/// polygon id.
pub async fn zone_forecast(
    State(state): State<Arc<AppState>>,
    Path(zone): Path<String>,
) -> ApiResult<Json<Value>> {
    let fc = loaded_forecast(&state)?;
    let f = fc
        .forecasts
        .forecasts
        .iter()
        .find(|f| f.id == zone)
        .or_else(|| fc.forecasts.forecast_for(&zone))
        .ok_or_else(|| ApiError::not_found(format!("no forecast for zone `{zone}`")))?;
    let mut v = forecast_meta(fc);
    v["forecast"] = f.to_json(now_unix());
    Ok(Json(v))
}
