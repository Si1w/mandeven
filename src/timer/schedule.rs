//! Schedule grammar for timer-backed automation.
//!
//! Timers support one-shot `at`, fixed-interval `every`, and
//! Vixie-style `cron` expressions. The runtime form keeps the parsed
//! [`cron::Schedule`] eagerly compiled so per-tick `next_after` calls
//! are allocation-free; serde round-trips through `ScheduleSpec` so
//! the file store keeps just the raw fields.

use std::str::FromStr;

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Errors surfaced when constructing a [`Schedule`].
#[derive(Debug, Error)]
pub enum ScheduleError {
    /// `every` schedule was given a non-positive interval.
    #[error("interval must be greater than zero")]
    NonPositiveInterval,

    /// Cron expression was empty or whitespace-only.
    #[error("cron expression must not be empty")]
    EmptyCronExpression,

    /// Cron expression failed [`cron::Schedule`] parsing.
    #[error("invalid cron expression {expr:?}: {source}")]
    InvalidCronExpression {
        /// Original user input.
        expr: String,
        /// Parser failure from the `cron` crate.
        #[source]
        source: cron::error::Error,
    },
}

/// One scheduling rule attached to a timer.
///
/// Uses `ScheduleSpec` as its serde shape so persisted state only
/// stores raw fields. Construct via [`Schedule::at`],
/// [`Schedule::every`], or [`Schedule::cron`] so validation stays in
/// one place.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(try_from = "ScheduleSpec", into = "ScheduleSpec")]
pub enum Schedule {
    /// Fire once at an absolute UTC instant.
    At {
        /// Wall-clock instant at which the timer should fire.
        at: DateTime<Utc>,
    },

    /// Recurring fixed interval, anchored to a reference point.
    Every {
        /// Interval between consecutive fires.
        interval: Duration,
        /// Reference point for step alignment.
        anchor: DateTime<Utc>,
    },

    /// Recurring Vixie-style cron expression.
    Cron {
        /// Original user input, kept for round-tripping and status.
        expr: String,
        /// Pre-parsed schedule, used by [`Schedule::next_after`].
        compiled: Box<cron::Schedule>,
    },
}

impl Schedule {
    /// One-shot at the given UTC instant.
    #[must_use]
    pub fn at(at: DateTime<Utc>) -> Self {
        Self::At { at }
    }

    /// Recurring fixed interval anchored at `anchor`.
    ///
    /// # Errors
    ///
    /// Returns [`ScheduleError::NonPositiveInterval`] when `interval`
    /// is zero or negative.
    pub fn every(interval: Duration, anchor: DateTime<Utc>) -> Result<Self, ScheduleError> {
        if interval <= Duration::zero() {
            return Err(ScheduleError::NonPositiveInterval);
        }
        Ok(Self::Every { interval, anchor })
    }

    /// Vixie-style 5-field cron expression, or 6/7 fields accepted by
    /// the `cron` crate after expansion.
    ///
    /// # Errors
    ///
    /// - [`ScheduleError::EmptyCronExpression`] when the input trims
    ///   to nothing.
    /// - [`ScheduleError::InvalidCronExpression`] when the parser
    ///   rejects the expression.
    pub fn cron(expr: &str) -> Result<Self, ScheduleError> {
        let trimmed = expr.trim();
        if trimmed.is_empty() {
            return Err(ScheduleError::EmptyCronExpression);
        }
        let expanded = expand_to_seven_fields(trimmed);
        let compiled = cron::Schedule::from_str(&expanded).map_err(|source| {
            ScheduleError::InvalidCronExpression {
                expr: trimmed.to_string(),
                source,
            }
        })?;
        Ok(Self::Cron {
            expr: trimmed.to_string(),
            compiled: Box::new(compiled),
        })
    }

    /// Compute the next firing instant strictly after `now`.
    #[must_use]
    pub fn next_after(&self, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        match self {
            Self::At { at } => (*at > now).then_some(*at),
            Self::Every { interval, anchor } => Some(next_every_after(*anchor, *interval, now)),
            Self::Cron { compiled, .. } => compiled.after(&now).next(),
        }
    }

    /// Discriminator used by status output and tests.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::At { .. } => "at",
            Self::Every { .. } => "every",
            Self::Cron { .. } => "cron",
        }
    }

    /// Human-readable summary.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::At { at } => format!("at {}", at.to_rfc3339()),
            Self::Every { interval, .. } => format!("every {}s", interval.num_seconds()),
            Self::Cron { expr, .. } => format!("cron {expr}"),
        }
    }
}

fn next_every_after(
    anchor: DateTime<Utc>,
    interval: Duration,
    now: DateTime<Utc>,
) -> DateTime<Utc> {
    if now < anchor {
        return anchor;
    }
    let elapsed_ns = (now - anchor).num_nanoseconds().unwrap_or(i64::MAX);
    let interval_ns = interval.num_nanoseconds().unwrap_or(i64::MAX).max(1);
    let steps = (elapsed_ns / interval_ns).saturating_add(1);
    let offset_ns = steps.saturating_mul(interval_ns);
    anchor + Duration::nanoseconds(offset_ns)
}

fn expand_to_seven_fields(expr: &str) -> String {
    let field_count = expr.split_whitespace().count();
    match field_count {
        5 => format!("0 {expr} *"),
        6 => format!("{expr} *"),
        _ => expr.to_string(),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ScheduleSpec {
    At {
        at: DateTime<Utc>,
    },
    Every {
        interval_secs: i64,
        anchor: DateTime<Utc>,
    },
    Cron {
        expr: String,
    },
}

impl TryFrom<ScheduleSpec> for Schedule {
    type Error = ScheduleError;

    fn try_from(spec: ScheduleSpec) -> Result<Self, Self::Error> {
        match spec {
            ScheduleSpec::At { at } => Ok(Schedule::at(at)),
            ScheduleSpec::Every {
                interval_secs,
                anchor,
            } => Schedule::every(Duration::seconds(interval_secs), anchor),
            ScheduleSpec::Cron { expr } => Schedule::cron(&expr),
        }
    }
}

impl From<Schedule> for ScheduleSpec {
    fn from(schedule: Schedule) -> Self {
        match schedule {
            Schedule::At { at } => Self::At { at },
            Schedule::Every { interval, anchor } => Self::Every {
                interval_secs: interval.num_seconds(),
                anchor,
            },
            Schedule::Cron { expr, .. } => Self::Cron { expr },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(rfc: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(rfc)
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn at_returns_target_only_when_in_future() {
        let target = ts("2030-01-01T00:00:00Z");
        let s = Schedule::at(target);
        assert_eq!(s.next_after(ts("2026-01-01T00:00:00Z")), Some(target));
        assert_eq!(s.next_after(ts("2030-01-01T00:00:00Z")), None);
        assert_eq!(s.next_after(ts("2031-01-01T00:00:00Z")), None);
    }

    #[test]
    fn every_aligns_to_anchor_steps() {
        let anchor = ts("2026-04-25T00:00:00Z");
        let s = Schedule::every(Duration::minutes(5), anchor).unwrap();
        assert_eq!(s.next_after(anchor), Some(ts("2026-04-25T00:05:00Z")));
        assert_eq!(
            s.next_after(ts("2026-04-25T00:07:00Z")),
            Some(ts("2026-04-25T00:10:00Z"))
        );
    }

    #[test]
    fn every_before_anchor_returns_anchor() {
        let anchor = ts("2026-04-25T00:00:00Z");
        let s = Schedule::every(Duration::minutes(5), anchor).unwrap();
        assert_eq!(s.next_after(ts("2026-04-24T23:00:00Z")), Some(anchor));
    }

    #[test]
    fn every_rejects_non_positive_interval() {
        let anchor = ts("2026-04-25T00:00:00Z");
        assert!(matches!(
            Schedule::every(Duration::zero(), anchor),
            Err(ScheduleError::NonPositiveInterval)
        ));
        assert!(matches!(
            Schedule::every(Duration::seconds(-1), anchor),
            Err(ScheduleError::NonPositiveInterval)
        ));
    }

    #[test]
    fn cron_five_field_expression_fires_at_expected_minute() {
        let s = Schedule::cron("0 9 * * *").unwrap();
        let next = s.next_after(ts("2026-04-25T08:30:00Z")).unwrap();
        assert_eq!(next, ts("2026-04-25T09:00:00Z"));
    }

    #[test]
    fn cron_rejects_empty_and_invalid() {
        assert!(matches!(
            Schedule::cron(""),
            Err(ScheduleError::EmptyCronExpression)
        ));
        assert!(matches!(
            Schedule::cron("    "),
            Err(ScheduleError::EmptyCronExpression)
        ));
        assert!(matches!(
            Schedule::cron("not a cron"),
            Err(ScheduleError::InvalidCronExpression { .. })
        ));
    }

    #[test]
    fn schedule_round_trips_through_json() {
        let cases = vec![
            Schedule::at(ts("2030-01-01T00:00:00Z")),
            Schedule::every(Duration::minutes(15), ts("2026-04-25T00:00:00Z")).unwrap(),
            Schedule::cron("0 9 * * *").unwrap(),
        ];
        for original in cases {
            let json = serde_json::to_string(&original).unwrap();
            let restored: Schedule = serde_json::from_str(&json).unwrap();
            assert_eq!(restored.kind(), original.kind());
            assert_eq!(restored.describe(), original.describe());
        }
    }

    #[test]
    fn describe_reads_naturally() {
        assert_eq!(
            Schedule::at(ts("2030-01-01T00:00:00Z")).describe(),
            "at 2030-01-01T00:00:00+00:00"
        );
        assert_eq!(
            Schedule::every(Duration::minutes(5), ts("2026-04-25T00:00:00Z"))
                .unwrap()
                .describe(),
            "every 300s"
        );
        assert_eq!(
            Schedule::cron("0 9 * * *").unwrap().describe(),
            "cron 0 9 * * *"
        );
    }

    #[test]
    fn expand_to_seven_fields_is_field_count_aware() {
        assert_eq!(expand_to_seven_fields("0 9 * * *"), "0 0 9 * * * *");
        assert_eq!(expand_to_seven_fields("0 0 9 * * *"), "0 0 9 * * * *");
        assert_eq!(
            expand_to_seven_fields("0 0 9 * * * 2030"),
            "0 0 9 * * * 2030"
        );
    }
}
