use chrono::Utc;
use indexmap::IndexMap;

use prtop::diff::diff_pr_sets;
use prtop::types::{PrId, PrRole, PrState, PullRequest};

fn make_pr(owner: &str, repo: &str, number: u64, updated_secs: i64) -> (PrId, PullRequest) {
    let id = PrId {
        owner: owner.to_string(),
        repo: repo.to_string(),
        number,
    };
    let base = Utc::now();
    let pr = PullRequest {
        id: id.clone(),
        title: format!("PR #{number}"),
        url: format!("https://github.com/{owner}/{repo}/pull/{number}"),
        author_login: "user".to_string(),
        role: PrRole::Author,
        state: PrState::Open,
        created_at: base,
        updated_at: base + chrono::Duration::seconds(updated_secs),
        is_draft: false,
        review_decision: None,
        total_comments: 0,
        last_commenter: None,
        ci_status: None,
    };
    (id, pr)
}

#[test]
fn mixed_add_remove_update() {
    let mut prev = IndexMap::new();
    let (id1, pr1) = make_pr("org", "repo", 1, 0);
    let (id2, pr2) = make_pr("org", "repo", 2, 0);
    prev.insert(id1.clone(), pr1);
    prev.insert(id2.clone(), pr2);

    let mut curr = IndexMap::new();
    // PR #1 updated
    let (id1b, pr1b) = make_pr("org", "repo", 1, 100);
    // PR #2 removed (not in curr)
    // PR #3 added
    let (id3, pr3) = make_pr("org", "repo", 3, 0);
    curr.insert(id1b, pr1b);
    curr.insert(id3.clone(), pr3);

    let diff = diff_pr_sets(&prev, &curr);
    assert_eq!(diff.added, vec![id3]);
    assert_eq!(diff.removed, vec![id2]);
    assert_eq!(diff.updated, vec![id1]);
}
