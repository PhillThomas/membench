//! Timestamps and small host facts, without pulling in a date-time crate.

/// Crate version baked in at compile time.
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Seconds and nanoseconds since the Unix epoch.
pub fn now_unix() -> (i64, u32) {
    let mut tv = libc::timeval {
        tv_sec: 0,
        tv_usec: 0,
    };
    // SAFETY: `tv` is a valid, fully initialised pointer to writable memory of
    // the size libc expects; a NULL `tz` is explicitly allowed by the API.
    let rc = unsafe { libc::gettimeofday(&mut tv, std::ptr::null_mut()) };
    if rc != 0 {
        // Fall back to SystemTime rather than reporting 1970.
        let d = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        return (d.as_secs() as i64, d.subsec_nanos());
    }
    (tv.tv_sec as i64, (tv.tv_usec as i64 * 1000) as u32)
}

/// Days since 1970-01-01 to (year, month, day). Howard Hinnant's
/// `civil_from_days`, valid for the whole proleptic Gregorian calendar.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as i64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn format_with_offset(secs: i64, nanos: u32, offset_secs: i32) -> String {
    // The wall clock being printed is UTC *plus* the offset, so the split into
    // days and seconds-of-day has to happen after the shift: a 23:30 UTC instant
    // in UTC+01:00 is the next day.
    let local = secs + offset_secs as i64;
    let days = local.div_euclid(86_400);
    let secs_of_day = local.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let (h, mi, s) = (
        secs_of_day / 3600,
        (secs_of_day % 3600) / 60,
        secs_of_day % 60,
    );
    let sign = if offset_secs < 0 { '-' } else { '+' };
    let ao = offset_secs.abs();
    let frac = if nanos >= 1_000 {
        format!(".{:09}", nanos).trim_end_matches('0').to_string()
    } else {
        String::new()
    };
    if offset_secs == 0 {
        format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}{frac}Z")
    } else {
        format!(
            // RFC 3339 wants a two-digit offset in both fields.
            "{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}{frac}{sign}{oh:02}:{om:02}",
            oh = ao / 3600,
            om = (ao % 3600) / 60
        )
    }
}

/// Local time with numeric offset, RFC 3339. Falls back to UTC if libc cannot
/// resolve the local zone.
pub fn now_rfc3339() -> String {
    let (secs, nanos) = now_unix();
    // SAFETY: `tm` points at writable memory of the right size; `secs` is a
    // value time_t by construction. localtime_r is the re-entrant variant.
    let offset = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        let t = secs as libc::time_t;
        if libc::localtime_r(&t, &mut tm).is_null() {
            None
        } else {
            Some(tm.tm_gmtoff as i32)
        }
    };
    format_with_offset(secs, nanos, offset.unwrap_or(0))
}

/// File-name stamp (UTC, filename safe) for
/// `~/.local/share/membench/<stamp>.json`.
pub fn now_file_stamp() -> String {
    let (secs, _) = now_unix();
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let (h, mi, sec) = (sod / 3600, sod % 3600 / 60, sod % 60);
    format!("{y:04}{m:02}{d:02}-{h:02}{mi:02}{sec:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_is_1970() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
        assert_eq!(civil_from_days(17_897), (2019, 1, 1));
    }

    #[test]
    fn formats_utc() {
        // 2025-01-01T00:00:00Z
        assert_eq!(
            format_with_offset(1_735_689_600, 0, 0),
            "2025-01-01T00:00:00Z"
        );
        // 1793244336 is 2026-10-29T03:25:36Z, i.e. 04:25:36 in UTC+01:00.
        assert_eq!(
            format_with_offset(1_793_244_336, 0, 3600),
            "2026-10-29T04:25:36+01:00"
        );
        // A negative offset crosses the day boundary backwards.
        assert_eq!(
            format_with_offset(1_735_689_600, 0, -18000),
            "2024-12-31T19:00:00-05:00"
        );
    }

    #[test]
    fn stamp_is_filename_safe() {
        let s = now_file_stamp();
        assert!(
            s.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "{s}"
        );
    }
}
