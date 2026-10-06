//! Forecast context for each stretch of a route report.
//!
//! [`annotate_route_report`] works on the GeoJSON report of
//! `ates_pipeline::route::report_geojson`: it reads each stretch's lon/lat
//! coordinates, elevation range and aspects, and adds a `forecast_context`
//! property, plus a `forecast` member in the summary. ATES classes and
//! lengths are left exactly as they were.

use std::collections::BTreeSet;

use ates_core::route::Aspect;
use serde_json::{Map, Value, json};

use crate::model::{Band, Danger, DayForecast, Treeline, ZoneForecast, locations_json};
use crate::{FORECAST_NOTICE, Forecasts};

/// How to attach forecast context.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ContextOptions {
    /// Forecast day: 0 is the issue day.
    pub day: usize,
    /// Treeline elevations for the zone. Without them a stretch's band is
    /// unknown, and all three bands are shown.
    pub treeline: Option<Treeline>,
}

/// The stretch facts the context depends on.
struct StretchFacts {
    coords: Vec<(f64, f64)>,
    elevation: Option<(f64, f64)>,
    aspects: Vec<Aspect>,
}

fn facts(f: &Value) -> StretchFacts {
    let p = &f["properties"];
    let coords = f["geometry"]["coordinates"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| Some((c.get(0)?.as_f64()?, c.get(1)?.as_f64()?)))
        .collect();
    let elevation = p["elevation_min_m"]
        .as_f64()
        .zip(p["elevation_max_m"].as_f64());
    let aspects = p["aspect_m"]
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(_, m)| m.as_f64().is_some_and(|m| m > 0.0))
        .filter_map(|(k, _)| Aspect::ALL.into_iter().find(|a| a.as_str() == k))
        .collect();
    StretchFacts {
        coords,
        elevation,
        aspects,
    }
}

fn highest<'a>(ds: impl Iterator<Item = &'a Danger>) -> Option<&'a Danger> {
    ds.filter(|d| d.level().is_some()).max_by_key(|d| d.level())
}

/// Context for one stretch in one zone forecast.
fn stretch_context(
    zf: &ZoneForecast,
    s: &StretchFacts,
    opts: &ContextOptions,
    now_unix: i64,
) -> Value {
    let mut ctx = zf.header_json(now_unix);
    let (bands, bands_from) = match (opts.treeline, s.elevation) {
        (Some(t), Some((lo, hi))) => (t.bands(lo, hi), "treeline"),
        (None, _) => (
            Band::ALL.to_vec(),
            "unknown: no treeline elevations; all bands shown",
        ),
        (_, None) => (Band::ALL.to_vec(), "unknown: no elevation; all bands shown"),
    };
    ctx["bands"] = json!(bands.iter().map(|b| b.code()).collect::<Vec<_>>());
    ctx["bands_from"] = json!(bands_from);
    let Some(day) = zf.days.get(opts.day) else {
        ctx["day"] = Value::Null;
        ctx["note"] = json!(format!("the forecast has no day {}", opts.day));
        return ctx;
    };
    ctx["day"] = json!(day.date);
    let danger: Map<String, Value> = bands
        .iter()
        .map(|b| (b.code().to_owned(), day.danger(*b).to_json()))
        .collect();
    ctx["danger"] = Value::Object(danger);
    ctx["highest_danger"] =
        highest(bands.iter().map(|b| day.danger(*b))).map_or(Value::Null, Danger::to_json);
    ctx["problems"] = problems(day, &bands, &s.aspects);
    ctx
}

/// Each problem, and whether this stretch's aspects and bands are listed.
/// `listed_here` is `null` when that cannot be told: the stretch has no
/// aspect (flat or missing), or the problem has location codes that could
/// not be decoded and none of the decoded ones match.
fn problems(day: &DayForecast, bands: &[Band], aspects: &[Aspect]) -> Value {
    let here: BTreeSet<(Aspect, Band)> = aspects
        .iter()
        .flat_map(|a| bands.iter().map(move |b| (*a, *b)))
        .collect();
    day.problems
        .iter()
        .map(|p| {
            let matched: Vec<_> = p.locations.intersection(&here).collect();
            let listed = if !matched.is_empty() {
                json!(true)
            } else if aspects.is_empty() || !p.undecoded.is_empty() {
                Value::Null
            } else {
                json!(false)
            };
            json!({
                "type": p.kind,
                "likelihood": p.likelihood,
                "size_min": p.size_min,
                "size_max": p.size_max,
                "listed_here": listed,
                "matched_locations": locations_json(matched.into_iter()),
                "undecoded_locations": p.undecoded,
            })
        })
        .collect()
}

/// Add forecast context to a route report (a GeoJSON `FeatureCollection`
/// with a `summary` member).
///
/// Each feature gets `forecast_context`: one entry per forecast zone its
/// vertices fall in (usually one; none outside every zone). The summary
/// gets `forecast`: the source, the notice, the zones used and the highest
/// danger met along the route.
pub fn annotate_route_report(
    report: &mut Value,
    forecasts: &Forecasts,
    opts: &ContextOptions,
    now_unix: i64,
) {
    let mut used: Vec<&ZoneForecast> = Vec::new();
    let mut no_zone_m = 0.0;
    let mut route_highest: Option<Danger> = None;
    for f in report["features"].as_array_mut().into_iter().flatten() {
        let s = facts(f);
        let mut zones: Vec<&ZoneForecast> = Vec::new();
        for &(lon, lat) in &s.coords {
            for z in forecasts.forecasts_at(lon, lat) {
                if !zones.iter().any(|o| o.id == z.id) {
                    zones.push(z);
                }
            }
        }
        if zones.is_empty() {
            no_zone_m += f["properties"]["length_m"].as_f64().unwrap_or(0.0);
        }
        let ctx: Vec<Value> = zones
            .iter()
            .map(|z| stretch_context(z, &s, opts, now_unix))
            .collect();
        for c in &ctx {
            if let Some(l) = c["highest_danger"]["level"].as_u64()
                && route_highest
                    .as_ref()
                    .and_then(Danger::level)
                    .is_none_or(|h| u64::from(h) < l)
            {
                route_highest = Some(Danger::Rated(l as u8));
            }
        }
        for z in zones {
            if !used.iter().any(|o| o.id == z.id) {
                used.push(z);
            }
        }
        f["properties"]["forecast_context"] = Value::Array(ctx);
    }
    let src = &forecasts.source;
    report["summary"]["forecast"] = json!({
        "source": {"name": src.name, "url": src.url, "retrieved": src.retrieved},
        "notice": FORECAST_NOTICE,
        "day": opts.day,
        "treeline_m": opts.treeline.map(|t| [t.lower_m, t.upper_m]),
        "zones": used.iter().map(|z| z.header_json(now_unix)).collect::<Vec<_>>(),
        "highest_danger": route_highest.as_ref().map(Danger::to_json),
        "no_zone_m": (no_zone_m * 10.0_f64).round() / 10.0,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Problem;
    use crate::zones::Zone;
    use crate::{Source, Treeline};

    fn forecasts() -> Forecasts {
        let problem = Problem {
            kind: "windSlab".into(),
            likelihood: Some("likely".into()),
            size_min: Some("1".into()),
            size_max: Some("2".into()),
            locations: [(Aspect::N, Band::Alp), (Aspect::NE, Band::Tln)]
                .into_iter()
                .collect(),
            undecoded: vec![],
        };
        Forecasts {
            source: Source {
                name: "Test".into(),
                url: "https://example.org".into(),
                retrieved: None,
            },
            forecasts: vec![ZoneForecast {
                id: "f1".into(),
                area_id: "area-1".into(),
                title: Some("Zone 1".into()),
                polygons: vec!["poly-a".into()],
                issued: Some("2026-12-01T23:00:00Z".into()),
                expires: Some("2026-12-02T23:00:00Z".into()),
                days: vec![DayForecast {
                    date: Some("2026-12-02".into()),
                    danger: [Danger::Rated(1), Danger::Rated(2), Danger::Rated(3)],
                    problems: vec![problem],
                }],
            }],
            zones: vec![Zone {
                id: "poly-a".into(),
                name: None,
                polygons: vec![vec![vec![
                    (-106.0, 40.0),
                    (-105.0, 40.0),
                    (-105.0, 41.0),
                    (-106.0, 41.0),
                ]]],
            }],
        }
    }

    fn report(lon: f64, elev: (f64, f64), aspect: &str) -> Value {
        json!({
            "type": "FeatureCollection",
            "summary": {"total_m": 100.0},
            "features": [{
                "type": "Feature",
                "geometry": {"type": "LineString", "coordinates": [[lon, 40.5], [lon + 0.001, 40.5]]},
                "properties": {
                    "ates_class": 3, "length_m": 100.0,
                    "elevation_min_m": elev.0, "elevation_max_m": elev.1,
                    "aspect_m": {aspect: 100.0},
                }
            }]
        })
    }

    const BEFORE_EXPIRY: i64 = 1_796_200_000; // 2026-12-02T~10:00Z

    #[test]
    fn danger_and_problems_follow_bands_and_aspects() {
        let opts = ContextOptions {
            day: 0,
            treeline: Some(Treeline::new(3300.0, 3500.0).unwrap()),
        };
        // North-facing, entirely above treeline.
        let mut r = report(-105.8, (3550.0, 3600.0), "N");
        annotate_route_report(&mut r, &forecasts(), &opts, BEFORE_EXPIRY);
        let c = &r["features"][0]["properties"]["forecast_context"][0];
        assert_eq!(c["area_id"], "area-1");
        assert_eq!(c["bands"], json!(["alp"]));
        assert_eq!(c["highest_danger"]["level"], 3);
        assert_eq!(c["expired"], false);
        assert_eq!(c["problems"][0]["listed_here"], true);
        assert_eq!(c["problems"][0]["matched_locations"], json!(["N_alp"]));
        // ATES fields untouched.
        assert_eq!(r["features"][0]["properties"]["ates_class"], 3);
        assert_eq!(r["summary"]["forecast"]["highest_danger"]["level"], 3);

        // South-facing below treeline: not listed, lower danger.
        let mut r = report(-105.8, (3000.0, 3100.0), "S");
        annotate_route_report(&mut r, &forecasts(), &opts, BEFORE_EXPIRY);
        let c = &r["features"][0]["properties"]["forecast_context"][0];
        assert_eq!(c["bands"], json!(["btl"]));
        assert_eq!(c["highest_danger"]["level"], 1);
        assert_eq!(c["problems"][0]["listed_here"], false);
    }

    #[test]
    fn unknowns_stay_unknown() {
        // No treeline: every band shown, highest is the alpine rating.
        let mut r = report(-105.8, (3000.0, 3100.0), "NE");
        annotate_route_report(
            &mut r,
            &forecasts(),
            &ContextOptions::default(),
            BEFORE_EXPIRY,
        );
        let c = &r["features"][0]["properties"]["forecast_context"][0];
        assert_eq!(c["bands"], json!(["btl", "tln", "alp"]));
        assert!(c["bands_from"].as_str().unwrap().starts_with("unknown"));
        assert_eq!(c["highest_danger"]["level"], 3);
        assert_eq!(c["problems"][0]["listed_here"], true);

        // Undecoded codes and no match: cannot say "not listed".
        let mut fc = forecasts();
        fc.forecasts[0].days[0].problems[0].undecoded = vec!["mystery".into()];
        let mut r = report(-105.8, (3000.0, 3100.0), "S");
        annotate_route_report(&mut r, &fc, &ContextOptions::default(), BEFORE_EXPIRY);
        let c = &r["features"][0]["properties"]["forecast_context"][0];
        assert!(c["problems"][0]["listed_here"].is_null());

        // Expired forecast is flagged.
        let mut r = report(-105.8, (3000.0, 3100.0), "S");
        annotate_route_report(&mut r, &fc, &ContextOptions::default(), 1_900_000_000);
        assert_eq!(
            r["features"][0]["properties"]["forecast_context"][0]["expired"],
            true
        );
    }

    #[test]
    fn outside_every_zone() {
        let mut r = report(-110.0, (3000.0, 3100.0), "S");
        annotate_route_report(
            &mut r,
            &forecasts(),
            &ContextOptions::default(),
            BEFORE_EXPIRY,
        );
        assert_eq!(
            r["features"][0]["properties"]["forecast_context"],
            json!([])
        );
        assert_eq!(r["summary"]["forecast"]["no_zone_m"], 100.0);
        assert!(r["summary"]["forecast"]["highest_danger"].is_null());
    }
}
