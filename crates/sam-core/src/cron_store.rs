//! Cron/reminder job persistence and scheduling.

use std::path::PathBuf;

use chrono::{Datelike, Local, NaiveDateTime, TimeZone, Timelike};
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};
use uuid::Uuid;

use crate::state_dir;

/// A scheduled job (one-shot reminder or recurring cron task).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronJob {
    pub id: String,
    pub message: String,
    pub handle: String,
    pub schedule: CronSchedule,
    pub repeat: bool,
    pub created_at: i64,
    pub last_fired_at: Option<i64>,
}

/// How the job is scheduled.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum CronSchedule {
    /// Standard 5-field cron: min hour dom month dow
    Cron { expr: String },
    /// Fire once at specific unix timestamp (UTC).
    Once { at_unix: i64 },
}

/// Persistent store for cron jobs.
pub struct CronStore {
    jobs: Vec<CronJob>,
    path: PathBuf,
}

impl CronStore {
    /// Load from disk or create empty.
    pub fn load() -> Self {
        let path = state_dir().join("cron_jobs.json");
        let jobs = if path.exists() {
            match std::fs::read_to_string(&path) {
                Ok(s) => serde_json::from_str(&s).unwrap_or_else(|e| {
                    warn!("failed to parse cron_jobs.json: {e}");
                    Vec::new()
                }),
                Err(e) => {
                    warn!("failed to read cron_jobs.json: {e}");
                    Vec::new()
                }
            }
        } else {
            Vec::new()
        };
        debug!(count = jobs.len(), "CronStore loaded");
        Self { jobs, path }
    }

    /// Persist to disk.
    pub fn save(&self) {
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match serde_json::to_string_pretty(&self.jobs) {
            Ok(json) => {
                if let Err(e) = std::fs::write(&self.path, json) {
                    warn!("failed to write cron_jobs.json: {e}");
                }
            }
            Err(e) => warn!("failed to serialize cron jobs: {e}"),
        }
    }

    /// Add a new job and persist.
    pub fn add(&mut self, job: CronJob) -> String {
        let id = job.id.clone();
        self.jobs.push(job);
        self.save();
        id
    }

    /// Remove a job by id.
    pub fn remove(&mut self, id: &str) -> bool {
        let before = self.jobs.len();
        self.jobs.retain(|j| j.id != id);
        let removed = self.jobs.len() < before;
        if removed {
            self.save();
        }
        removed
    }

    /// Remove a job by message substring match (case-insensitive).
    pub fn remove_by_message(&mut self, query: &str) -> Option<CronJob> {
        let lower = query.to_lowercase();
        let idx = self.jobs.iter().position(|j| j.message.to_lowercase().contains(&lower));
        if let Some(i) = idx {
            let job = self.jobs.remove(i);
            self.save();
            Some(job)
        } else {
            None
        }
    }

    /// List all active jobs.
    pub fn list(&self) -> &[CronJob] {
        &self.jobs
    }

    /// Find jobs that are due at the given unix timestamp.
    /// For `Once`: fires if `at_unix <= now` and never fired.
    /// For `Cron`: fires if current minute matches the expression.
    pub fn due_jobs(&self, now_unix: i64) -> Vec<&CronJob> {
        self.jobs
            .iter()
            .filter(|job| self.is_due(job, now_unix))
            .collect()
    }

    /// Mark a job as fired.
    pub fn mark_fired(&mut self, id: &str, now_unix: i64) {
        if let Some(job) = self.jobs.iter_mut().find(|j| j.id == id) {
            job.last_fired_at = Some(now_unix);
        }
    }

    /// Remove non-repeating jobs that have already fired.
    pub fn cleanup_fired(&mut self) {
        let before = self.jobs.len();
        self.jobs.retain(|j| j.repeat || j.last_fired_at.is_none());
        if self.jobs.len() < before {
            self.save();
        }
    }

    fn is_due(&self, job: &CronJob, now_unix: i64) -> bool {
        match &job.schedule {
            CronSchedule::Once { at_unix } => {
                // Fire if time has arrived and not yet fired.
                *at_unix <= now_unix && job.last_fired_at.is_none()
            }
            CronSchedule::Cron { expr } => {
                // Only fire once per minute — check last_fired_at.
                if let Some(last) = job.last_fired_at {
                    // Already fired within this minute.
                    if now_unix - last < 60 {
                        return false;
                    }
                }
                cron_matches(expr, now_unix)
            }
        }
    }
}

/// Check if a 5-field cron expression matches the given unix timestamp.
/// Fields: minute hour day-of-month month day-of-week
/// Supports: `*`, specific numbers, comma-separated lists, ranges (e.g. 1-5),
/// and step values (e.g. */5).
pub fn cron_matches(expr: &str, now_unix: i64) -> bool {
    let local = match Local.timestamp_opt(now_unix, 0) {
        chrono::LocalResult::Single(t) => t,
        _ => return false,
    };

    let fields: Vec<&str> = expr.split_whitespace().collect();
    if fields.len() != 5 {
        return false;
    }

    let minute = local.minute();
    let hour = local.hour();
    let dom = local.day();
    let month = local.month();
    let dow = local.weekday().num_days_from_sunday(); // 0=Sun

    field_matches(fields[0], minute, 0, 59)
        && field_matches(fields[1], hour, 0, 23)
        && field_matches(fields[2], dom, 1, 31)
        && field_matches(fields[3], month, 1, 12)
        && field_matches(fields[4], dow, 0, 6)
}

/// Check if a single cron field matches a value.
fn field_matches(field: &str, value: u32, min: u32, max: u32) -> bool {
    if field == "*" {
        return true;
    }

    // Handle comma-separated values: "1,3,5"
    for part in field.split(',') {
        if part_matches(part.trim(), value, min, max) {
            return true;
        }
    }
    false
}

fn part_matches(part: &str, value: u32, min: u32, max: u32) -> bool {
    // Step: */5 or 1-10/2
    if let Some((range_part, step_str)) = part.split_once('/') {
        let step: u32 = match step_str.parse() {
            Ok(s) if s > 0 => s,
            _ => return false,
        };
        let (start, end) = if range_part == "*" {
            (min, max)
        } else if let Some((a, b)) = range_part.split_once('-') {
            match (a.parse::<u32>(), b.parse::<u32>()) {
                (Ok(a), Ok(b)) => (a, b),
                _ => return false,
            }
        } else {
            return false;
        };
        // Check if value is in range and matches step.
        if value >= start && value <= end && (value - start).is_multiple_of(step) {
            return true;
        }
        return false;
    }

    // Range: 1-5
    if let Some((a, b)) = part.split_once('-') {
        if let (Ok(start), Ok(end)) = (a.parse::<u32>(), b.parse::<u32>()) {
            return value >= start && value <= end;
        }
        return false;
    }

    // Exact number
    if let Ok(n) = part.parse::<u32>() {
        return value == n;
    }

    false
}

/// Parse a user-friendly datetime string into a unix timestamp (KST).
/// Supports: "2026-03-03T09:00:00", "2026-03-03 09:00", "2026-03-03"
pub fn parse_datetime_to_unix(s: &str) -> Option<i64> {
    // Try ISO 8601 with T separator
    if let Ok(dt) = NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S") {
        return Local.from_local_datetime(&dt).single().map(|t| t.timestamp());
    }
    // Try space-separated
    if let Ok(dt) = NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S") {
        return Local.from_local_datetime(&dt).single().map(|t| t.timestamp());
    }
    if let Ok(dt) = NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M") {
        return Local.from_local_datetime(&dt).single().map(|t| t.timestamp());
    }
    // Date only (default to 09:00)
    if let Ok(date) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        let dt = date.and_hms_opt(9, 0, 0)?;
        return Local.from_local_datetime(&dt).single().map(|t| t.timestamp());
    }
    None
}

/// Create a new CronJob helper.
pub fn new_job(handle: &str, message: &str, schedule: CronSchedule, repeat: bool) -> CronJob {
    CronJob {
        id: Uuid::new_v4().to_string(),
        message: message.to_string(),
        handle: handle.to_string(),
        schedule,
        repeat,
        created_at: chrono::Utc::now().timestamp(),
        last_fired_at: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cron_every_minute_matches() {
        let now = chrono::Local::now().timestamp();
        assert!(cron_matches("* * * * *", now));
    }

    #[test]
    fn cron_specific_time() {
        // 2026-04-27 09:00 KST (Sunday)
        let dt = chrono::NaiveDate::from_ymd_opt(2026, 4, 27)
            .unwrap()
            .and_hms_opt(9, 0, 0)
            .unwrap();
        let ts = Local.from_local_datetime(&dt).single().unwrap().timestamp();
        assert!(cron_matches("0 9 * * *", ts));
        assert!(!cron_matches("30 9 * * *", ts));
        assert!(cron_matches("0 9 27 4 *", ts));
    }

    #[test]
    fn cron_step_values() {
        let dt = chrono::NaiveDate::from_ymd_opt(2026, 4, 27)
            .unwrap()
            .and_hms_opt(9, 15, 0)
            .unwrap();
        let ts = Local.from_local_datetime(&dt).single().unwrap().timestamp();
        assert!(cron_matches("*/15 * * * *", ts));
        assert!(!cron_matches("*/7 * * * *", ts));
    }

    #[test]
    fn parse_datetime_variants() {
        assert!(parse_datetime_to_unix("2026-03-03T09:00:00").is_some());
        assert!(parse_datetime_to_unix("2026-03-03 09:00").is_some());
        assert!(parse_datetime_to_unix("2026-03-03").is_some());
        assert!(parse_datetime_to_unix("invalid").is_none());
    }

    #[test]
    fn once_job_fires_once() {
        let mut store = CronStore {
            jobs: Vec::new(),
            path: std::env::temp_dir().join("sam-test-cron.json"),
        };
        let job = new_job("+8210", "test", CronSchedule::Once { at_unix: 100 }, false);
        store.add(job);

        // Due at t=100
        let due = store.due_jobs(100);
        assert_eq!(due.len(), 1);

        // After marking fired, should not be due again
        let id = due[0].id.clone();
        store.mark_fired(&id, 100);
        let due = store.due_jobs(101);
        assert_eq!(due.len(), 0);

        let _ = std::fs::remove_file(&store.path);
    }
}
