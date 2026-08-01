//! Minimal civil-date math so nothing else needs a datetime dependency.

/// Convert unix seconds to (year, month, day) in UTC.
/// Uses the classic days-from-civil inverse (Howard Hinnant's algorithm).
pub fn civil_from_unix(secs: i64) -> (i64, u32, u32) {
    let days = secs.div_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = if m <= 2 { y + 1 } else { y };
    (year, m, d)
}

/// "YYYY-MM-DD" in UTC.
pub fn date_str(secs: i64) -> String {
    let (y, m, d) = civil_from_unix(secs);
    format!("{y:04}-{m:02}-{d:02}")
}

/// "YYYY-Qn" quarter label, for timeline bucketing.
pub fn quarter_str(secs: i64) -> String {
    let (y, m, _) = civil_from_unix(secs);
    format!("{y:04}-Q{}", (m - 1) / 3 + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_dates() {
        assert_eq!(date_str(0), "1970-01-01");
        assert_eq!(date_str(951_782_400), "2000-02-29");
        assert_eq!(date_str(1_785_542_400), "2026-08-01");
        assert_eq!(quarter_str(1_785_542_400), "2026-Q3");
    }
}
