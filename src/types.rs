use chrono::{DateTime, Utc};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PrId {
    pub owner: String,
    pub repo: String,
    pub number: u64,
}

impl std::fmt::Display for PrId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}#{}", self.owner, self.repo, self.number)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrRole {
    Author,
    ReviewRequested,
    Both,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrState {
    Open,
    Closed,
    Merged,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewDecision {
    Approved,
    ChangesRequested,
    ReviewRequired,
    Unknown(String),
}

impl ReviewDecision {
    pub fn from_str_opt(s: Option<&str>) -> Option<Self> {
        s.map(|s| match s {
            "APPROVED" => ReviewDecision::Approved,
            "CHANGES_REQUESTED" => ReviewDecision::ChangesRequested,
            "REVIEW_REQUIRED" => ReviewDecision::ReviewRequired,
            other => ReviewDecision::Unknown(other.to_string()),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CiStatus {
    Pending,
    Success,
    Failure,
    Error,
    Expected,
}

impl CiStatus {
    pub fn from_str_opt(s: Option<&str>) -> Option<Self> {
        s.map(|s| match s {
            "PENDING" => CiStatus::Pending,
            "SUCCESS" => CiStatus::Success,
            "FAILURE" => CiStatus::Failure,
            "ERROR" => CiStatus::Error,
            "EXPECTED" => CiStatus::Expected,
            _ => CiStatus::Pending,
        })
    }

    pub fn is_in_progress(&self) -> bool {
        matches!(self, CiStatus::Pending | CiStatus::Expected)
    }

    pub fn is_finished(&self) -> bool {
        matches!(
            self,
            CiStatus::Success | CiStatus::Failure | CiStatus::Error
        )
    }
}

#[derive(Debug, Clone)]
pub struct PullRequest {
    pub id: PrId,
    pub title: String,
    pub url: String,
    #[allow(dead_code)]
    pub author_login: String,
    pub role: PrRole,
    pub state: PrState,
    #[allow(dead_code)]
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub is_draft: bool,
    pub review_decision: Option<ReviewDecision>,
    pub total_comments: u64,
    pub last_commenter: Option<String>,
    pub ci_status: Option<CiStatus>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn review_decision_none_input() {
        assert_eq!(ReviewDecision::from_str_opt(None), None);
    }

    #[test]
    fn review_decision_known_values() {
        assert_eq!(
            ReviewDecision::from_str_opt(Some("APPROVED")),
            Some(ReviewDecision::Approved)
        );
        assert_eq!(
            ReviewDecision::from_str_opt(Some("CHANGES_REQUESTED")),
            Some(ReviewDecision::ChangesRequested)
        );
        assert_eq!(
            ReviewDecision::from_str_opt(Some("REVIEW_REQUIRED")),
            Some(ReviewDecision::ReviewRequired)
        );
    }

    #[test]
    fn review_decision_unknown_value() {
        assert_eq!(
            ReviewDecision::from_str_opt(Some("FUTURE_DECISION")),
            Some(ReviewDecision::Unknown("FUTURE_DECISION".to_string()))
        );
        assert_eq!(
            ReviewDecision::from_str_opt(Some("")),
            Some(ReviewDecision::Unknown("".to_string()))
        );
    }

    #[test]
    fn pr_id_display() {
        let id = PrId {
            owner: "org".to_string(),
            repo: "repo".to_string(),
            number: 42,
        };
        assert_eq!(id.to_string(), "org/repo#42");
    }

    // --- CiStatus ---

    #[test]
    fn ci_status_none_input() {
        assert_eq!(CiStatus::from_str_opt(None), None);
    }

    #[test]
    fn ci_status_known_values() {
        assert_eq!(
            CiStatus::from_str_opt(Some("PENDING")),
            Some(CiStatus::Pending)
        );
        assert_eq!(
            CiStatus::from_str_opt(Some("SUCCESS")),
            Some(CiStatus::Success)
        );
        assert_eq!(
            CiStatus::from_str_opt(Some("FAILURE")),
            Some(CiStatus::Failure)
        );
        assert_eq!(CiStatus::from_str_opt(Some("ERROR")), Some(CiStatus::Error));
        assert_eq!(
            CiStatus::from_str_opt(Some("EXPECTED")),
            Some(CiStatus::Expected)
        );
    }

    #[test]
    fn ci_status_unknown_defaults_to_pending() {
        assert_eq!(
            CiStatus::from_str_opt(Some("WHATEVER")),
            Some(CiStatus::Pending)
        );
    }

    #[test]
    fn ci_status_in_progress() {
        assert!(CiStatus::Pending.is_in_progress());
        assert!(CiStatus::Expected.is_in_progress());
        assert!(!CiStatus::Success.is_in_progress());
        assert!(!CiStatus::Failure.is_in_progress());
        assert!(!CiStatus::Error.is_in_progress());
    }

    #[test]
    fn ci_status_finished() {
        assert!(CiStatus::Success.is_finished());
        assert!(CiStatus::Failure.is_finished());
        assert!(CiStatus::Error.is_finished());
        assert!(!CiStatus::Pending.is_finished());
        assert!(!CiStatus::Expected.is_finished());
    }
}
