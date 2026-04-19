use indexmap::IndexMap;

use crate::types::{PrId, PullRequest};

#[derive(Debug, Default)]
pub struct DiffResult {
    pub added: Vec<PrId>,
    pub removed: Vec<PrId>,
    pub updated: Vec<PrId>,
}

pub fn diff_pr_sets(
    previous: &IndexMap<PrId, PullRequest>,
    current: &IndexMap<PrId, PullRequest>,
) -> DiffResult {
    let mut result = DiffResult::default();

    for (id, new_pr) in current {
        match previous.get(id) {
            None => result.added.push(id.clone()),
            Some(old_pr) => {
                if old_pr.updated_at != new_pr.updated_at {
                    result.updated.push(id.clone());
                }
            }
        }
    }

    for id in previous.keys() {
        if !current.contains_key(id) {
            result.removed.push(id.clone());
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{PrRole, PrState};
    use chrono::Utc;

    fn make_pr(owner: &str, repo: &str, number: u64) -> (PrId, PullRequest) {
        let id = PrId {
            owner: owner.to_string(),
            repo: repo.to_string(),
            number,
        };
        let pr = PullRequest {
            id: id.clone(),
            title: format!("PR #{number}"),
            url: format!("https://github.com/{owner}/{repo}/pull/{number}"),
            author_login: "user".to_string(),
            role: PrRole::Author,
            state: PrState::Open,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            is_draft: false,
            review_decision: None,
            total_comments: 0,
            last_commenter: None,
        };
        (id, pr)
    }

    #[test]
    fn empty_to_empty() {
        let prev = IndexMap::new();
        let curr = IndexMap::new();
        let diff = diff_pr_sets(&prev, &curr);
        assert!(diff.added.is_empty());
        assert!(diff.removed.is_empty());
        assert!(diff.updated.is_empty());
    }

    #[test]
    fn detects_added() {
        let prev = IndexMap::new();
        let mut curr = IndexMap::new();
        let (id, pr) = make_pr("org", "repo", 1);
        curr.insert(id.clone(), pr);

        let diff = diff_pr_sets(&prev, &curr);
        assert_eq!(diff.added, vec![id]);
        assert!(diff.removed.is_empty());
    }

    #[test]
    fn detects_removed() {
        let mut prev = IndexMap::new();
        let (id, pr) = make_pr("org", "repo", 1);
        prev.insert(id.clone(), pr);
        let curr = IndexMap::new();

        let diff = diff_pr_sets(&prev, &curr);
        assert!(diff.added.is_empty());
        assert_eq!(diff.removed, vec![id]);
    }

    #[test]
    fn detects_updated() {
        let mut prev = IndexMap::new();
        let (id, pr) = make_pr("org", "repo", 1);
        prev.insert(id.clone(), pr);

        let mut curr = IndexMap::new();
        let (id2, mut pr2) = make_pr("org", "repo", 1);
        pr2.updated_at = Utc::now() + chrono::Duration::seconds(10);
        curr.insert(id2, pr2);

        let diff = diff_pr_sets(&prev, &curr);
        assert!(diff.added.is_empty());
        assert!(diff.removed.is_empty());
        assert_eq!(diff.updated, vec![id]);
    }

    #[test]
    fn no_change_when_same() {
        let mut prev = IndexMap::new();
        let (id, pr) = make_pr("org", "repo", 1);
        let updated_at = pr.updated_at;
        prev.insert(id.clone(), pr);

        let mut curr = IndexMap::new();
        let (id2, mut pr2) = make_pr("org", "repo", 1);
        pr2.updated_at = updated_at;
        curr.insert(id2, pr2);

        let diff = diff_pr_sets(&prev, &curr);
        assert!(diff.added.is_empty());
        assert!(diff.removed.is_empty());
        assert!(diff.updated.is_empty());
    }
}
