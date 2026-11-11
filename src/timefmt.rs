//! Human-friendly time formatting ("il y a 3 heures", "hier", "la semaine dernière"…).

use chrono::{DateTime, Local, TimeZone};

fn plural(n: i64, one: &str, many: &str) -> String {
    format!("il y a {n} {}", if n == 1 { one } else { many })
}

/// Distance between `then` and `now`, in French. Calendar days are used from "hier" onwards,
/// so "hier" really means yesterday and not just "more than 24 hours ago".
pub fn relative<Tz: TimeZone>(now: &DateTime<Tz>, then: &DateTime<Tz>) -> String {
    let secs = now.clone().signed_duration_since(then.clone()).num_seconds();
    if secs < 60 {
        // Also covers commits dated slightly in the future (clock skew).
        return "à l'instant".into();
    }
    if secs < 3600 {
        return plural(secs / 60, "minute", "minutes");
    }
    if secs < 24 * 3600 {
        return plural(secs / 3600, "heure", "heures");
    }
    let days = (now.date_naive() - then.date_naive()).num_days().max(1);
    match days {
        1 => "hier".into(),
        2 => "avant-hier".into(),
        3..=6 => plural(days, "jour", "jours"),
        7..=13 => "la semaine dernière".into(),
        14..=29 => plural(days / 7, "semaine", "semaines"),
        30..=59 => "le mois dernier".into(),
        60..=364 => format!("il y a {} mois", (days / 30).min(11)),
        365..=729 => "l'année dernière".into(),
        _ => plural(days / 365, "an", "ans"),
    }
}

/// Relative time of a unix timestamp, relative to `now` (local time zone).
/// Pass the same `now` for every row of a frame so neighbours never straddle a boundary.
pub fn relative_from(now: &DateTime<Local>, secs: i64) -> String {
    match DateTime::from_timestamp(secs, 0) {
        Some(t) => relative(now, &t.with_timezone(&Local)),
        None => String::new(),
    }
}

pub fn relative_to_now(secs: i64) -> String {
    relative_from(&Local::now(), secs)
}

/// Absolute local date, e.g. `2026-11-11 16:48`.
pub fn absolute(secs: i64) -> String {
    DateTime::from_timestamp(secs, 0)
        .map(|d| d.with_timezone(&Local).format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{FixedOffset, NaiveDate};

    fn at(y: i32, m: u32, d: u32, h: u32, min: u32) -> DateTime<FixedOffset> {
        let tz = FixedOffset::east_opt(3600).unwrap();
        tz.from_local_datetime(&NaiveDate::from_ymd_opt(y, m, d).unwrap().and_hms_opt(h, min, 0).unwrap())
            .unwrap()
    }

    fn rel(then: DateTime<FixedOffset>) -> String {
        // "now" is Wednesday 2026-11-11 16:00.
        relative(&at(2026, 11, 11, 16, 0), &then)
    }

    #[test]
    fn seconds_and_future_are_now() {
        assert_eq!(rel(at(2026, 11, 11, 16, 0)), "à l'instant");
        assert_eq!(rel(at(2026, 11, 11, 15, 59)), "il y a 1 minute"); // 60 s
        assert_eq!(rel(at(2026, 11, 11, 17, 30)), "à l'instant"); // clock skew
    }

    #[test]
    fn minutes() {
        assert_eq!(rel(at(2026, 11, 11, 15, 55)), "il y a 5 minutes");
        assert_eq!(rel(at(2026, 11, 11, 15, 1)), "il y a 59 minutes");
    }

    #[test]
    fn hours_even_across_midnight() {
        assert_eq!(rel(at(2026, 11, 11, 15, 0)), "il y a 1 heure");
        assert_eq!(rel(at(2026, 11, 11, 13, 30)), "il y a 2 heures");
        assert_eq!(rel(at(2026, 11, 10, 20, 0)), "il y a 20 heures"); // yesterday evening, < 24 h
    }

    #[test]
    fn yesterday_and_day_before() {
        assert_eq!(rel(at(2026, 11, 10, 15, 0)), "hier"); // 25 h
        assert_eq!(rel(at(2026, 11, 10, 0, 5)), "hier");
        assert_eq!(rel(at(2026, 11, 9, 12, 0)), "avant-hier");
    }

    #[test]
    fn days_weeks_months_years() {
        assert_eq!(rel(at(2026, 11, 8, 12, 0)), "il y a 3 jours");
        assert_eq!(rel(at(2026, 11, 5, 12, 0)), "il y a 6 jours");
        assert_eq!(rel(at(2026, 11, 4, 12, 0)), "la semaine dernière"); // 7 days
        assert_eq!(rel(at(2026, 10, 29, 12, 0)), "la semaine dernière"); // 13 days
        assert_eq!(rel(at(2026, 10, 28, 12, 0)), "il y a 2 semaines");
        assert_eq!(rel(at(2026, 10, 14, 12, 0)), "il y a 4 semaines"); // 28 days
        assert_eq!(rel(at(2026, 10, 10, 12, 0)), "le mois dernier"); // 32 days
        assert_eq!(rel(at(2026, 8, 11, 12, 0)), "il y a 3 mois");
        assert_eq!(rel(at(2025, 11, 20, 12, 0)), "il y a 11 mois");
        assert_eq!(rel(at(2025, 11, 11, 12, 0)), "l'année dernière"); // 365 days
        assert_eq!(rel(at(2023, 11, 11, 12, 0)), "il y a 3 ans");
    }

    #[test]
    fn plural_forms() {
        assert_eq!(plural(1, "an", "ans"), "il y a 1 an");
        assert_eq!(plural(2, "an", "ans"), "il y a 2 ans");
    }
}
