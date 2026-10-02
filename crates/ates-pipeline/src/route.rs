//! Route evaluation against a region build, with a GeoJSON report.
//!
//! The route (lon/lat) is projected onto the region's grid and sampled
//! every half cell (`ates_core::route`). The report is a GeoJSON
//! `FeatureCollection` with one `LineString` per stretch of constant ATES
//! class, plus a `summary` member that carries totals and the disclaimer.
//! It describes the modeled terrain along the route; it is not an
//! avalanche forecast and does not rate a route as safe.

use ates_core::Grid;
use ates_core::route::{RouteLayers, RouteReport, evaluate};
use ates_core::terrain::aspect_deg;
use ates_io::{DISCLAIMER, Projector};
use serde_json::{Map, Value, json};

use crate::PipelineError;

/// ATES v2 class names (Statham, Campbell & Klassen 2018, "The Avalanche
/// Terrain Exposure Scale" v2, Table 1).
pub const ATES_CLASS_NAMES: [&str; 5] = [
    "Non-avalanche terrain",
    "Simple",
    "Challenging",
    "Complex",
    "Extreme",
];

/// The layers of a region build that route evaluation reads.
#[derive(Debug, Clone)]
pub struct RegionGrids {
    pub ates: Grid<i16>,
    pub dem: Option<Grid<f32>>,
    pub pra: Option<Grid<i16>>,
    pub fp_travel_angle: Option<Grid<f32>>,
    pub overhead: Option<Grid<i16>>,
}

/// A route's report together with the grid's EPSG code.
#[derive(Debug, Clone)]
pub struct RouteResult {
    pub epsg: u32,
    pub report: RouteReport,
}

/// Evaluate a route given as lon/lat polylines.
pub fn evaluate_route(
    parts_lonlat: &[Vec<(f64, f64)>],
    grids: &RegionGrids,
    projector: &dyn Projector,
) -> Result<RouteResult, PipelineError> {
    let ates_core::Crs::Epsg(epsg) = grids.ates.crs else {
        return Err(PipelineError::Io(ates_io::IoError::Invalid(
            "route evaluation needs a region grid with an EPSG CRS".into(),
        )));
    };
    let parts = parts_lonlat
        .iter()
        .map(|p| projector.lonlat_to_many(p, epsg))
        .collect::<Result<Vec<_>, _>>()?;
    let aspect = grids.dem.as_ref().map(|d| aspect_deg(d, false));
    let layers = RouteLayers {
        ates: &grids.ates,
        dem: grids.dem.as_ref(),
        aspect: aspect.as_ref(),
        pra: grids.pra.as_ref(),
        fp_travel_angle: grids.fp_travel_angle.as_ref(),
        overhead: grids.overhead.as_ref(),
    };
    let step = grids.ates.transform.ew_res() / 2.0;
    Ok(RouteResult {
        epsg,
        report: evaluate(&parts, &layers, step)?,
    })
}

/// Degrees to 6 decimals (about 0.1 m).
fn r6(v: f64) -> f64 {
    (v * 1e6).round() / 1e6
}

fn r1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

/// The report as a GeoJSON `FeatureCollection` in WGS 84. `meta` is merged
/// into the `summary` member (region, provenance, ...).
pub fn report_geojson(
    result: &RouteResult,
    projector: &dyn Projector,
    meta: Map<String, Value>,
) -> Result<Value, PipelineError> {
    let rep = &result.report;
    let mut features = Vec::with_capacity(rep.stretches.len());
    for s in &rep.stretches {
        let coords: Vec<[f64; 2]> = projector
            .to_lonlat_many(&s.points, result.epsg)?
            .into_iter()
            .map(|(lon, lat)| [r6(lon), r6(lat)])
            .collect();
        let aspect: Map<String, Value> = ates_core::route::Aspect::ALL
            .iter()
            .zip(s.aspect_m)
            .filter(|(_, m)| *m > 0.0)
            .map(|(a, m)| (a.as_str().to_owned(), json!(r1(m))))
            .collect();
        features.push(json!({
            "type": "Feature",
            "geometry": {"type": "LineString", "coordinates": coords},
            "properties": {
                "ates_class": s.class,
                "ates_class_name": s.class.map(|c| ATES_CLASS_NAMES[c as usize]),
                "outside_region": s.outside,
                "part": s.part,
                "start_m": r1(s.start_m),
                "end_m": r1(s.end_m),
                "length_m": r1(s.length_m()),
                "elevation_min_m": s.elevation_min_m.map(|z| r1(f64::from(z))),
                "elevation_max_m": s.elevation_max_m.map(|z| r1(f64::from(z))),
                "dominant_aspect": s.dominant_aspect().map(|a| a.as_str()),
                "aspect_m": aspect,
                "release_area_m": r1(s.release_area_m),
                "avalanche_path_m": r1(s.avalanche_path_m),
                "max_overhead": s.max_overhead,
            }
        }));
    }
    let class_m: Map<String, Value> = rep
        .class_m
        .iter()
        .enumerate()
        .map(|(c, m)| (c.to_string(), json!(r1(*m))))
        .collect();
    let mut summary = json!({
        "total_m": r1(rep.total_m),
        "class_m": class_m,
        "class_names": ATES_CLASS_NAMES,
        "nodata_m": r1(rep.nodata_m),
        "outside_region_m": r1(rep.outside_m),
        "release_area_m": r1(rep.release_area_m),
        "avalanche_path_m": r1(rep.avalanche_path_m),
        "max_class": rep.stretches.iter().filter_map(|s| s.class).max(),
        "disclaimer": DISCLAIMER,
    });
    if let Value::Object(m) = &mut summary {
        m.extend(meta);
    }
    Ok(json!({
        "type": "FeatureCollection",
        "summary": summary,
        "features": features,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ates_core::{Crs, GeoTransform};
    use ates_io::IoError;
    use ndarray::Array2;

    /// Treats lon/lat as projected metres, for synthetic grids.
    struct Identity;

    impl Projector for Identity {
        fn lonlat_to(&self, lon: f64, lat: f64, _: u32) -> Result<(f64, f64), IoError> {
            Ok((lon, lat))
        }

        fn to_lonlat(&self, x: f64, y: f64, _: u32) -> Result<(f64, f64), IoError> {
            Ok((x, y))
        }
    }

    fn grids() -> RegionGrids {
        let gt = GeoTransform::north_up(0.0, 100.0, 10.0, 10.0);
        let ates = Grid::new(
            Array2::from_shape_fn((10, 10), |(_, c)| if c < 5 { 1 } else { 4 }),
            gt,
            Crs::Epsg(32613),
            Some(-9999.0),
        )
        .unwrap();
        // A plane falling to the west: aspect 270.
        let dem = Grid::new(
            Array2::from_shape_fn((10, 10), |(_, c)| 2000.0 + 5.0 * c as f32),
            gt,
            Crs::Epsg(32613),
            Some(-9999.0),
        )
        .unwrap();
        RegionGrids {
            ates,
            dem: Some(dem),
            pra: None,
            fp_travel_angle: None,
            overhead: None,
        }
    }

    #[test]
    fn report_has_stretches_summary_and_disclaimer() {
        let result =
            evaluate_route(&[vec![(0.0, 55.0), (100.0, 55.0)]], &grids(), &Identity).unwrap();
        let mut meta = Map::new();
        meta.insert("region".into(), json!("test"));
        let gj = report_geojson(&result, &Identity, meta).unwrap();
        assert_eq!(gj["type"], "FeatureCollection");
        assert_eq!(gj["features"].as_array().unwrap().len(), 2);
        let s = &gj["summary"];
        assert_eq!(s["total_m"], 100.0);
        assert_eq!(s["class_m"]["1"], 50.0);
        assert_eq!(s["class_m"]["4"], 50.0);
        assert_eq!(s["max_class"], 4);
        assert_eq!(s["region"], "test");
        assert_eq!(s["disclaimer"], DISCLAIMER);
        let p = &gj["features"][1]["properties"];
        assert_eq!(p["ates_class_name"], "Extreme");
        assert_eq!(p["dominant_aspect"], "W");
        assert_eq!(p["start_m"], 50.0);
    }
}
