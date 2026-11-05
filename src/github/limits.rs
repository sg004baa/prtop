use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use crate::error::AppError;

#[derive(Clone)]
pub(super) struct Cooldown {
    pub until: tokio::time::Instant,
    pub details: String,
    pub service: bool,
}

impl Cooldown {
    pub fn error(&self) -> Option<AppError> {
        let duration = self
            .until
            .saturating_duration_since(tokio::time::Instant::now());
        if duration.is_zero() {
            return None;
        }
        let retry_after_secs = duration
            .as_secs()
            .saturating_add(u64::from(duration.subsec_nanos() > 0));
        if self.service {
            Some(AppError::ServiceUnavailable {
                retry_after_secs,
                details: self.details.clone(),
            })
        } else {
            Some(AppError::RateLimited {
                retry_after_secs,
                details: self.details.clone(),
            })
        }
    }
}

#[derive(Default)]
pub(super) struct Limits {
    buckets: HashMap<String, Cooldown>,
}

impl Limits {
    pub fn check(&self, resource: &str) -> Option<AppError> {
        ["*", resource]
            .into_iter()
            .filter_map(|key| self.buckets.get(key))
            .filter(|c| c.until > tokio::time::Instant::now())
            .max_by_key(|c| c.until)
            .and_then(Cooldown::error)
    }

    pub fn record(
        &mut self,
        resource: &str,
        seconds: u64,
        details: String,
        service: bool,
    ) -> Cooldown {
        // Bound untrusted header values before adding to Instant.
        let until = tokio::time::Instant::now() + Duration::from_secs(seconds.clamp(1, 86400));
        let cooldown = Cooldown {
            until,
            details,
            service,
        };
        self.merge(resource.to_string(), cooldown.clone());
        cooldown
    }

    fn merge(&mut self, key: String, value: Cooldown) {
        if self
            .buckets
            .get(&key)
            .is_none_or(|old| old.until < value.until)
        {
            self.buckets.insert(key, value);
        }
    }

    #[cfg(test)]
    pub fn clear(&mut self) {
        self.buckets.clear();
    }
}

pub(super) type SharedLimits = Arc<Mutex<Limits>>;
type Registry = Mutex<HashMap<String, Weak<Mutex<Limits>>>>;
static USERS: OnceLock<Registry> = OnceLock::new();

/// Associate only a GitHub-verified viewer ID, never a configured username or token.
/// Host/base URL isolates GitHub instances (and mock servers). No secrets are stored.
pub(super) fn for_viewer(base: &str, id: &str, local: &SharedLimits) -> SharedLimits {
    let mut users = USERS
        .get_or_init(Default::default)
        .lock()
        .expect("rate limit registry");
    users.retain(|_, weak| weak.strong_count() > 0);
    let key = format!("{base}/{id}");
    if let Some(existing) = users.get(&key).and_then(Weak::upgrade) {
        if !Arc::ptr_eq(&existing, local) {
            let mut shared = existing.lock().expect("shared rate limits");
            for (key, value) in &local.lock().expect("local rate limits").buckets {
                shared.merge(key.clone(), value.clone());
            }
        }
        existing
    } else {
        users.insert(key, Arc::downgrade(local));
        local.clone()
    }
}
