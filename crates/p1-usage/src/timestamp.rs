//! Minimal RFC 3339 seconds precision for timestamps, without optional time crates.
use time::{Date, Month, OffsetDateTime, PrimitiveDateTime, Time, UtcOffset};

pub fn format(date: OffsetDateTime) -> String {
    let date = date.to_offset(UtcOffset::UTC);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        date.year(),
        date.month() as u8,
        date.day(),
        date.hour(),
        date.minute(),
        date.second()
    )
}

pub fn parse(text: &str) -> Option<OffsetDateTime> {
    let (day, clock) = text.split_once('T')?;
    let mut date = day.split('-');
    let year = date.next()?.parse().ok()?;
    let month = Month::try_from(date.next()?.parse::<u8>().ok()?).ok()?;
    let day = date.next()?.parse().ok()?;
    let (clock, offset) = if let Some(clock) = clock.strip_suffix('Z') {
        (clock, UtcOffset::UTC)
    } else {
        let pos = clock.rfind(['+', '-'])?;
        let (clock, zone) = clock.split_at(pos);
        let (hours, minutes) = zone[1..].split_once(':')?;
        let sign = if zone.starts_with('-') { -1 } else { 1 };
        (
            clock,
            UtcOffset::from_hms(
                sign * hours.parse::<i8>().ok()?,
                sign * minutes.parse::<i8>().ok()?,
                0,
            )
            .ok()?,
        )
    };
    let mut parts = clock.split(':');
    let hour = parts.next()?.parse().ok()?;
    let minute = parts.next()?.parse().ok()?;
    let second = parts.next()?.split('.').next()?.parse().ok()?;
    Some(
        PrimitiveDateTime::new(
            Date::from_calendar_date(year, month, day).ok()?,
            Time::from_hms(hour, minute, second).ok()?,
        )
        .assume_offset(offset),
    )
}
