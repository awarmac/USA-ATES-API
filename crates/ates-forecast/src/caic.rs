//! Colorado Avalanche Information Center (CAIC) forecast files.
//!
//! CAIC's website reads two JSON documents through an undocumented proxy
//! (`https://avalanche.state.co.us/api-proxy/avid?_api_proxy_uri=...`):
//! - `/products/all`: an array of products. Those of `type`
//!   `avalancheforecast` carry `areaId`, `polygons` (zone polygon ids),
//!   `issueDateTime`, `expiryDateTime`, `dangerRatings.days[]` with
//!   `alp`/`tln`/`btl`, and `avalancheProblems.days[][]` with `type`,
//!   `aspectElevations`, `likelihood` and `expectedSize.{min,max}`.
//! - `/products/all/area`: GeoJSON zone polygons keyed by id.
//!
//! Field names follow probes of 2026-10-02 and the MIT-licensed `caicpy`
//! client (github.com/gormaniac/caicpy, `models.py`). The format of the
//! `aspectElevations` codes and of the danger values has not been seen in
//! season, so [`decode_location`] accepts only an unambiguous aspect and
//! band pair and keeps anything else as undecoded, and unknown danger
//! values stay [`Danger::Unrecognised`]. Check both against an in-season
//! sample.
//!
//! These functions only parse text. Nothing here fetches from CAIC; see the
//! crate docs on permission.

use std::collections::BTreeSet;

use ates_core::route::Aspect;
use serde_json::Value;

use crate::model::{Band, Danger, DayForecast, Problem, ZoneForecast};
use crate::zones::{Ring, Zone};
use crate::{ForecastError, Source};

pub fn source(retrieved: Option<String>) -> Source {
    Source {
        name: "Colorado Avalanche Information Center (CAIC)".into(),
        url: "https://avalanche.state.co.us/".into(),
        retrieved,
    }
}

fn parse_json(text: &str, what: &str) -> Result<Value, ForecastError> {
    serde_json::from_str(text).map_err(|source| ForecastError::Json {
        what: what.into(),
        source,
    })
}

/// A string, or a number written as one.
fn text(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// Decode one `aspectElevations` code into an aspect and a band.
///
/// The code is split into words at anything that is not a letter or digit
/// (`"n_alp"`, `"NE-TLN"`, `"btl sw"`). It decodes only if exactly one word
/// is a compass aspect (N ... NW) and exactly one is a band (`alp`, `tln`,
/// `btl`); anything else returns `None` and is reported as undecoded.
pub fn decode_location(code: &str) -> Option<(Aspect, Band)> {
    let words: Vec<String> = code
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_ascii_lowercase)
        .collect();
    if words.len() != 2 {
        return None;
    }
    let aspect = |w: &str| {
        Aspect::ALL
            .into_iter()
            .find(|a| a.as_str().eq_ignore_ascii_case(w))
    };
    let band = |w: &str| Band::ALL.into_iter().find(|b| b.code() == w);
    match (
        aspect(&words[0]),
        band(&words[1]),
        aspect(&words[1]),
        band(&words[0]),
    ) {
        (Some(a), Some(b), _, _) | (_, _, Some(a), Some(b)) => Some((a, b)),
        _ => None,
    }
}

fn parse_problem(v: &Value) -> Problem {
    let mut locations = BTreeSet::new();
    let mut undecoded = Vec::new();
    for code in v["aspectElevations"].as_array().into_iter().flatten() {
        match text(code) {
            Some(c) => match decode_location(&c) {
                Some(l) => {
                    locations.insert(l);
                }
                None => undecoded.push(c),
            },
            None => undecoded.push(code.to_string()),
        }
    }
    Problem {
        kind: text(&v["type"]).unwrap_or_else(|| "unknown".into()),
        likelihood: text(&v["likelihood"]),
        size_min: text(&v["expectedSize"]["min"]),
        size_max: text(&v["expectedSize"]["max"]),
        locations,
        undecoded,
    }
}

fn parse_forecast(p: &Value) -> Result<ZoneForecast, ForecastError> {
    let id = text(&p["id"]).ok_or_else(|| ForecastError::Format("forecast without `id`".into()))?;
    let area_id = text(&p["areaId"])
        .ok_or_else(|| ForecastError::Format(format!("forecast {id} has no `areaId`")))?;
    let danger_days = p["dangerRatings"]["days"]
        .as_array()
        .map_or(&[][..], Vec::as_slice);
    let problem_days = p["avalancheProblems"]["days"]
        .as_array()
        .map_or(&[][..], Vec::as_slice);
    let n = danger_days.len().max(problem_days.len());
    let days = (0..n)
        .map(|i| {
            let d = danger_days.get(i).unwrap_or(&Value::Null);
            let band = |k: &str| text(&d[k]).map_or(Danger::NoRating, |s| Danger::parse(&s));
            DayForecast {
                date: text(&d["date"]),
                danger: [band("btl"), band("tln"), band("alp")],
                problems: problem_days
                    .get(i)
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .map(parse_problem)
                    .collect(),
            }
        })
        .collect();
    Ok(ZoneForecast {
        title: text(&p["title"]),
        polygons: p["polygons"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(text)
            .collect(),
        issued: text(&p["issueDateTime"]),
        expires: text(&p["expiryDateTime"]),
        days,
        id,
        area_id,
    })
}

/// Parse `/products/all`, keeping only `avalancheforecast` products.
pub fn parse_products(json: &str) -> Result<Vec<ZoneForecast>, ForecastError> {
    let v = parse_json(json, "CAIC products")?;
    let items = v
        .as_array()
        .ok_or_else(|| ForecastError::Format("CAIC products: expected a JSON array".into()))?;
    items
        .iter()
        .filter(|p| p["type"] == "avalancheforecast")
        .map(parse_forecast)
        .collect()
}

fn ring(v: &Value) -> Option<Ring> {
    v.as_array()?
        .iter()
        .map(|p| Some((p.get(0)?.as_f64()?, p.get(1)?.as_f64()?)))
        .collect()
}

fn polygon(v: &Value) -> Option<Vec<Ring>> {
    v.as_array()?.iter().map(ring).collect()
}

/// Parse `/products/all/area`: a GeoJSON `FeatureCollection` of
/// `Polygon`/`MultiPolygon` features, each with an `id` (on the feature or
/// in its properties).
pub fn parse_areas(json: &str) -> Result<Vec<Zone>, ForecastError> {
    let v = parse_json(json, "CAIC areas")?;
    let features = v["features"].as_array().ok_or_else(|| {
        ForecastError::Format("CAIC areas: expected a GeoJSON FeatureCollection".into())
    })?;
    features
        .iter()
        .map(|f| {
            let id = text(&f["id"])
                .or_else(|| text(&f["properties"]["id"]))
                .ok_or_else(|| ForecastError::Format("CAIC areas: feature without an id".into()))?;
            let g = &f["geometry"];
            let c = &g["coordinates"];
            let polygons = match g["type"].as_str() {
                Some("Polygon") => polygon(c).map(|p| vec![p]),
                Some("MultiPolygon") => {
                    c.as_array().and_then(|ps| ps.iter().map(polygon).collect())
                }
                _ => None,
            }
            .ok_or_else(|| {
                ForecastError::Format(format!("CAIC areas: zone {id} has no polygon geometry"))
            })?;
            Ok(Zone {
                name: text(&f["properties"]["name"]).or_else(|| text(&f["properties"]["title"])),
                id,
                polygons,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_only_unambiguous_locations() {
        assert_eq!(decode_location("n_alp"), Some((Aspect::N, Band::Alp)));
        assert_eq!(decode_location("NE-TLN"), Some((Aspect::NE, Band::Tln)));
        assert_eq!(decode_location("btl sw"), Some((Aspect::SW, Band::Btl)));
        assert_eq!(decode_location("nalp"), None);
        assert_eq!(decode_location("n_e_alp"), None);
        assert_eq!(decode_location("all_alp"), None);
        assert_eq!(decode_location("n_s"), None);
    }

    /// A synthetic document shaped like CAIC's (not CAIC data).
    const PRODUCTS: &str = r#"[
      {"id": "f1", "type": "avalancheforecast", "areaId": "area-1",
       "title": "Test zone", "polygons": ["poly-a", "poly-b"],
       "issueDateTime": "2026-12-01T23:00:00Z", "expiryDateTime": "2026-12-02T23:00:00Z",
       "dangerRatings": {"days": [
         {"position": 1, "alp": "considerable", "tln": "moderate", "btl": "low", "date": "2026-12-02T00:00:00Z"},
         {"position": 2, "alp": "noRating", "tln": "noRating", "btl": "noRating"}]},
       "avalancheProblems": {"days": [[
         {"type": "windSlab", "aspectElevations": ["n_alp", "ne_alp", "n_tln", "mystery"],
          "likelihood": "likely", "expectedSize": {"min": "1", "max": "2"}, "comment": ""}]]}},
      {"id": "r1", "type": "regionaldiscussionforecast", "areaId": "area-9"}
    ]"#;

    #[test]
    fn parses_forecasts() {
        let f = parse_products(PRODUCTS).unwrap();
        assert_eq!(f.len(), 1, "only avalanche forecasts");
        let z = &f[0];
        assert_eq!(z.area_id, "area-1");
        assert_eq!(z.polygons, ["poly-a", "poly-b"]);
        assert_eq!(z.days.len(), 2);
        assert_eq!(z.days[0].danger(Band::Alp), &Danger::Rated(3));
        assert_eq!(z.days[0].danger(Band::Btl), &Danger::Rated(1));
        assert_eq!(z.days[1].danger(Band::Tln), &Danger::NoRating);
        let p = &z.days[0].problems[0];
        assert_eq!(p.kind, "windSlab");
        assert_eq!(p.locations.len(), 3);
        assert!(p.locations.contains(&(Aspect::NE, Band::Alp)));
        assert_eq!(p.undecoded, ["mystery"]);
        assert!(z.days[1].problems.is_empty());
        // Expires 2026-12-02T23:00Z = 1_796_252_400.
        assert_eq!(z.expired(1_796_252_399), Some(false));
        assert_eq!(z.expired(1_796_252_401), Some(true));
        assert!(parse_products("{}").is_err());
    }

    #[test]
    fn parses_areas() {
        let gj = r#"{"type": "FeatureCollection", "features": [
          {"type": "Feature", "id": "poly-a", "properties": {},
           "geometry": {"type": "MultiPolygon", "coordinates": [[[[0,0],[1,0],[1,1],[0,1],[0,0]]]]}},
          {"type": "Feature", "properties": {"id": "poly-b"},
           "geometry": {"type": "Polygon", "coordinates": [[[2,0],[3,0],[3,1],[2,1],[2,0]]]}}]}"#;
        let z = parse_areas(gj).unwrap();
        assert_eq!(z.len(), 2);
        assert!(z[0].contains(0.5, 0.5));
        assert_eq!(z[1].id, "poly-b");
        assert!(z[1].contains(2.5, 0.5));
    }
}
