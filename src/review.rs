use std::collections::{HashMap, HashSet};

/// A complete review snapshot. Unknown/failed fetches are represented by None.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReviewSummary {
    pub approved: usize,
    pub total: usize,
    pub changes_requested: bool,
    pub pending_teams: usize,
}

impl ReviewSummary {
    pub fn all_approved(&self) -> bool {
        self.total > 0
            && self.approved == self.total
            && self.pending_teams == 0
            && !self.changes_requested
    }
}

#[derive(Debug, Clone)]
pub struct SubmittedReview {
    pub login: String,
    pub state: String,
    pub submitted_at: String,
}

/// Reviews must be in submission order. Comments retain an existing decision;
/// dismissal invalidates it, and an outstanding request overrides approval.
pub fn summarize_reviews(
    author: &str,
    reviews: &[SubmittedReview],
    requested: &[String],
    teams: &[String],
) -> ReviewSummary {
    let mut decisions: HashMap<String, &str> = HashMap::new();
    for review in reviews {
        let login = review.login.to_ascii_lowercase();
        if login.eq_ignore_ascii_case(author) || review.state == "PENDING" {
            continue;
        }
        match review.state.as_str() {
            "COMMENTED" => {
                decisions.entry(login).or_insert("COMMENTED");
            }
            state => {
                decisions.insert(login, state);
            }
        }
    }
    let changes_requested = decisions
        .values()
        .any(|&state| state == "CHANGES_REQUESTED");
    for login in requested {
        if !login.eq_ignore_ascii_case(author) {
            decisions.insert(login.to_ascii_lowercase(), "REQUESTED");
        }
    }
    ReviewSummary {
        approved: decisions
            .values()
            .filter(|&&state| state == "APPROVED")
            .count(),
        total: decisions.len(),
        changes_requested,
        pending_teams: teams.iter().collect::<HashSet<_>>().len(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn review(login: &str, state: &str) -> SubmittedReview {
        SubmittedReview {
            login: login.into(),
            state: state.into(),
            submitted_at: String::new(),
        }
    }

    #[test]
    fn counts_unique_people_and_ignores_author_and_pending_reviews() {
        let reviews = vec![
            review("alice", "APPROVED"),
            review("ALICE", "COMMENTED"),
            review("author", "APPROVED"),
            review("bob", "PENDING"),
        ];
        let summary = summarize_reviews("author", &reviews, &["carol".into()], &[]);
        assert_eq!(summary.approved, 1);
        assert_eq!(summary.total, 2);
        assert!(!summary.all_approved());
    }

    #[test]
    fn latest_decision_dismissal_and_rerequest_invalidate_approval() {
        for state in ["CHANGES_REQUESTED", "DISMISSED"] {
            let reviews = vec![
                review("alice", "APPROVED"),
                review("alice", state),
                review("alice", "COMMENTED"),
            ];
            assert!(!summarize_reviews("author", &reviews, &[], &[]).all_approved());
        }
        let reviews = vec![
            review("alice", "CHANGES_REQUESTED"),
            review("alice", "APPROVED"),
        ];
        assert!(summarize_reviews("author", &reviews, &[], &[]).all_approved());
        assert!(!summarize_reviews("author", &reviews, &["Alice".into()], &[]).all_approved());
    }

    #[test]
    fn comments_only_empty_and_team_requests_are_not_approval() {
        assert!(!ReviewSummary::default().all_approved());
        assert!(
            !summarize_reviews("author", &[review("alice", "COMMENTED")], &[], &[]).all_approved()
        );
        let summary = summarize_reviews(
            "author",
            &[review("alice", "APPROVED")],
            &[],
            &["team".into()],
        );
        assert_eq!(summary.approved, 1);
        assert_eq!(summary.pending_teams, 1);
        assert!(!summary.all_approved());
    }
}
