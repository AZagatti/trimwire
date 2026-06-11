//! Shared no-chrono UTC civil-date math (Howard-Hinnant). One copy for the
//! three CLI call sites that used to each inline it: the dashboard timestamp,
//! the `share stats` date, and `stats --since/--until` parsing.

/// Days since 1970-01-01 for a UTC `(y, m, d)`. Inverse of [`civil_from_days`].
pub(crate) fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

/// `(year, month, day)` for a count of days since 1970-01-01 (UTC).
pub(crate) fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Format unix seconds as `YYYY-MM-DD HH:MM` (UTC).
pub(crate) fn fmt_date_time(secs: i64) -> String {
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    let rem = secs.rem_euclid(86_400);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}",
        rem / 3600,
        (rem % 3600) / 60
    )
}

/// Format unix seconds as `YYYY-MM-DD` (UTC, date only).
pub(crate) fn fmt_date(secs: i64) -> String {
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    format!("{y:04}-{m:02}-{d:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_points_round_trip() {
        assert_eq!(fmt_date_time(0), "1970-01-01 00:00");
        assert_eq!(fmt_date_time(1_780_704_000), "2026-06-06 00:00");
        assert_eq!(fmt_date(0), "1970-01-01");
        // encode/decode are inverses
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2026, 6, 7) * 86_400, 1_780_790_400);
        assert_eq!(civil_from_days(days_from_civil(2026, 6, 7)), (2026, 6, 7));
    }
}
