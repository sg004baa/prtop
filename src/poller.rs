use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use indexmap::IndexMap;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::dismiss::{DismissStore, contains_mention};
use crate::error::AppError;
use crate::github::client::{GitHubClient, MAX_PAGES};
use crate::github::query;
use crate::github::types::PrNode;
use crate::types::{PrId, PrRole, PrState, PullRequest, ReviewDecision};

pub struct PollPayload {
    pub prs: IndexMap<PrId, PullRequest>,
    pub polled_at: DateTime<Utc>,
}

fn parse_state(s: &str) -> PrState {
    match s {
        "OPEN" => PrState::Open,
        "CLOSED" => PrState::Closed,
        "MERGED" => PrState::Merged,
        _ => PrState::Open,
    }
}

/// Convert a GraphQL PR node to a domain PullRequest. `ci_status` is left `None`;
/// it is populated by [`enrich_with_ci_status`] in a separate REST call.
fn node_to_pr(node: PrNode, role: PrRole) -> (PullRequest, String) {
    let id = PrId {
        owner: node.repository.owner.login,
        repo: node.repository.name,
        number: node.number,
    };
    let head_sha = node.head_ref_oid;
    let pr = PullRequest {
        id,
        title: node.title,
        url: node.url,
        author_login: node.author.map(|a| a.login).unwrap_or_default(),
        role,
        state: parse_state(&node.state),
        created_at: node.created_at.parse::<DateTime<Utc>>().unwrap_or_default(),
        updated_at: node.updated_at.parse::<DateTime<Utc>>().unwrap_or_default(),
        is_draft: node.is_draft,
        review_decision: ReviewDecision::from_str_opt(node.review_decision.as_deref()),
        total_comments: node.comments.total_count + node.review_threads.total_count,
        last_commenter: node
            .comments
            .nodes
            .first()
            .and_then(|c| c.author.as_ref())
            .map(|a| a.login.clone()),
        ci_status: None,
    };
    (pr, head_sha)
}

fn node_id(node: &PrNode) -> PrId {
    PrId {
        owner: node.repository.owner.login.clone(),
        repo: node.repository.name.clone(),
        number: node.number,
    }
}

pub fn merge_and_convert(
    author_nodes: Vec<PrNode>,
    review_nodes: Vec<PrNode>,
    mentioned_nodes: Vec<PrNode>,
) -> (IndexMap<PrId, PullRequest>, HashMap<PrId, String>) {
    let mut result: IndexMap<PrId, PullRequest> = IndexMap::new();
    let mut shas: HashMap<PrId, String> = HashMap::new();

    // 同一 PR が複数クエリに現れた場合は Author > ReviewRequested > Mentioned の
    // 優先度で解決する(先に insert された方が勝つ)。
    for node in author_nodes {
        let (pr, sha) = node_to_pr(node, PrRole::Author);
        shas.insert(pr.id.clone(), sha);
        result.insert(pr.id.clone(), pr);
    }

    for node in review_nodes {
        let (pr, sha) = node_to_pr(node, PrRole::ReviewRequested);
        if !result.contains_key(&pr.id) {
            shas.insert(pr.id.clone(), sha);
            result.insert(pr.id.clone(), pr);
        }
    }

    for node in mentioned_nodes {
        let (pr, sha) = node_to_pr(node, PrRole::Mentioned);
        if !result.contains_key(&pr.id) {
            shas.insert(pr.id.clone(), sha);
            result.insert(pr.id.clone(), pr);
        }
    }

    (result, shas)
}

/// Fetch CI status for every PR in parallel and assign `ci_status`.
/// Errors for individual PRs are swallowed (best-effort enrichment).
async fn enrich_with_ci_status(
    client: &GitHubClient,
    prs: &mut IndexMap<PrId, PullRequest>,
    shas: HashMap<PrId, String>,
) {
    let mut joinset: JoinSet<(PrId, Option<crate::types::CiStatus>)> = JoinSet::new();
    for (id, sha) in shas {
        if sha.is_empty() {
            continue;
        }
        let client = client.clone();
        joinset.spawn(async move {
            let ci = client
                .fetch_ci_status(&id.owner, &id.repo, &sha)
                .await
                .unwrap_or(None);
            (id, ci)
        });
    }
    while let Some(res) = joinset.join_next().await {
        let Ok((id, ci)) = res else { continue };
        if let Some(pr) = prs.get_mut(&id) {
            pr.ci_status = ci;
        }
    }
}

/// polling_loop の実行時依存(クライアント・対象ユーザー・ポーリング間隔・dismiss 状態)。
pub struct PollerContext {
    pub client: GitHubClient,
    pub username: String,
    pub interval: Duration,
    pub dismiss_store: Arc<Mutex<DismissStore>>,
}

pub async fn polling_loop(
    ctx: PollerContext,
    tx: mpsc::Sender<PollPayload>,
    error_tx: mpsc::Sender<String>,
    cancel: CancellationToken,
    mut refresh_rx: mpsc::Receiver<()>,
) {
    let mut backoff_secs = 0u64;

    loop {
        let result = poll_once(&ctx.client, &ctx.username, &ctx.dismiss_store, &error_tx).await;

        match result {
            Ok(payload) => {
                backoff_secs = 0;
                let _ = tx.send(payload).await;
            }
            Err(AppError::RateLimited { retry_after_secs }) => {
                let _ = error_tx
                    .send(format!("Rate limited. Retry after {retry_after_secs}s"))
                    .await;
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(retry_after_secs)) => {}
                    _ = cancel.cancelled() => return,
                }
                continue;
            }
            Err(AppError::Auth(msg)) => {
                let _ = error_tx.send(format!("Auth error: {msg}")).await;
                // Stop polling on auth errors
                cancel.cancelled().await;
                return;
            }
            Err(e) => {
                backoff_secs = (backoff_secs * 2).clamp(2, 60);
                let _ = error_tx
                    .send(format!("{e} (retry in {backoff_secs}s)"))
                    .await;
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(backoff_secs)) => {}
                    _ = cancel.cancelled() => return,
                }
                continue;
            }
        }

        // Wait for next interval or manual refresh
        tokio::select! {
            _ = tokio::time::sleep(ctx.interval) => {}
            _ = refresh_rx.recv() => {}
            _ = cancel.cancelled() => return,
        }
    }
}

async fn poll_once(
    client: &GitHubClient,
    username: &str,
    dismiss_store: &Mutex<DismissStore>,
    error_tx: &mpsc::Sender<String>,
) -> Result<PollPayload, AppError> {
    let author_open_query = query::author_search_query(username);
    let author_closed_query = query::author_closed_search_query(username);
    let review_open_query = query::review_requested_search_query(username);
    let review_closed_query = query::review_requested_closed_search_query(username);
    let mentions_open_query = query::mentions_search_query(username);
    let mentions_closed_query = query::mentions_closed_search_query(username);

    // Run all six queries in parallel.
    // Open queries are fully paginated to ensure no active PRs are missed.
    // Closed/merged queries fetch only one page (the most recently updated ones).
    let (author_open, author_closed, review_open, review_closed, mentions_open, mentions_closed) = tokio::join!(
        client.search_prs(&author_open_query, MAX_PAGES),
        client.search_prs(&author_closed_query, 1),
        client.search_prs(&review_open_query, MAX_PAGES),
        client.search_prs(&review_closed_query, 1),
        client.search_prs(&mentions_open_query, MAX_PAGES),
        client.search_prs(&mentions_closed_query, 1),
    );

    let mut author_nodes = author_open?;
    author_nodes.extend(author_closed?);

    let mut review_nodes = review_open?;
    review_nodes.extend(review_closed?);

    let mut mentioned_nodes = mentions_open?;
    mentioned_nodes.extend(mentions_closed?);

    let mention_ids: HashSet<PrId> = mentioned_nodes.iter().map(node_id).collect();

    let (mut prs, shas) = merge_and_convert(author_nodes, review_nodes, mentioned_nodes);

    // mentions クエリに出なくなった PR の dismiss エントリは掃除する。
    // 全クエリ成功後(`?` の後)なので、エラー時に誤って掃除されることはない。
    // ロックは std::sync::Mutex のため await を跨がないよう、スナップショットを取ってすぐ手放す。
    let dismissed = {
        let mut store = dismiss_store.lock().expect("dismiss store lock poisoned");
        store.retain_ids(&mention_ids);
        store.snapshot()
    };

    // dismiss 後に更新があった mentioned PR だけ、直近 issue コメントを見て
    // 再メンションを検証する(レビューコメント内の再メンション検出は v1 スコープ外)。
    let candidates: Vec<(PrId, DateTime<Utc>)> = prs
        .iter()
        .filter(|(_, pr)| pr.role == PrRole::Mentioned)
        .filter_map(|(id, pr)| {
            let dismissed_at = *dismissed.get(id)?;
            (pr.updated_at > dismissed_at).then(|| (id.clone(), dismissed_at))
        })
        .collect();

    let mut undismissed: HashSet<PrId> = HashSet::new();
    if !candidates.is_empty() {
        let ids: Vec<PrId> = candidates.iter().map(|(id, _)| id.clone()).collect();
        // 再メンション検証は best-effort: 失敗しても poll 全体は落とさず、
        // 該当 PR はこのポーリングでは dismissed のまま維持する。
        match client.fetch_recent_comments(&ids).await {
            Ok(comments) => {
                for (id, dismissed_at) in &candidates {
                    let re_mentioned = comments.get(id).is_some_and(|list| {
                        list.iter().any(|c| {
                            c.created_at > *dismissed_at
                                && !c
                                    .author_login
                                    .as_deref()
                                    .is_some_and(|a| a.eq_ignore_ascii_case(username))
                                && contains_mention(&c.body_text, username)
                        })
                    });
                    if re_mentioned {
                        undismissed.insert(id.clone());
                    }
                }
            }
            Err(e) => {
                let _ = error_tx.send(format!("Mention re-check failed: {e}")).await;
            }
        }
    }

    let still_dismissed = {
        let mut store = dismiss_store.lock().expect("dismiss store lock poisoned");
        for id in &undismissed {
            store.undismiss(id);
        }
        if store.is_dirty() {
            store.save()?;
        }
        store.dismissed_ids()
    };

    // dismiss されたままの Mentioned ロール PR はリストに出さない。
    prs.retain(|id, pr| pr.role != PrRole::Mentioned || !still_dismissed.contains(id));

    enrich_with_ci_status(client, &mut prs, shas).await;

    Ok(PollPayload {
        prs,
        polled_at: Utc::now(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::client::GitHubClient;
    use crate::github::types::{
        ActorNode, CommentsConnection, PrNode, RepoNode, RepoOwnerNode, TotalCount,
    };
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn make_node(owner: &str, repo: &str, number: u64) -> PrNode {
        PrNode {
            number,
            title: format!("PR #{number}"),
            url: format!("https://github.com/{owner}/{repo}/pull/{number}"),
            state: "OPEN".to_string(),
            is_draft: false,
            created_at: "2024-01-01T00:00:00Z".to_string(),
            updated_at: "2024-01-01T00:00:00Z".to_string(),
            review_decision: None,
            head_ref_oid: String::new(),
            author: Some(ActorNode {
                login: "user".to_string(),
            }),
            repository: RepoNode {
                name: repo.to_string(),
                owner: RepoOwnerNode {
                    login: owner.to_string(),
                },
            },
            comments: CommentsConnection {
                total_count: 0,
                nodes: vec![],
            },
            review_threads: TotalCount { total_count: 0 },
        }
    }

    #[test]
    fn merge_author_only() {
        let author = vec![make_node("org", "repo", 1)];
        let (result, _) = merge_and_convert(author, vec![], vec![]);
        assert_eq!(result.len(), 1);
        let pr = result.values().next().unwrap();
        assert_eq!(pr.role, PrRole::Author);
    }

    #[test]
    fn merge_review_only() {
        let review = vec![make_node("org", "repo", 1)];
        let (result, _) = merge_and_convert(vec![], review, vec![]);
        assert_eq!(result.len(), 1);
        let pr = result.values().next().unwrap();
        assert_eq!(pr.role, PrRole::ReviewRequested);
    }

    #[test]
    fn merge_mentioned_only() {
        let mentioned = vec![make_node("org", "repo", 1)];
        let (result, _) = merge_and_convert(vec![], vec![], mentioned);
        assert_eq!(result.len(), 1);
        let pr = result.values().next().unwrap();
        assert_eq!(pr.role, PrRole::Mentioned);
    }

    #[test]
    fn merge_author_wins_over_review_and_mentioned() {
        let author = vec![make_node("org", "repo", 1)];
        let review = vec![make_node("org", "repo", 1)];
        let mentioned = vec![make_node("org", "repo", 1)];
        let (result, _) = merge_and_convert(author, review, mentioned);
        assert_eq!(result.len(), 1);
        let pr = result.values().next().unwrap();
        assert_eq!(pr.role, PrRole::Author);
    }

    #[test]
    fn merge_review_wins_over_mentioned() {
        let review = vec![make_node("org", "repo", 1)];
        let mentioned = vec![make_node("org", "repo", 1)];
        let (result, _) = merge_and_convert(vec![], review, mentioned);
        assert_eq!(result.len(), 1);
        let pr = result.values().next().unwrap();
        assert_eq!(pr.role, PrRole::ReviewRequested);
    }

    #[test]
    fn merge_distinct_prs() {
        let author = vec![make_node("org", "repo", 1)];
        let review = vec![make_node("org", "repo", 2)];
        let mentioned = vec![make_node("org", "repo", 3)];
        let (result, _) = merge_and_convert(author, review, mentioned);
        assert_eq!(result.len(), 3);
    }

    // --- parse_state ---

    #[test]
    fn parse_state_known_values() {
        assert_eq!(parse_state("OPEN"), PrState::Open);
        assert_eq!(parse_state("CLOSED"), PrState::Closed);
        assert_eq!(parse_state("MERGED"), PrState::Merged);
    }

    #[test]
    fn parse_state_unknown_defaults_to_open() {
        assert_eq!(parse_state("WHATEVER"), PrState::Open);
        assert_eq!(parse_state(""), PrState::Open);
    }

    // --- node_to_pr boundary cases ---

    #[test]
    fn node_to_pr_author_none_gives_empty_string() {
        let mut node = make_node("org", "repo", 1);
        node.author = None;
        let (pr, _) = node_to_pr(node, PrRole::Author);
        assert_eq!(pr.author_login, "");
    }

    #[test]
    fn node_to_pr_invalid_timestamp_defaults_to_epoch() {
        let mut node = make_node("org", "repo", 1);
        node.created_at = "not-a-timestamp".to_string();
        node.updated_at = "also-invalid".to_string();
        let (pr, _) = node_to_pr(node, PrRole::Author);
        assert_eq!(pr.created_at, DateTime::<Utc>::default());
        assert_eq!(pr.updated_at, DateTime::<Utc>::default());
    }

    #[test]
    fn node_to_pr_unknown_review_decision() {
        let mut node = make_node("org", "repo", 1);
        node.review_decision = Some("FUTURE_DECISION".to_string());
        let (pr, _) = node_to_pr(node, PrRole::Author);
        assert_eq!(
            pr.review_decision,
            Some(ReviewDecision::Unknown("FUTURE_DECISION".to_string()))
        );
    }

    // --- node_to_pr last_commenter extraction ---

    #[test]
    fn node_to_pr_extracts_last_commenter() {
        use crate::github::types::CommentNode;
        let mut node = make_node("org", "repo", 1);
        node.comments = CommentsConnection {
            total_count: 5,
            nodes: vec![CommentNode {
                author: Some(ActorNode {
                    login: "reviewer1".to_string(),
                }),
            }],
        };
        let (pr, _) = node_to_pr(node, PrRole::Author);
        assert_eq!(pr.last_commenter.as_deref(), Some("reviewer1"));
        assert_eq!(pr.total_comments, 5); // review_threads(0) + comments(5)
    }

    #[test]
    fn node_to_pr_empty_comments_gives_none() {
        let node = make_node("org", "repo", 1);
        let (pr, _) = node_to_pr(node, PrRole::Author);
        assert_eq!(pr.last_commenter, None);
    }

    #[test]
    fn node_to_pr_comment_author_none_gives_none() {
        use crate::github::types::CommentNode;
        let mut node = make_node("org", "repo", 1);
        node.comments = CommentsConnection {
            total_count: 2,
            nodes: vec![CommentNode { author: None }],
        };
        let (pr, _) = node_to_pr(node, PrRole::Author);
        assert_eq!(pr.last_commenter, None);
    }

    #[test]
    fn node_to_pr_propagates_head_sha() {
        let mut node = make_node("org", "repo", 1);
        node.head_ref_oid = "deadbeef".to_string();
        let (_, sha) = node_to_pr(node, PrRole::Author);
        assert_eq!(sha, "deadbeef");
    }

    // --- polling_loop async control ---

    fn empty_graphql_response() -> serde_json::Value {
        serde_json::json!({
            "data": {
                "search": {
                    "issueCount": 0,
                    "pageInfo": {"hasNextPage": false, "endCursor": null},
                    "nodes": []
                }
            }
        })
    }

    fn test_dismiss_store(name: &str) -> Arc<Mutex<DismissStore>> {
        let path = std::env::temp_dir().join(format!(
            "prtop-poller-test-{name}-{}-{}.json",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        Arc::new(Mutex::new(DismissStore::load_from(path).unwrap()))
    }

    #[tokio::test]
    async fn polling_loop_stops_on_cancel() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(empty_graphql_response()))
            .mount(&server)
            .await;

        let client = GitHubClient::new_with_base_url("token".to_string(), server.uri());
        let (tx, mut rx) = mpsc::channel(16);
        let (err_tx, _err_rx) = mpsc::channel(16);
        let (_refresh_tx, refresh_rx) = mpsc::channel(1);
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();

        let handle = tokio::spawn(async move {
            polling_loop(
                PollerContext {
                    client,
                    username: "user".to_string(),
                    interval: Duration::from_secs(3600),
                    dismiss_store: test_dismiss_store("cancel"),
                },
                tx,
                err_tx,
                cancel_clone,
                refresh_rx,
            )
            .await;
        });

        // Wait for first poll to complete, then cancel while sleeping for next interval
        tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("timeout waiting for first poll result")
            .expect("channel closed unexpectedly");

        cancel.cancel();

        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("timeout: polling_loop did not stop after cancel")
            .expect("task panicked");
    }

    #[tokio::test]
    async fn polling_loop_stops_on_auth_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;

        let client = GitHubClient::new_with_base_url("bad_token".to_string(), server.uri());
        let (tx, _rx) = mpsc::channel(16);
        let (err_tx, mut err_rx) = mpsc::channel(16);
        let (_refresh_tx, refresh_rx) = mpsc::channel(1);
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();

        let handle = tokio::spawn(async move {
            polling_loop(
                PollerContext {
                    client,
                    username: "user".to_string(),
                    interval: Duration::from_secs(60),
                    dismiss_store: test_dismiss_store("auth"),
                },
                tx,
                err_tx,
                cancel_clone,
                refresh_rx,
            )
            .await;
        });

        let error_msg = tokio::time::timeout(Duration::from_secs(5), err_rx.recv())
            .await
            .expect("timeout waiting for auth error")
            .expect("channel closed");
        assert!(
            error_msg.contains("Auth error"),
            "unexpected message: {error_msg}"
        );

        // Unblock the loop which is waiting on cancel.cancelled()
        cancel.cancel();

        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("timeout: polling_loop did not stop after auth error")
            .expect("task panicked");
    }

    #[tokio::test]
    async fn polling_loop_refresh_triggers_early_poll() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(empty_graphql_response()))
            .mount(&server)
            .await;

        let client = GitHubClient::new_with_base_url("token".to_string(), server.uri());
        let (tx, mut rx) = mpsc::channel(16);
        let (err_tx, _err_rx) = mpsc::channel(16);
        let (refresh_tx, refresh_rx) = mpsc::channel(1);
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();

        let handle = tokio::spawn(async move {
            polling_loop(
                PollerContext {
                    client,
                    username: "user".to_string(),
                    interval: Duration::from_secs(3600),
                    dismiss_store: test_dismiss_store("refresh"),
                },
                tx,
                err_tx,
                cancel_clone,
                refresh_rx,
            )
            .await;
        });

        // Wait for first poll
        tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("timeout waiting for first poll")
            .expect("channel closed");

        // Trigger refresh to force early second poll
        refresh_tx.send(()).await.unwrap();

        // Wait for second poll triggered by refresh
        tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("timeout waiting for refresh-triggered poll")
            .expect("channel closed");

        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .unwrap()
            .unwrap();
    }
}
