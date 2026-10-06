//! Forecast model: danger by elevation band, and avalanche problems by
//! aspect and elevation band.

use std::collections::BTreeSet;

use ates_core::route::Aspect;
use serde_json::{Value, json};

/// Forecast elevation bands, from low to high.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Band {
    /// Below treeline.
    Btl,
    /// Near treeline.
    Tln,
    /// Above treeline (alpine).
    Alp,
}

impl Band {
    pub const ALL: [Band; 3] = [Band::Btl, Band::Tln, Band::Alp];

    /// The code forecasts use: `btl`, `tln` or `alp`.
    pub fn code(self) -> &'static str {
        match self {
            Band::Btl => "btl",
            Band::Tln => "tln",
            Band::Alp => "alp",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Band::Btl => "Below treeline",
            Band::Tln => "Near treeline",
            Band::Alp => "Above treeline",
        }
    }
}

/// Where treeline lies, which splits elevations into bands: below
/// `lower_m` is below treeline, above `upper_m` is above treeline, and in
/// between is near treeline.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Treeline {
    pub lower_m: f64,
    pub upper_m: f64,
}

impl Treeline {
    pub fn new(lower_m: f64, upper_m: f64) -> Result<Self, String> {
        if lower_m.is_finite() && upper_m.is_finite() && lower_m <= upper_m {
            Ok(Self { lower_m, upper_m })
        } else {
            Err(format!(
                "treeline needs lower <= upper metres (got {lower_m}, {upper_m})"
            ))
        }
    }

    pub fn band(&self, z: f64) -> Band {
        if z < self.lower_m {
            Band::Btl
        } else if z > self.upper_m {
            Band::Alp
        } else {
            Band::Tln
        }
    }

    /// Every band an elevation range touches.
    pub fn bands(&self, min_m: f64, max_m: f64) -> Vec<Band> {
        let (lo, hi) = (self.band(min_m), self.band(max_m));
        Band::ALL
            .into_iter()
            .filter(|b| (lo..=hi).contains(b))
            .collect()
    }
}

/// A danger rating on the North American Public Avalanche Danger Scale.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Danger {
    /// Level 1 (Low) to 5 (Extreme).
    Rated(u8),
    /// The forecast gives no rating (out of season, or not rated).
    NoRating,
    /// A value this crate does not recognise, kept as given.
    Unrecognised(String),
}

impl Danger {
    const NAMES: [&str; 5] = ["Low", "Moderate", "Considerable", "High", "Extreme"];

    /// Parse a rating: a level name in any case (`"considerable"`), a
    /// digit 1-5, or a "no rating" form (`noRating`, `noForecast`, empty).
    pub fn parse(raw: &str) -> Self {
        let t = raw.trim();
        let key: String = t
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .collect::<String>()
            .to_ascii_lowercase();
        if let Some(i) = Self::NAMES
            .iter()
            .position(|n| n.eq_ignore_ascii_case(&key))
        {
            return Danger::Rated(i as u8 + 1);
        }
        match key.as_str() {
            "1" | "2" | "3" | "4" | "5" => Danger::Rated(key.parse().expect("digit")),
            "" | "norating" | "noforecast" | "none" | "0" => Danger::NoRating,
            _ => Danger::Unrecognised(t.to_owned()),
        }
    }

    pub fn level(&self) -> Option<u8> {
        match self {
            Danger::Rated(l) => Some(*l),
            _ => None,
        }
    }

    pub fn name(&self) -> String {
        match self {
            Danger::Rated(l) => format!("{} - {}", l, Self::NAMES[usize::from(*l - 1)]),
            Danger::NoRating => "No rating".into(),
            Danger::Unrecognised(s) => format!("Unrecognised rating \"{s}\""),
        }
    }

    pub fn to_json(&self) -> Value {
        json!({ "level": self.level(), "name": self.name() })
    }
}

/// An avalanche problem listed in a forecast day.
#[derive(Debug, Clone, PartialEq)]
pub struct Problem {
    /// The problem type as given (e.g. `windSlab`).
    pub kind: String,
    pub likelihood: Option<String>,
    pub size_min: Option<String>,
    pub size_max: Option<String>,
    /// Aspect and elevation band pairs where the problem is listed.
    pub locations: BTreeSet<(Aspect, Band)>,
    /// Location codes that could not be decoded, kept as given. While any
    /// remain, "not listed here" cannot be concluded.
    pub undecoded: Vec<String>,
}

impl Problem {
    pub fn to_json(&self) -> Value {
        json!({
            "type": self.kind,
            "likelihood": self.likelihood,
            "size_min": self.size_min,
            "size_max": self.size_max,
            "locations": locations_json(self.locations.iter()),
            "undecoded_locations": self.undecoded,
        })
    }
}

pub(crate) fn locations_json<'a>(it: impl Iterator<Item = &'a (Aspect, Band)>) -> Vec<String> {
    it.map(|(a, b)| format!("{}_{}", a.as_str(), b.code()))
        .collect()
}

/// One day of a zone forecast.
#[derive(Debug, Clone, PartialEq)]
pub struct DayForecast {
    /// The day's date as given.
    pub date: Option<String>,
    /// Danger per band, indexed by `Band as usize`.
    pub danger: [Danger; 3],
    pub problems: Vec<Problem>,
}

impl DayForecast {
    pub fn danger(&self, band: Band) -> &Danger {
        &self.danger[band as usize]
    }

    pub fn to_json(&self) -> Value {
        json!({
            "date": self.date,
            "danger": {
                "btl": self.danger(Band::Btl).to_json(),
                "tln": self.danger(Band::Tln).to_json(),
                "alp": self.danger(Band::Alp).to_json(),
            },
            "problems": self.problems.iter().map(Problem::to_json).collect::<Vec<_>>(),
        })
    }
}

/// A forecast for one zone (a set of polygons).
#[derive(Debug, Clone, PartialEq)]
pub struct ZoneForecast {
    /// The forecast product id.
    pub id: String,
    /// The forecast area id.
    pub area_id: String,
    pub title: Option<String>,
    /// Ids of the zone polygons it covers.
    pub polygons: Vec<String>,
    /// ISO 8601 timestamps as given.
    pub issued: Option<String>,
    pub expires: Option<String>,
    /// Day 0 is the issue day.
    pub days: Vec<DayForecast>,
}

impl ZoneForecast {
    /// `Some(true)` if `expires` is before `now_unix`; `None` if unknown.
    pub fn expired(&self, now_unix: i64) -> Option<bool> {
        let t = crate::time::parse_iso8601(self.expires.as_deref()?)?;
        Some(t < now_unix)
    }

    /// Summary without the days.
    pub fn header_json(&self, now_unix: i64) -> Value {
        json!({
            "id": self.id,
            "area_id": self.area_id,
            "title": self.title,
            "issued": self.issued,
            "expires": self.expires,
            "expired": self.expired(now_unix),
        })
    }

    pub fn to_json(&self, now_unix: i64) -> Value {
        let mut v = self.header_json(now_unix);
        v["polygons"] = json!(self.polygons);
        v["days"] = self.days.iter().map(DayForecast::to_json).collect();
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn danger_parsing() {
        assert_eq!(Danger::parse("considerable"), Danger::Rated(3));
        assert_eq!(Danger::parse("Moderate"), Danger::Rated(2));
        assert_eq!(Danger::parse("5"), Danger::Rated(5));
        assert_eq!(Danger::parse("noRating"), Danger::NoRating);
        assert_eq!(Danger::parse("noForecast"), Danger::NoRating);
        assert_eq!(Danger::parse(""), Danger::NoRating);
        assert_eq!(Danger::parse("spicy"), Danger::Unrecognised("spicy".into()));
        assert_eq!(Danger::Rated(3).name(), "3 - Considerable");
    }

    #[test]
    fn treeline_bands() {
        let t = Treeline::new(3200.0, 3500.0).unwrap();
        assert_eq!(t.band(3000.0), Band::Btl);
        assert_eq!(t.band(3200.0), Band::Tln);
        assert_eq!(t.band(3500.0), Band::Tln);
        assert_eq!(t.band(3600.0), Band::Alp);
        assert_eq!(t.bands(3100.0, 3600.0), Band::ALL.to_vec());
        assert_eq!(t.bands(3300.0, 3400.0), vec![Band::Tln]);
        assert!(Treeline::new(3500.0, 3200.0).is_err());
    }
}
