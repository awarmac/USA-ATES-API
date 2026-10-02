//! Reading routes from GPX and GeoJSON files.
//!
//! Both return the route as parts, each a polyline of (lon, lat) in WGS 84
//! degrees:
//! - **GPX:** each track segment (`trkseg`) and each route (`rte`) is a
//!   part. Points are `trkpt` and `rtept`; waypoints (`wpt`) are ignored.
//!   This is a small reader for the common GPX 1.1 layout, not a full XML
//!   parser.
//! - **GeoJSON:** `LineString` and `MultiLineString`, alone or inside a
//!   `Feature` or `FeatureCollection`. Each line is a part; other geometry
//!   types are ignored.

use std::path::Path;

use crate::IoError;

/// A route as polylines of (lon, lat).
pub type RouteParts = Vec<Vec<(f64, f64)>>;

/// Read a `.gpx`, `.geojson` or `.json` route file.
pub fn read_route(path: &Path) -> Result<RouteParts, IoError> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| IoError::Invalid(format!("{}: {e}", path.display())))?;
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    let parts = match ext.as_deref() {
        Some("gpx") => parse_gpx(&text)?,
        Some("geojson" | "json") => parse_geojson(&text)?,
        _ => {
            return Err(IoError::Invalid(format!(
                "{}: expected a .gpx, .geojson or .json route",
                path.display()
            )));
        }
    };
    if parts.iter().all(|p| p.len() < 2) {
        return Err(IoError::Invalid(format!(
            "{}: no line with at least two points",
            path.display()
        )));
    }
    Ok(parts)
}

/// Parse GPX text into parts.
pub fn parse_gpx(text: &str) -> Result<RouteParts, IoError> {
    let mut parts: RouteParts = Vec::new();
    let mut open = false;
    let mut rest = text;
    while let Some(start) = rest.find('<') {
        rest = &rest[start + 1..];
        let end = rest
            .find('>')
            .ok_or_else(|| IoError::Invalid("GPX: unterminated tag".into()))?;
        let tag = &rest[..end];
        rest = &rest[end + 1..];
        let closing = tag.starts_with('/');
        let name = tag
            .trim_start_matches('/')
            .split(|c: char| c.is_whitespace() || c == '/')
            .next()
            .unwrap_or("");
        // Ignore namespace prefixes such as `gpx:trkpt`.
        let local = name.rsplit(':').next().unwrap_or(name);
        match (local, closing) {
            ("trkseg" | "rte", false) => {
                parts.push(Vec::new());
                open = true;
            }
            ("trkseg" | "rte", true) => open = false,
            ("trkpt" | "rtept", false) => {
                let lat = attr(tag, "lat")?;
                let lon = attr(tag, "lon")?;
                if !open {
                    parts.push(Vec::new());
                    open = true;
                }
                parts.last_mut().expect("opened above").push((lon, lat));
            }
            _ => {}
        }
    }
    Ok(parts)
}

/// Numeric attribute `name="…"` (or single-quoted) inside a tag.
fn attr(tag: &str, name: &str) -> Result<f64, IoError> {
    let bad = || IoError::Invalid(format!("GPX: point without a valid `{name}`: <{tag}>"));
    let mut search = tag;
    loop {
        let i = search.find(name).ok_or_else(bad)?;
        let before = search[..i].chars().last();
        let after = search[i + name.len()..].trim_start();
        search = &search[i + name.len()..];
        // Whole attribute name only (so `lat` does not match `lato`).
        if !before.is_some_and(char::is_whitespace) || !after.starts_with('=') {
            continue;
        }
        let v = after[1..].trim_start();
        let q = v
            .chars()
            .next()
            .filter(|c| *c == '"' || *c == '\'')
            .ok_or_else(bad)?;
        let close = v[1..].find(q).ok_or_else(bad)?;
        return v[1..=close].parse().map_err(|_| bad());
    }
}

/// Parse GeoJSON text into parts.
pub fn parse_geojson(text: &str) -> Result<RouteParts, IoError> {
    let v: serde_json::Value =
        serde_json::from_str(text).map_err(|e| IoError::Invalid(format!("GeoJSON: {e}")))?;
    let mut parts = Vec::new();
    collect(&v, &mut parts)?;
    Ok(parts)
}

fn collect(v: &serde_json::Value, parts: &mut RouteParts) -> Result<(), IoError> {
    match v.get("type").and_then(|t| t.as_str()) {
        Some("FeatureCollection") => {
            for f in v
                .get("features")
                .and_then(|f| f.as_array())
                .into_iter()
                .flatten()
            {
                collect(f, parts)?;
            }
        }
        Some("Feature") => {
            if let Some(g) = v.get("geometry").filter(|g| !g.is_null()) {
                collect(g, parts)?;
            }
        }
        Some("GeometryCollection") => {
            for g in v
                .get("geometries")
                .and_then(|g| g.as_array())
                .into_iter()
                .flatten()
            {
                collect(g, parts)?;
            }
        }
        Some("LineString") => parts.push(line(v.get("coordinates"))?),
        Some("MultiLineString") => {
            for l in v
                .get("coordinates")
                .and_then(|c| c.as_array())
                .into_iter()
                .flatten()
            {
                parts.push(line(Some(l))?);
            }
        }
        _ => {}
    }
    Ok(())
}

fn line(coords: Option<&serde_json::Value>) -> Result<Vec<(f64, f64)>, IoError> {
    let bad = || IoError::Invalid("GeoJSON: a line needs [[lon, lat], ...] coordinates".into());
    coords
        .and_then(|c| c.as_array())
        .ok_or_else(bad)?
        .iter()
        .map(|p| {
            let p = p.as_array().ok_or_else(bad)?;
            match (
                p.first().and_then(|x| x.as_f64()),
                p.get(1).and_then(|y| y.as_f64()),
            ) {
                (Some(lon), Some(lat)) => Ok((lon, lat)),
                _ => Err(bad()),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gpx_tracks_and_routes() {
        let gpx = r#"<?xml version="1.0"?>
<gpx version="1.1" creator="test" xmlns="http://www.topografix.com/GPX/1/1">
  <wpt lat="1" lon="1"><name>ignored</name></wpt>
  <trk><name>tour</name>
    <trkseg>
      <trkpt lat="40.52" lon="-105.89"><ele>3132</ele></trkpt>
      <trkpt lon='-105.88' lat='40.53'/>
    </trkseg>
    <trkseg><trkpt lat="40.54" lon="-105.87"></trkpt></trkseg>
  </trk>
  <rte><rtept lat="1.5" lon="2.5"/><rtept lat="1.6" lon="2.6"/></rte>
</gpx>"#;
        let parts = parse_gpx(gpx).unwrap();
        assert_eq!(
            parts,
            vec![
                vec![(-105.89, 40.52), (-105.88, 40.53)],
                vec![(-105.87, 40.54)],
                vec![(2.5, 1.5), (2.6, 1.6)],
            ]
        );
        assert!(parse_gpx(r#"<trkpt lon="1"/>"#).is_err(), "missing lat");
    }

    #[test]
    fn geojson_lines() {
        let fc = r#"{"type":"FeatureCollection","features":[
            {"type":"Feature","properties":{},"geometry":{"type":"LineString","coordinates":[[-105.89,40.52,3132],[-105.88,40.53]]}},
            {"type":"Feature","properties":{},"geometry":{"type":"Point","coordinates":[0,0]}},
            {"type":"Feature","properties":{},"geometry":{"type":"MultiLineString","coordinates":[[[1,2],[3,4]],[[5,6],[7,8]]]}}
        ]}"#;
        let parts = parse_geojson(fc).unwrap();
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0], vec![(-105.89, 40.52), (-105.88, 40.53)]);
        assert_eq!(parts[2], vec![(5.0, 6.0), (7.0, 8.0)]);
        assert!(parse_geojson(r#"{"type":"LineString","coordinates":[["a",1]]}"#).is_err());
        assert!(parse_geojson("not json").is_err());
    }
}
