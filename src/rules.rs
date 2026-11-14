//! Archiving rules shared by sources: which messages to archive (date
//! window) and which archived messages to delete from the source.

use chrono::{Local, NaiveDate, TimeZone};
use serde::{Deserialize, Serialize};

/// Archive only messages inside this window (all bounds optional).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DateWindow {
    /// Only messages on or after this date (YYYY-MM-DD).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
    /// Only messages before this date (YYYY-MM-DD).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<String>,
    /// Only messages at least this many days old.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub older_than_days: Option<u32>,
}

/// Delete messages from the source after they are safely archived.
/// Flagged messages are never deleted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeleteRule {
    /// Only delete messages at least this many days old (0 = any age).
    #[serde(default)]
    pub older_than_days: u32,
    /// Also delete unread messages (default: keep unread mail).
    #[serde(default)]
    pub include_unread: bool,
}

fn parse_date(s: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d").ok()
}

fn local_midnight(d: NaiveDate) -> i64 {
    Local.from_local_datetime(&d.and_hms_opt(0, 0, 0).unwrap()).earliest().map(|t| t.timestamp()).unwrap_or(0)
}

impl DateWindow {
    pub fn is_empty(&self) -> bool {
        self.after.is_none() && self.before.is_none() && self.older_than_days.is_none()
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        for d in [&self.after, &self.before].into_iter().flatten() {
            if parse_date(d).is_none() {
                anyhow::bail!("dates must look like 2024-01-31 (got {d:?})");
            }
        }
        Ok(())
    }

    /// Unix-second bounds [lo, hi) at time `now`.
    pub fn bounds(&self, now: i64) -> (Option<i64>, Option<i64>) {
        let lo = self.after.as_deref().and_then(parse_date).map(local_midnight);
        let mut hi = self.before.as_deref().and_then(parse_date).map(local_midnight);
        if let Some(days) = self.older_than_days {
            let cut = now - days as i64 * 86_400;
            hi = Some(hi.map_or(cut, |h| h.min(cut)));
        }
        (lo, hi)
    }

    pub fn bounds_now(&self) -> (Option<i64>, Option<i64>) {
        self.bounds(chrono::Utc::now().timestamp())
    }

    pub fn contains(&self, ts: i64) -> bool {
        let (lo, hi) = self.bounds_now();
        lo.is_none_or(|l| ts >= l) && hi.is_none_or(|h| ts < h)
    }

    /// IMAP SEARCH criteria (SINCE/BEFORE, by internal date).
    pub fn imap_criteria(&self) -> String {
        let (lo, hi) = self.bounds_now();
        let mut c = String::new();
        if let Some(l) = lo {
            c.push_str(&format!(" SINCE {}", imap_date(l)));
        }
        if let Some(h) = hi {
            c.push_str(&format!(" BEFORE {}", imap_date(h)));
        }
        c
    }
}

impl DeleteRule {
    /// Messages received before this instant may be deleted.
    pub fn cutoff(&self) -> i64 {
        chrono::Utc::now().timestamp() - self.older_than_days as i64 * 86_400
    }
}

/// IMAP date like 5-Mar-2019 (local calendar day of `ts`).
pub fn imap_date(ts: i64) -> String {
    Local.timestamp_opt(ts, 0).single().map(|d| d.format("%-d-%b-%Y").to_string()).unwrap_or_else(|| "1-Jan-1970".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_bounds() {
        let now = 1_800_000_000;
        let w = DateWindow { after: Some("2020-01-01".into()), before: Some("2021-01-01".into()), older_than_days: None };
        let (lo, hi) = w.bounds(now);
        assert_eq!(lo, Some(local_midnight(NaiveDate::from_ymd_opt(2020, 1, 1).unwrap())));
        assert_eq!(hi, Some(local_midnight(NaiveDate::from_ymd_opt(2021, 1, 1).unwrap())));
        let w = DateWindow { older_than_days: Some(30), ..Default::default() };
        assert_eq!(w.bounds(now).1, Some(now - 30 * 86_400));
        assert!(DateWindow { after: Some("2020/01/01".into()), ..Default::default() }.validate().is_err());
    }

    #[test]
    fn imap_dates() {
        let ts = local_midnight(NaiveDate::from_ymd_opt(2019, 3, 5).unwrap()) + 3600;
        assert_eq!(imap_date(ts), "5-Mar-2019");
    }
}
