//! Schedules for unattended tasks, evaluated in local time.

use std::time::Duration;

use chrono::{Datelike, Days, NaiveDateTime, NaiveTime, Weekday};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Schedule {
    Daily(NaiveTime),
    Weekly(Weekday, NaiveTime),
    Every(Duration),
}

const MIN_INTERVAL: Duration = Duration::from_secs(15 * 60);

fn parse_time(s: &str) -> Result<NaiveTime, String> {
    NaiveTime::parse_from_str(s.trim(), "%H:%M").map_err(|_| format!("{s:?} is not HH:MM"))
}

impl std::str::FromStr for Schedule {
    type Err = String;

    /// `daily 09:00`, `weekly mon 08:30`, `every 6h`, `every 30m`.
    fn from_str(s: &str) -> Result<Self, String> {
        let parts: Vec<&str> = s.split_whitespace().collect();
        match parts.as_slice() {
            ["daily", t] => Ok(Schedule::Daily(parse_time(t)?)),
            ["weekly", day, t] => {
                let wd: Weekday = day
                    .parse()
                    .map_err(|_| format!("{day:?} is not a weekday"))?;
                Ok(Schedule::Weekly(wd, parse_time(t)?))
            }
            ["every", n] => {
                let (num, unit) = n.split_at(n.len().saturating_sub(1));
                let num: u64 = num
                    .parse()
                    .map_err(|_| format!("{n:?} is not like 6h or 30m"))?;
                let d = match unit {
                    "h" => Duration::from_secs(num * 3600),
                    "m" => Duration::from_secs(num * 60),
                    _ => return Err(format!("{n:?} is not like 6h or 30m")),
                };
                if d < MIN_INTERVAL {
                    return Err("the shortest interval is 15m".into());
                }
                Ok(Schedule::Every(d))
            }
            _ => Err(format!(
                "unknown schedule {s:?}; use \"daily 09:00\", \"weekly mon 09:00\" or \"every 6h\""
            )),
        }
    }
}

impl std::fmt::Display for Schedule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Schedule::Daily(t) => write!(f, "daily {}", t.format("%H:%M")),
            Schedule::Weekly(d, t) => {
                write!(
                    f,
                    "weekly {} {}",
                    d.to_string().to_lowercase(),
                    t.format("%H:%M")
                )
            }
            Schedule::Every(d) if d.as_secs() % 3600 == 0 => {
                write!(f, "every {}h", d.as_secs() / 3600)
            }
            Schedule::Every(d) => write!(f, "every {}m", d.as_secs() / 60),
        }
    }
}

impl Schedule {
    /// The most recent scheduled instant at or before `now`.
    fn last_occurrence(&self, now: NaiveDateTime) -> Option<NaiveDateTime> {
        match *self {
            Schedule::Daily(t) => {
                let today = now.date().and_time(t);
                Some(if today <= now {
                    today
                } else {
                    today.checked_sub_days(Days::new(1))?
                })
            }
            Schedule::Weekly(wd, t) => {
                let back =
                    (now.weekday().num_days_from_monday() + 7 - wd.num_days_from_monday()) % 7;
                let candidate = now
                    .date()
                    .checked_sub_days(Days::new(back.into()))?
                    .and_time(t);
                Some(if candidate <= now {
                    candidate
                } else {
                    candidate.checked_sub_days(Days::new(7))?
                })
            }
            Schedule::Every(_) => None,
        }
    }

    /// Due when a scheduled instant has passed since the task last ran (or
    /// was created). Several missed instants still mean a single run.
    pub fn is_due(&self, anchor: NaiveDateTime, now: NaiveDateTime) -> bool {
        match self {
            Schedule::Every(d) => chrono::Duration::from_std(*d)
                .map(|d| now >= anchor + d)
                .unwrap_or(false),
            _ => self.last_occurrence(now).is_some_and(|occ| anchor < occ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap()
    }

    #[test]
    fn parses_and_displays_round_trip() {
        for s in ["daily 09:00", "weekly mon 08:30", "every 6h", "every 45m"] {
            assert_eq!(s.parse::<Schedule>().unwrap().to_string(), s);
        }
        assert!("every 5m".parse::<Schedule>().is_err());
        assert!("daily 25:00".parse::<Schedule>().is_err());
        assert!("hourly".parse::<Schedule>().is_err());
    }

    #[test]
    fn daily_is_due_once_per_day_after_its_time() {
        let s: Schedule = "daily 09:00".parse().unwrap();
        let created = at("2026-10-16 10:00");
        assert!(
            !s.is_due(created, at("2026-10-16 23:00")),
            "created after today's slot"
        );
        assert!(s.is_due(created, at("2026-10-17 09:00")));
        let ran = at("2026-10-17 09:00");
        assert!(!s.is_due(ran, at("2026-10-17 18:00")));
        assert!(
            s.is_due(ran, at("2026-10-20 07:00")),
            "missed days catch up once"
        );
    }

    #[test]
    fn weekly_and_interval_schedules() {
        let w: Schedule = "weekly mon 08:00".parse().unwrap();
        // 2026-10-19 is a Monday.
        assert!(!w.is_due(at("2026-10-16 12:00"), at("2026-10-19 07:59")));
        assert!(w.is_due(at("2026-10-16 12:00"), at("2026-10-19 08:00")));
        assert!(!w.is_due(at("2026-10-19 08:00"), at("2026-10-25 23:00")));
        let e: Schedule = "every 6h".parse().unwrap();
        assert!(!e.is_due(at("2026-10-17 00:00"), at("2026-10-17 05:59")));
        assert!(e.is_due(at("2026-10-17 00:00"), at("2026-10-17 06:00")));
    }
}
