//! Pure calendar projections shared by summary dispatch and periodic recovery.
//!
//! All projections receive the workflow's resolved IANA timezone.  This keeps
//! recovery identities aligned with the periods the worker actually targets.

use chrono::{DateTime, Datelike as _, Duration, NaiveDate, Utc};
use chrono_tz::Tz;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PeriodKind {
    Daily,
    Weekly,
    Monthly,
}

impl PeriodKind {
    pub(crate) fn key(self) -> &'static str {
        match self {
            Self::Daily => "daily",
            Self::Weekly => "weekly",
            Self::Monthly => "monthly",
        }
    }
}

pub(crate) fn completed_periods(
    kind: PeriodKind,
    count: i32,
    now: DateTime<Utc>,
    timezone: Tz,
) -> Vec<String> {
    let count = i64::from(count.max(1));
    let today = now.with_timezone(&timezone).date_naive();
    match kind {
        PeriodKind::Daily => (1..=count)
            .rev()
            .map(|offset| {
                (today - Duration::days(offset))
                    .format("%Y-%m-%d")
                    .to_string()
            })
            .collect(),
        PeriodKind::Weekly => {
            let monday_offset = i64::from(today.weekday().num_days_from_monday());
            (1..=count)
                .rev()
                .map(|offset| today - Duration::days(monday_offset + offset * 7))
                .map(iso_week_key)
                .collect()
        }
        PeriodKind::Monthly => (1..=count)
            .rev()
            .map(|offset| month_key(month_index(today) - offset as i32))
            .collect(),
    }
}

pub(crate) fn explicit_periods(kind: PeriodKind, start: &str, end: &str) -> Option<Vec<String>> {
    match kind {
        PeriodKind::Daily => {
            let mut day = NaiveDate::parse_from_str(start, "%Y-%m-%d").ok()?;
            let end = NaiveDate::parse_from_str(end, "%Y-%m-%d").ok()?;
            (day <= end).then_some(())?;
            let mut periods = Vec::new();
            while day <= end {
                periods.push(day.format("%Y-%m-%d").to_string());
                day += Duration::days(1);
            }
            Some(periods)
        }
        PeriodKind::Weekly => {
            let parse =
                |value: &str| NaiveDate::parse_from_str(&format!("{value}-1"), "%G-W%V-%u").ok();
            let mut day = parse(start)?;
            let end = parse(end)?;
            (day <= end).then_some(())?;
            let mut periods = Vec::new();
            while day <= end {
                periods.push(iso_week_key(day));
                day += Duration::weeks(1);
            }
            Some(periods)
        }
        PeriodKind::Monthly => {
            let parse = |value: &str| {
                let (year, month) = value.split_once('-')?;
                let year = year.parse::<i32>().ok()?;
                let month = month.parse::<u32>().ok()?;
                (1..=12)
                    .contains(&month)
                    .then_some(year * 12 + month as i32 - 1)
            };
            let mut month = parse(start)?;
            let end = parse(end)?;
            (month <= end).then_some(())?;
            let mut periods = Vec::new();
            while month <= end {
                periods.push(month_key(month));
                month += 1;
            }
            Some(periods)
        }
    }
}

/// The full dependency plan for a periodic task that ended without a batch
/// result. Weekly and monthly plans include prerequisite periods, matching the
/// periodic workflow's generation order.
pub(crate) fn periodic_failure_periods(
    task_kind: &str,
    run_daily: bool,
    lookback_days: i32,
    triggered_at_ms: i64,
    timezone: Tz,
) -> Vec<(PeriodKind, String)> {
    let today = DateTime::<Utc>::from_timestamp_millis(triggered_at_ms)
        .unwrap_or_else(Utc::now)
        .with_timezone(&timezone)
        .date_naive();
    match task_kind {
        "regular" if run_daily => {
            let count = lookback_days.max(1);
            let offsets: Vec<_> = if lookback_days == 0 {
                vec![0]
            } else {
                (1..=count).rev().collect()
            };
            offsets
                .into_iter()
                .map(|offset| {
                    (
                        PeriodKind::Daily,
                        (today - Duration::days(i64::from(offset)))
                            .format("%Y-%m-%d")
                            .to_string(),
                    )
                })
                .collect()
        }
        "weekly" => weekly_plan(today),
        "monthly" => monthly_plan(today),
        _ => Vec::new(),
    }
}

fn weekly_plan(today: NaiveDate) -> Vec<(PeriodKind, String)> {
    let monday = today - Duration::days(i64::from(today.weekday().num_days_from_monday()) + 7);
    let mut periods = (0..7)
        .map(|offset| {
            (
                PeriodKind::Daily,
                (monday + Duration::days(offset))
                    .format("%Y-%m-%d")
                    .to_string(),
            )
        })
        .collect::<Vec<_>>();
    periods.push((PeriodKind::Weekly, iso_week_key(monday)));
    periods
}

fn monthly_plan(today: NaiveDate) -> Vec<(PeriodKind, String)> {
    let target = month_index(today) - 1;
    let year = target.div_euclid(12);
    let month = target.rem_euclid(12) + 1;
    let first_day =
        NaiveDate::from_ymd_opt(year, month as u32, 1).expect("calculated month is valid");
    let next_month = NaiveDate::from_ymd_opt(
        year + (month == 12) as i32,
        if month == 12 { 1 } else { month as u32 + 1 },
        1,
    )
    .expect("next month is valid");
    let last_day = next_month - Duration::days(1);
    let first_monday =
        first_day - Duration::days(i64::from(first_day.weekday().num_days_from_monday()));
    let last_monday =
        last_day - Duration::days(i64::from(last_day.weekday().num_days_from_monday()));
    let mut periods = (0..=(last_day - first_monday).num_days())
        .map(|offset| {
            (
                PeriodKind::Daily,
                (first_monday + Duration::days(offset))
                    .format("%Y-%m-%d")
                    .to_string(),
            )
        })
        .collect::<Vec<_>>();
    periods.extend(
        (0..=((last_monday - first_monday).num_days() / 7)).map(|offset| {
            (
                PeriodKind::Weekly,
                iso_week_key(first_monday + Duration::weeks(offset)),
            )
        }),
    );
    periods.push((PeriodKind::Monthly, month_key(target)));
    periods
}

fn month_index(day: NaiveDate) -> i32 {
    day.year() * 12 + day.month0() as i32
}

fn month_key(index: i32) -> String {
    format!(
        "{:04}-{:02}",
        index.div_euclid(12),
        index.rem_euclid(12) + 1
    )
}

fn iso_week_key(day: NaiveDate) -> String {
    let iso = day.iso_week();
    format!("{}-W{:02}", iso.year(), iso.week())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn instant() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-09-04T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn completed_periods_use_the_workflow_timezone_at_a_date_boundary() {
        assert_eq!(
            completed_periods(PeriodKind::Daily, 1, instant(), chrono_tz::Asia::Tokyo),
            ["2026-09-03"]
        );
        assert_eq!(
            completed_periods(
                PeriodKind::Daily,
                1,
                instant(),
                chrono_tz::America::New_York
            ),
            ["2026-09-02"]
        );
    }

    #[test]
    fn monthly_plan_includes_the_covering_iso_week_dependencies() {
        let periods = periodic_failure_periods(
            "monthly",
            true,
            1,
            instant().timestamp_millis(),
            chrono_tz::Asia::Tokyo,
        );
        assert!(periods.contains(&(PeriodKind::Daily, "2026-07-27".into())));
        assert!(periods.contains(&(PeriodKind::Weekly, "2026-W36".into())));
        assert!(periods.contains(&(PeriodKind::Monthly, "2026-08".into())));
    }
}
