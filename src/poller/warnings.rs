use std::collections::BTreeMap;

use tokio::sync::mpsc;

use crate::error::AppError;

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum WarningScope {
    Search(usize),
    Review(usize, String),
    Ci(usize),
    Mentions(usize),
    Dismissals,
}

#[derive(Default)]
struct Failure {
    consecutive: u32,
    visible: Option<String>,
}

/// Only unresolved failures are retained. Success in one operation cannot clear
/// another token's warning, or a review failure for another PR.
#[derive(Default)]
pub(super) struct Warnings {
    failures: BTreeMap<WarningScope, Failure>,
    published: Option<String>,
}

impl Warnings {
    pub fn fail(&mut self, scope: WarningScope, error: &AppError, message: String) {
        let failure = self.failures.entry(scope).or_default();
        failure.consecutive = failure.consecutive.saturating_add(1);
        let temporary = matches!(
            error,
            AppError::Transient(_) | AppError::ServiceUnavailable { .. }
        );
        // Auth, permissions and quota exhaustion need attention immediately.
        if !temporary || failure.consecutive >= 2 || failure.visible.is_some() {
            failure.visible = Some(message);
        }
    }

    pub fn set(&mut self, scope: WarningScope, message: String) {
        self.failures.entry(scope).or_default().visible = Some(message);
    }

    pub fn recover(&mut self, scope: &WarningScope) {
        self.failures.remove(scope);
    }

    pub fn retain_reviews(&mut self, keep: impl Fn(usize, &str) -> bool) {
        self.failures.retain(|scope, _| match scope {
            WarningScope::Review(index, id) => keep(*index, id),
            _ => true,
        });
    }

    pub fn retain_mentions(&mut self, keep: impl Fn(usize) -> bool) {
        self.failures.retain(|scope, _| match scope {
            WarningScope::Mentions(index) => keep(*index),
            _ => true,
        });
    }

    pub async fn publish(&mut self, tx: &mpsc::Sender<Option<String>>) {
        let messages: Vec<_> = self
            .failures
            .values()
            .filter_map(|f| f.visible.as_deref())
            .collect();
        let current = (!messages.is_empty()).then(|| messages.join("\n"));
        if current != self.published {
            let _ = tx.send(current.clone()).await;
            self.published = current;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unavailable() -> AppError {
        AppError::ServiceUnavailable {
            retry_after_secs: 60,
            details: "HTTP 504".into(),
        }
    }

    #[tokio::test]
    async fn transient_failure_requires_consecutive_attempts_and_recovery_forgets_it() {
        let (tx, mut rx) = mpsc::channel(10);
        let mut warnings = Warnings::default();
        let scope = WarningScope::Search(0);
        warnings.fail(scope.clone(), &unavailable(), "504".into());
        warnings.publish(&tx).await;
        assert!(rx.try_recv().is_err());
        warnings.recover(&scope);
        warnings.fail(scope.clone(), &unavailable(), "504".into());
        warnings.publish(&tx).await;
        assert!(rx.try_recv().is_err());
        warnings.fail(scope.clone(), &unavailable(), "504".into());
        warnings.publish(&tx).await;
        assert_eq!(rx.recv().await, Some(Some("504".into())));
        warnings.publish(&tx).await;
        assert!(rx.try_recv().is_err());
        warnings.recover(&scope);
        warnings.publish(&tx).await;
        assert_eq!(rx.recv().await, Some(None));
        assert!(warnings.failures.is_empty());
    }

    #[tokio::test]
    async fn recovery_is_scoped_to_token_operation_and_pr() {
        let (tx, mut rx) = mpsc::channel(10);
        let mut warnings = Warnings::default();
        let search = WarningScope::Search(0);
        let review = WarningScope::Review(0, "o/r#1".into());
        let other = WarningScope::Search(1);
        for (scope, message) in [
            (search.clone(), "search"),
            (review.clone(), "review"),
            (other.clone(), "other"),
        ] {
            warnings.fail(scope, &AppError::Auth("denied".into()), message.into());
        }
        warnings.recover(&search);
        warnings.recover(&WarningScope::Review(0, "o/r#2".into()));
        warnings.publish(&tx).await;
        let current = rx.recv().await.unwrap().unwrap();
        assert!(!current.contains("search"));
        assert!(current.contains("review") && current.contains("other"));
        warnings.recover(&review);
        warnings.publish(&tx).await;
        assert_eq!(rx.recv().await, Some(Some("other".into())));
        warnings.recover(&other);
        warnings.publish(&tx).await;
        assert_eq!(rx.recv().await, Some(None));
    }
}
