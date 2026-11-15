//! Time helpers. Timestamps are plain unix seconds (`i64`, UTC) so drivers
//! and the UI do not need a date library. `std::time::SystemTime::now()`
//! panics on `wasm32-unknown-unknown`, hence [`unix_now`].

/// Current time as seconds since the unix epoch.
pub fn unix_now() -> i64 {
    #[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
    {
        (js_sys::Date::now() / 1000.0) as i64
    }
    #[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
    {
        match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
            Ok(d) => d.as_secs() as i64,
            Err(e) => -(e.duration().as_secs() as i64),
        }
    }
}

/// Current time as nanoseconds since the unix epoch.
pub fn unix_now_ns() -> u64 {
    #[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
    {
        (js_sys::Date::now() * 1_000_000.0) as u64
    }
    #[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
    {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0)
    }
}

/// Converts a `SystemTime` into unix seconds.
pub fn system_time_to_unix(t: std::time::SystemTime) -> i64 {
    match t.duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(e) => -(e.duration().as_secs() as i64),
    }
}

/// Civil date-time (UTC) from unix seconds:
/// `(year, month 1-12, day 1-31, hour, minute, second)`.
pub fn civil_from_unix(ts: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = ts.div_euclid(86_400);
    let secs = ts.rem_euclid(86_400);
    // Howard Hinnant's days_from_civil inverse.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d, (secs / 3600) as u32, ((secs % 3600) / 60) as u32, (secs % 60) as u32)
}

/// Days since 1970-01-01 of a civil date (proleptic Gregorian).
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let (m, d) = (m as i64, d as i64);
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// An MS-DOS date and time (zip, FAT), read as UTC; `None` for an empty
/// date.
pub fn from_dos(date: u16, time: u16) -> Option<i64> {
    if date == 0 {
        return None;
    }
    let (y, m, d) = (1980 + (date >> 9) as i64, ((date >> 5) & 15) as u32, (date & 31) as u32);
    let (h, min, s) = ((time >> 11) as i64, ((time >> 5) & 63) as i64, ((time & 31) * 2) as i64);
    Some(days_from_civil(y, m.max(1), d.max(1)) * 86_400 + h * 3600 + min * 60 + s)
}

/// `(time, date)` in MS-DOS form (UTC; 2-second steps; 1980–2107).
pub fn to_dos(t: i64) -> (u16, u16) {
    let t = t.clamp(days_from_civil(1980, 1, 1) * 86_400, days_from_civil(2107, 12, 31) * 86_400 + 86_399);
    let (y, m, d, h, min, s) = civil_from_unix(t);
    let time = ((h << 11) | (min << 5) | (s / 2)) as u16;
    let date = (((y - 1980) as u32) << 9 | (m << 5) | d) as u16;
    (time, date)
}

/// `YYYY-MM-DD HH:MM` (UTC).
pub fn format_unix(ts: i64) -> String {
    let (y, mo, d, h, mi, _) = civil_from_unix(ts);
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil() {
        assert_eq!(civil_from_unix(0), (1970, 1, 1, 0, 0, 0));
        assert_eq!(format_unix(1_793_000_000), "2026-10-26 07:33");
        assert_eq!(civil_from_unix(951_782_400), (2000, 2, 29, 0, 0, 0));
        assert_eq!(civil_from_unix(-1), (1969, 12, 31, 23, 59, 59));
    }
}

#[cfg(test)]
mod dos_tests {
    use super::*;

    #[test]
    fn dos_times_round_trip() {
        for t in [315_532_800i64, 1_700_000_000, 4_354_819_198] {
            let (time, date) = to_dos(t);
            assert_eq!(from_dos(date, time), Some(t - t % 2));
        }
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(from_dos(0, 0), None);
    }
}
