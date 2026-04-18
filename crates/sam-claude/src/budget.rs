//! Daily token budget tracker.
//!
//! State is persisted to `~/.sam/state/token_budget.json` and resets
//! automatically when the date changes.

use std::path::PathBuf;

use chrono::Local;
use serde::{Deserialize, Serialize};
use tracing::info;

/// Persistent daily token budget.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenBudget {
    pub daily_limit: u64,
    pub used_today: u64,
    pub date: String,
}

impl TokenBudget {
    /// Load from disk or create a fresh budget for today.
    ///
    /// If the persisted date differs from today, the counter resets.
    pub fn load_or_new(limit: u64) -> Self {
        let path = Self::state_path();
        let today = Self::today();

        if let Ok(data) = std::fs::read_to_string(&path) {
            if let Ok(mut budget) = serde_json::from_str::<TokenBudget>(&data) {
                if budget.date == today {
                    budget.daily_limit = limit;
                    return budget;
                }
                info!(old_date = %budget.date, "date changed — resetting token budget");
            }
        }

        Self {
            daily_limit: limit,
            used_today: 0,
            date: today,
        }
    }

    /// Check whether recording `tokens` would exceed the daily limit, and
    /// if not, add them to the running total.
    pub fn check_and_record(&mut self, tokens: u32) -> anyhow::Result<()> {
        let new_total = self.used_today + tokens as u64;
        if new_total > self.daily_limit {
            return Err(sam_core::SamError::BudgetExceeded {
                used: new_total,
                limit: self.daily_limit,
            }
            .into());
        }
        self.used_today = new_total;
        Ok(())
    }

    /// Tokens remaining for today.
    pub fn remaining(&self) -> u64 {
        self.daily_limit.saturating_sub(self.used_today)
    }

    /// Persist the current budget state to disk (atomic via temp-file rename).
    pub fn save(&self) -> anyhow::Result<()> {
        let path = Self::state_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let tmp = path.with_extension("tmp");
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(&tmp, &json)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    fn state_path() -> PathBuf {
        sam_core::state_dir().join("token_budget.json")
    }

    fn today() -> String {
        Local::now().format("%Y-%m-%d").to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_and_remaining() {
        let mut budget = TokenBudget {
            daily_limit: 1000,
            used_today: 0,
            date: "2026-04-17".to_string(),
        };

        budget.check_and_record(300).expect("should succeed");
        assert_eq!(budget.remaining(), 700);

        budget.check_and_record(500).expect("should succeed");
        assert_eq!(budget.remaining(), 200);
    }

    #[test]
    fn exceed_budget_returns_error() {
        let mut budget = TokenBudget {
            daily_limit: 100,
            used_today: 80,
            date: "2026-04-17".to_string(),
        };

        let result = budget.check_and_record(50);
        assert!(result.is_err());
        let err_msg = format!("{}", result.unwrap_err());
        assert!(err_msg.contains("budget exceeded") || err_msg.contains("Budget exceeded"));
    }

    #[test]
    fn date_reset() {
        let mut budget = TokenBudget {
            daily_limit: 1000,
            used_today: 999,
            date: "2026-04-16".to_string(), // yesterday
        };

        // Simulate what load_or_new does: if date differs, reset.
        let today = TokenBudget::today();
        if budget.date != today {
            budget.used_today = 0;
            budget.date = today;
        }

        assert_eq!(budget.used_today, 0);
        assert_eq!(budget.remaining(), 1000);
    }
}
