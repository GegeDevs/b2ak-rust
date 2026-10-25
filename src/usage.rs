use std::collections::HashMap;
use std::path::PathBuf;

use chrono::Utc;
use serde_json::Value;
use tokio::sync::Mutex;

/// Maximum number of compiles a non-owner user may run per UTC day.
pub const DAILY_LIMIT: u32 = 3;

/// Persistent daily usage counters, stored as
/// `{ "<user_id>": { "YYYY-MM-DD": <count> } }`.
///
/// The on-disk shape is compatible with the original Python bot.
pub struct UsageStore {
    path: PathBuf,
    lock: Mutex<()>,
}

impl UsageStore {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            lock: Mutex::new(()),
        }
    }

    /// Returns `(allowed, runs_taken_today)`. Owner runs are never counted.
    pub async fn check_and_increment(&self, user_id: i64, owner_id: i64) -> (bool, u32) {
        if user_id == owner_id {
            return (true, 0);
        }

        let _guard = self.lock.lock().await;
        let today = Utc::now().format("%Y-%m-%d").to_string();

        let mut data = self.load();
        let user = data.entry(user_id.to_string()).or_default();
        let count = user.get(&today).copied().unwrap_or(0);

        if count >= DAILY_LIMIT {
            return (false, count);
        }

        let next = count + 1;
        // Mirror the Python bot: only keep the current day's counter.
        user.clear();
        user.insert(today, next);
        self.save(&data);
        (true, next)
    }

    fn load(&self) -> HashMap<String, HashMap<String, u32>> {
        let Ok(raw) = std::fs::read_to_string(&self.path) else {
            return HashMap::new();
        };
        let Ok(value) = serde_json::from_str::<Value>(&raw) else {
            return HashMap::new();
        };
        let Some(users) = value.as_object() else {
            return HashMap::new();
        };

        let mut data = HashMap::new();
        for (user_id, history) in users {
            let Some(days) = history.as_object() else {
                continue;
            };
            let mut counts = HashMap::new();
            for (day, count) in days {
                if let Some(count) = count.as_u64() {
                    counts.insert(day.clone(), count as u32);
                }
            }
            data.insert(user_id.clone(), counts);
        }
        data
    }

    fn save(&self, data: &HashMap<String, HashMap<String, u32>>) {
        let Ok(json) = serde_json::to_vec(data) else {
            return;
        };

        // Write through a temporary file so a crash cannot truncate the counters.
        let tmp = self.path.with_extension("json.tmp");
        if let Err(err) = std::fs::write(&tmp, json) {
            tracing::error!("Failed to write usage file {}: {err}", tmp.display());
            return;
        }
        if let Err(err) = std::fs::rename(&tmp, &self.path) {
            tracing::error!(
                "Failed to move usage file {} into place: {err}",
                tmp.display()
            );
            let _ = std::fs::remove_file(tmp);
        }
    }
}
