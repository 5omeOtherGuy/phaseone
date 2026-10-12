//! RFC 3339 instants at seconds precision, without a calendar crate: the component has no
//! clock and needs only to read and write the vendors' reset instants, in UTC.

/// `seconds` since the Unix epoch as `YYYY-MM-DDTHH:MM:SSZ`, or `None` outside the years
/// 0 to 9999.
pub(crate) fn format(seconds: i64) -> Option<String> {
    let days = seconds.div_euclid(86_400);
    let clock = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    (0..=9999).contains(&year).then(|| {
        format!(
            "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
            clock / 3600,
            clock / 60 % 60,
            clock % 60
        )
    })
}

/// The seconds since the Unix epoch of `YYYY-MM-DDTHH:MM:SS[.fraction](Z|±HH:MM)`; the
/// fraction is dropped. `None` for anything else, an impossible date or time included.
pub(crate) fn parse(text: &str) -> Option<i64> {
    let (day, clock) = text.split_once('T')?;
    let mut date = day.split('-');
    let year: i64 = date.next()?.parse().ok()?;
    let month: u32 = date.next()?.parse().ok()?;
    let day: u32 = date.next()?.parse().ok()?;
    if date.next().is_some() {
        return None;
    }
    let (clock, offset) = if let Some(clock) = clock.strip_suffix('Z') {
        (clock, 0)
    } else {
        let pos = clock.rfind(['+', '-'])?;
        let (clock, zone) = clock.split_at(pos);
        let (hours, minutes) = zone[1..].split_once(':')?;
        let (hours, minutes): (i64, i64) = (hours.parse().ok()?, minutes.parse().ok()?);
        if !(0..=25).contains(&hours) || !(0..=59).contains(&minutes) {
            return None;
        }
        let sign = if zone.starts_with('-') { -1 } else { 1 };
        (clock, sign * (hours * 3600 + minutes * 60))
    };
    let mut parts = clock.split(':');
    let hour: i64 = parts.next()?.parse().ok()?;
    let minute: i64 = parts.next()?.parse().ok()?;
    let second: i64 = parts.next()?.split('.').next()?.parse().ok()?;
    if !(1..=12).contains(&month)
        || day == 0
        || day > days_in_month(year, month)
        || !(0..24).contains(&hour)
        || !(0..60).contains(&minute)
        || !(0..60).contains(&second)
    {
        return None;
    }
    Some(days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second - offset)
}

fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year.rem_euclid(400);
    let month = i64::from(month);
    let doy = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The date of `days` since 1970-01-01, the inverse of [`days_from_civil`].
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instants_round_trip_and_impossible_ones_are_refused() {
        for (text, seconds) in [
            ("1970-01-01T00:00:00Z", 0),
            ("2025-09-24T08:00:00Z", 1_758_700_800),
            ("2000-02-29T23:59:59Z", 951_868_799),
        ] {
            assert_eq!(parse(text), Some(seconds), "{text}");
            assert_eq!(format(seconds).as_deref(), Some(text));
        }
        assert_eq!(
            parse("2026-09-24T00:51:16.925Z"),
            parse("2026-09-24T00:51:16Z")
        );
        assert_eq!(
            parse("2026-09-24T02:05:01+02:00"),
            parse("2026-09-24T00:05:01Z")
        );
        for bad in [
            "tomorrow",
            "2026-02-30T00:00:00Z",
            "2026-09-24T24:00:00Z",
            "2026-09-24",
        ] {
            assert_eq!(parse(bad), None, "{bad}");
        }
    }
}
