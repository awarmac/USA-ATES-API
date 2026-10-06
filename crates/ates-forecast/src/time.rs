//! Just enough ISO 8601 to tell whether a forecast has expired, without a
//! date-time dependency.

/// Seconds since the Unix epoch, now.
pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// Days from 1970-01-01 to a proleptic Gregorian date (H. Hinnant's
/// `days_from_civil`).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Parse `YYYY-MM-DDTHH:MM[:SS[.fff]]` followed by `Z` or `±HH:MM` into
/// Unix seconds. A time without a zone is not accepted, because its
/// instant is unknown.
pub fn parse_iso8601(s: &str) -> Option<i64> {
    let s = s.trim();
    let (date, rest) = s.split_once(['T', ' '])?;
    let mut d = date.split('-');
    let (y, mo, da): (i64, i64, i64) = (
        d.next()?.parse().ok()?,
        d.next()?.parse().ok()?,
        d.next()?.parse().ok()?,
    );
    if d.next().is_some() || !(1..=12).contains(&mo) || !(1..=31).contains(&da) {
        return None;
    }
    let (clock, offset_s) = if let Some(c) = rest.strip_suffix(['Z', 'z']) {
        (c, 0)
    } else {
        let i = rest.rfind(['+', '-'])?;
        let (c, off) = rest.split_at(i);
        let sign = if off.starts_with('-') { -1 } else { 1 };
        let (oh, om) = off[1..].split_once(':').unwrap_or((&off[1..], "0"));
        let (oh, om): (i64, i64) = (oh.parse().ok()?, om.parse().ok()?);
        (c, sign * (oh * 3600 + om * 60))
    };
    let mut t = clock.split(':');
    let h: i64 = t.next()?.parse().ok()?;
    let mi: i64 = t.next()?.parse().ok()?;
    let sec: f64 = t.next().map_or(Some(0.0), |v| v.parse().ok())?;
    if t.next().is_some() || h > 23 || mi > 59 || !(0.0..61.0).contains(&sec) {
        return None;
    }
    Some(days_from_civil(y, mo, da) * 86_400 + h * 3600 + mi * 60 + sec as i64 - offset_s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_utc_and_offsets() {
        assert_eq!(parse_iso8601("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_iso8601("2026-10-06T19:05:50.000Z"),
            Some(1_791_313_550)
        );
        // 13:05:50 at UTC-6 is 19:05:50 UTC.
        assert_eq!(
            parse_iso8601("2026-10-06T13:05:50-06:00"),
            Some(1_791_313_550)
        );
        assert_eq!(parse_iso8601("2024-02-29T12:00Z"), Some(1_709_208_000));
        assert_eq!(parse_iso8601("2026-10-06T19:05:50"), None, "no zone");
        assert_eq!(parse_iso8601("not a date"), None);
    }
}
