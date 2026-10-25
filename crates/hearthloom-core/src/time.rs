//! Event-time representation.
//!
//! Every episode timestamp carries three things, none optional at the type
//! level: the instant in UTC, the local UTC offset at the event (so day
//! views group by the user's day, not Greenwich's), and an uncertainty
//! grade (chat exports and photo sidecars routinely have day- or
//! month-level precision; pretending otherwise corrupts recall).

use anyhow::{anyhow, Result};
use chrono::{DateTime, FixedOffset, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};

/// How precisely the event time is known.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimeUncertainty {
    /// Known to the second (or better).
    Exact,
    Minute,
    Hour,
    /// Date known, time-of-day not (all-day calendar events, photo sidecars).
    Day,
    Month,
    Year,
    /// No usable event time; ingest time is the only anchor.
    Unknown,
}

impl TimeUncertainty {
    pub fn as_str(&self) -> &'static str {
        match self {
            TimeUncertainty::Exact => "exact",
            TimeUncertainty::Minute => "minute",
            TimeUncertainty::Hour => "hour",
            TimeUncertainty::Day => "day",
            TimeUncertainty::Month => "month",
            TimeUncertainty::Year => "year",
            TimeUncertainty::Unknown => "unknown",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "exact" => TimeUncertainty::Exact,
            "minute" => TimeUncertainty::Minute,
            "hour" => TimeUncertainty::Hour,
            "day" => TimeUncertainty::Day,
            "month" => TimeUncertainty::Month,
            "year" => TimeUncertainty::Year,
            "unknown" => TimeUncertainty::Unknown,
            other => return Err(anyhow!("unknown time uncertainty: {other}")),
        })
    }
}

/// An event timestamp: UTC instant + local offset + uncertainty.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventTime {
    /// RFC 3339 in UTC, e.g. `2026-03-15T18:04:00Z`. None only when
    /// uncertainty is `unknown`.
    pub utc: Option<String>,
    /// Local UTC offset in minutes at the event (e.g. -300 for EST).
    pub tz_offset_minutes: Option<i32>,
    pub uncertainty: TimeUncertainty,
}

impl EventTime {
    /// From a timezone-aware instant; the offset is preserved.
    pub fn from_fixed(dt: DateTime<FixedOffset>, uncertainty: TimeUncertainty) -> Self {
        EventTime {
            utc: Some(
                dt.with_timezone(&Utc)
                    .to_rfc3339_opts(SecondsFormat::Secs, true),
            ),
            tz_offset_minutes: Some(dt.offset().local_minus_utc() / 60),
            uncertainty,
        }
    }

    /// Parse an RFC 3339 / ISO-8601 string with offset (e.g. git's `%aI`).
    pub fn parse_rfc3339(s: &str, uncertainty: TimeUncertainty) -> Result<Self> {
        let dt = DateTime::parse_from_rfc3339(s)
            .map_err(|e| anyhow!("bad timestamp {s:?}: {e}"))?;
        Ok(Self::from_fixed(dt, uncertainty))
    }

    /// No event time at all.
    pub fn unknown() -> Self {
        EventTime {
            utc: None,
            tz_offset_minutes: None,
            uncertainty: TimeUncertainty::Unknown,
        }
    }
}

/// Current wall-clock time as an RFC 3339 UTC string (for ingest_time,
/// t_created, and other system timestamps).
pub fn now_utc() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// The Monday of the current UTC week, as YYYY-MM-DD — the weekly
/// review's idempotency key (source_ref review:<monday>).
pub fn current_week_start() -> String {
    use chrono::Datelike;
    let today = Utc::now().date_naive();
    let monday =
        today - chrono::Duration::days(today.weekday().num_days_from_monday() as i64);
    monday.format("%Y-%m-%d").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_preserves_offset() {
        let t = EventTime::parse_rfc3339("2026-03-15T14:30:00-04:00", TimeUncertainty::Exact)
            .unwrap();
        assert_eq!(t.utc.as_deref(), Some("2026-03-15T18:30:00Z"));
        assert_eq!(t.tz_offset_minutes, Some(-240));
        assert_eq!(t.uncertainty, TimeUncertainty::Exact);
    }

    #[test]
    fn test_uncertainty_roundtrip() {
        for u in [
            TimeUncertainty::Exact,
            TimeUncertainty::Day,
            TimeUncertainty::Unknown,
        ] {
            assert_eq!(TimeUncertainty::parse(u.as_str()).unwrap(), u);
        }
        assert!(TimeUncertainty::parse("fuzzy").is_err());
    }
}
