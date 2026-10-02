//! Handler tests on a small synthetic region placed at Cameron Pass in
//! UTM 13N, so coordinates go through real PROJ transformations.

use std::path::PathBuf;
use std::sync::Arc;

use ates_api::regions::Region;
use ates_api::{
    AppState, PointQuery, RegionQuery, area, health, list_regions, point, route_evaluate,
};
use ates_core::{Crs, GeoTransform, Grid};
use ates_io::gdal_backend::GdalProjector;
use ates_io::{DISCLAIMER, Projector};
use ates_pipeline::route::RegionGrids;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use ndarray::Array2;

const EPSG: u32 = 32613;
const CENTRE: (f64, f64) = (-105.8917, 40.5208);

/// 20 x 20 cells of 10 m centred on Cameron Pass: west half class 1, east
/// half class 3; release area and avalanche path in the east half.
fn state() -> Arc<AppState> {
    let (cx, cy) = GdalProjector.lonlat_to(CENTRE.0, CENTRE.1, EPSG).unwrap();
    let gt = GeoTransform::north_up(cx - 100.0, cy + 100.0, 10.0, 10.0);
    let i16_grid = |f: &dyn Fn(usize, usize) -> i16| {
        Grid::new(
            Array2::from_shape_fn((20, 20), |(r, c)| f(r, c)),
            gt,
            Crs::Epsg(EPSG),
            Some(-9999.0),
        )
        .unwrap()
    };
    let f32_grid = |f: &dyn Fn(usize, usize) -> f32| {
        Grid::new(
            Array2::from_shape_fn((20, 20), |(r, c)| f(r, c)),
            gt,
            Crs::Epsg(EPSG),
            Some(-9999.0),
        )
        .unwrap()
    };
    let grids = RegionGrids {
        ates: i16_grid(&|_, c| if c < 10 { 1 } else { 3 }),
        // Falls to the west: aspect W.
        dem: Some(f32_grid(&|_, c| 3100.0 + 6.0 * c as f32)),
        pra: Some(i16_grid(&|_, c| i16::from(c >= 15))),
        fp_travel_angle: Some(f32_grid(&|_, c| if c >= 10 { 30.0 } else { 0.0 })),
        overhead: Some(i16_grid(&|_, c| if c >= 10 { 25 } else { 0 })),
        aspect: None,
    };
    let mut manifest = toml::Table::new();
    manifest.insert("config_fnv1a64".into(), "abc123".into());
    let forest = Some(f32_grid(&|_, _| 40.0));
    let region = Region::from_grids("test_pass", manifest, grids, forest).unwrap();
    Arc::new(AppState {
        regions: vec![region],
        data_dir: PathBuf::from("does-not-exist"),
    })
}

/// lon/lat of a projected offset (dx, dy) metres from the centre.
fn lonlat(dx: f64, dy: f64) -> (f64, f64) {
    let (cx, cy) = GdalProjector.lonlat_to(CENTRE.0, CENTRE.1, EPSG).unwrap();
    GdalProjector.to_lonlat(cx + dx, cy + dy, EPSG).unwrap()
}

#[tokio::test]
async fn health_and_regions() {
    let s = state();
    let h = health(State(s.clone())).await.0;
    assert_eq!((h.status, h.regions), ("ok", 1));
    let r = list_regions(State(s)).await.unwrap().0;
    assert_eq!(r.disclaimer, DISCLAIMER);
    let info = &r.regions[0];
    assert_eq!(
        (info.name.as_str(), info.epsg, info.rows),
        ("test_pass", EPSG, 20)
    );
    assert_eq!(info.cells_per_class, [0, 200, 0, 200, 0]);
    assert!(
        info.files.is_empty(),
        "no files on disk for a synthetic region"
    );
}

#[tokio::test]
async fn point_inside_and_outside() {
    let s = state();
    let (lon, lat) = lonlat(55.0, 5.0); // east half, column 15
    let q = PointQuery {
        lon,
        lat,
        region: None,
    };
    let p = point(State(s.clone()), Query(q)).await.unwrap().0;
    assert_eq!(p.ates_class, Some(3));
    assert_eq!(p.ates_class_name, Some("Complex"));
    assert_eq!(p.aspect, Some("W"));
    assert_eq!(p.in_release_area, Some(true));
    assert_eq!(p.on_avalanche_path, Some(true));
    assert_eq!(p.overhead, Some(25));
    assert_eq!(p.forest_canopy_pct, Some(40.0));
    assert!(
        (p.slope_deg.unwrap() - 31.0).abs() < 0.1,
        "atan(0.6) = 31 degrees"
    );
    assert_eq!(p.provenance.region.as_deref(), Some("test_pass"));
    assert_eq!(p.provenance.config_fnv1a64.as_deref(), Some("abc123"));
    assert_eq!(p.disclaimer, DISCLAIMER);

    let far = PointQuery {
        lon: -100.0,
        lat: 40.0,
        region: None,
    };
    let e = point(State(s.clone()), Query(far)).await.unwrap_err();
    assert_eq!(e.status, StatusCode::NOT_FOUND);
    let bad = PointQuery {
        lon: 500.0,
        lat: 0.0,
        region: None,
    };
    assert_eq!(
        point(State(s.clone()), Query(bad))
            .await
            .unwrap_err()
            .status,
        StatusCode::BAD_REQUEST
    );
    let wrong = PointQuery {
        lon,
        lat,
        region: Some("nowhere".into()),
    };
    assert_eq!(
        point(State(s), Query(wrong)).await.unwrap_err().status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn area_counts_classes() {
    let s = state();
    // The northern half of the grid (projected rectangle, in lon/lat).
    let ring: Vec<[f64; 2]> = [
        (-100.0, 0.0),
        (100.0, 0.0),
        (100.0, 100.0),
        (-100.0, 100.0),
        (-100.0, 0.0),
    ]
    .iter()
    .map(|&(dx, dy)| {
        let (lon, lat) = lonlat(dx, dy);
        [lon, lat]
    })
    .collect();
    let body = serde_json::json!({"type": "Polygon", "coordinates": [ring]}).to_string();
    let a = area(State(s.clone()), Query(RegionQuery::default()), body)
        .await
        .unwrap()
        .0;
    // 10 rows x 20 columns, half class 1 and half class 3 (cell centres
    // on the edges can tip a row, so allow one row of slack).
    assert!((180..=220).contains(&a.cells), "{} cells", a.cells);
    assert!((a.class_share["1"].as_f64().unwrap() - 0.5).abs() < 0.05);
    assert!((a.class_share["3"].as_f64().unwrap() - 0.5).abs() < 0.05);
    assert_eq!(a.provenance.region.as_deref(), Some("test_pass"));

    let e = area(State(s), Query(RegionQuery::default()), "{}".into())
        .await
        .unwrap_err();
    assert_eq!(e.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn route_geojson_and_gpx() {
    let s = state();
    let (a, b) = (lonlat(-95.0, 5.0), lonlat(95.0, 5.0));
    let line = serde_json::json!({
        "type": "Feature",
        "properties": {},
        "geometry": {"type": "LineString", "coordinates": [[a.0, a.1], [b.0, b.1]]}
    })
    .to_string();
    let r = route_evaluate(
        State(s.clone()),
        Query(RegionQuery::default()),
        HeaderMap::new(),
        line,
    )
    .await
    .unwrap()
    .0;
    let sum = &r["summary"];
    assert!((sum["total_m"].as_f64().unwrap() - 190.0).abs() < 0.5);
    // Sampling is every half cell (5 m), so a class boundary is located to
    // within one piece.
    assert!((sum["class_m"]["3"].as_f64().unwrap() - 95.0).abs() <= 5.0);
    assert_eq!(sum["max_class"], 3);
    assert_eq!(sum["region"], "test_pass");
    assert_eq!(sum["disclaimer"], DISCLAIMER);
    assert_eq!(r["features"].as_array().unwrap().len(), 2);

    let gpx = format!(
        r#"<gpx><trk><trkseg><trkpt lat="{}" lon="{}"/><trkpt lat="{}" lon="{}"/></trkseg></trk></gpx>"#,
        a.1, a.0, b.1, b.0
    );
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/gpx+xml"),
    );
    let r = route_evaluate(
        State(s.clone()),
        Query(RegionQuery::default()),
        headers,
        gpx,
    )
    .await
    .unwrap()
    .0;
    assert_eq!(r["summary"]["max_class"], 3);

    let e = route_evaluate(
        State(s),
        Query(RegionQuery::default()),
        HeaderMap::new(),
        "[]".into(),
    )
    .await
    .unwrap_err();
    assert_eq!(e.status, StatusCode::BAD_REQUEST);
}
