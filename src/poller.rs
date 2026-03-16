use std::collections::HashSet;
use std::time::Duration;

use chrono::{DateTime, Utc};
use indexmap::IndexMap;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

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

fn node_to_pr(node: PrNode, role: PrRole) -> PullRequest {
    let id = PrId {
        owner: node.repository.owner.login,
        repo: node.repository.name,
        number: node.number,
    };
    PullRequest {
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
        total_comments: node.comments.total_count + node.review_comments.total_count,
    }
}

pub fn merge_and_convert(
    author_nodes: Vec<PrNode>,
    review_nodes: Vec<PrNode>,
) -> IndexMap<PrId, PullRequest> {
    let mut result = IndexMap::new();

    let mut author_ids: HashSet<PrId> = HashSet::new();
    for node in author_nodes {
        let pr = node_to_pr(node, PrRole::Author);
        author_ids.insert(pr.id.clone());
        result.insert(pr.id.clone(), pr);
    }

    for node in review_nodes {
        let pr = node_to_pr(node, PrRole::ReviewRequested);
        if author_ids.contains(&pr.id) {
            if let Some(existing) = result.get_mut(&pr.id) {
                existing.role = PrRole::Both;
            }
        } else {
            result.insert(pr.id.clone(), pr);
        }
    }

    result
}

pub async fn polling_loop(
    client: GitHubClient,
    username: String,
    interval: Duration,
    tx: mpsc::Sender<PollPayload>,
    error_tx: mpsc::Sender<String>,
    cancel: CancellationToken,
    mut refresh_rx: mpsc::Receiver<()>,
) {
    let mut backoff_secs = 0u64;

    loop {
        let result = poll_once(&client, &username).await;

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
            _ = tokio::time::sleep(interval) => {}
            _ = refresh_rx.recv() => {}
            _ = cancel.cancelled() => return,
        }
    }
}

async fn poll_once(client: &GitHubClient, username: &str) -> Result<PollPayload, AppError> {
    let author_open_query = query::author_search_query(username);
    let author_closed_query = query::author_closed_search_query(username);
    let review_open_query = query::review_requested_search_query(username);
    let review_closed_query = query::review_requested_closed_search_query(username);

    // Run all four queries in parallel.
    // Open queries are fully paginated to ensure no active PRs are missed.
    // Closed/merged queries fetch only one page (the most recently updated ones).
    let (author_open, author_closed, review_open, review_closed) = tokio::join!(
        client.search_prs(&author_open_query, MAX_PAGES),
        client.search_prs(&author_closed_query, 1),
        client.search_prs(&review_open_query, MAX_PAGES),
        client.search_prs(&review_closed_query, 1),
    );

    let mut author_nodes = author_open?;
    author_nodes.extend(author_closed?);

    let mut review_nodes = review_open?;
    review_nodes.extend(review_closed?);

    let prs = merge_and_convert(author_nodes, review_nodes);

    Ok(PollPayload {
        prs,
        polled_at: Utc::now(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::client::GitHubClient;
    use crate::github::types::{ActorNode, PrNode, RepoNode, RepoOwnerNode, TotalCount};
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
            author: Some(ActorNode {
                login: "user".to_string(),
            }),
            repository: RepoNode {
                name: repo.to_string(),
                owner: RepoOwnerNode {
                    login: owner.to_string(),
                },
            },
            comments: TotalCount { total_count: 0 },
            review_comments: TotalCount { total_count: 0 },
        }
    }

    #[test]
    fn merge_author_only() {
        let author = vec![make_node("org", "repo", 1)];
        let review = vec![];
        let result = merge_and_convert(author, review);
        assert_eq!(result.len(), 1);
        let pr = result.values().next().unwrap();
        assert_eq!(pr.role, PrRole::Author);
    }

    #[test]
    fn merge_review_only() {
        let author = vec![];
        let review = vec![make_node("org", "repo", 1)];
        let result = merge_and_convert(author, review);
        assert_eq!(result.len(), 1);
        let pr = result.values().next().unwrap();
        assert_eq!(pr.role, PrRole::ReviewRequested);
    }

    #[test]
    fn merge_both_roles() {
        let author = vec![make_node("org", "repo", 1)];
        let review = vec![make_node("org", "repo", 1)];
        let result = merge_and_convert(author, review);
        assert_eq!(result.len(), 1);
        let pr = result.values().next().unwrap();
        assert_eq!(pr.role, PrRole::Both);
    }

    #[test]
    fn merge_distinct_prs() {
        let author = vec![make_node("org", "repo", 1)];
        let review = vec![make_node("org", "repo", 2)];
        let result = merge_and_convert(author, review);
        assert_eq!(result.len(), 2);
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
        let pr = node_to_pr(node, PrRole::Author);
        assert_eq!(pr.author_login, "");
    }

    #[test]
    fn node_to_pr_invalid_timestamp_defaults_to_epoch() {
        let mut node = make_node("org", "repo", 1);
        node.created_at = "not-a-timestamp".to_string();
        node.updated_at = "also-invalid".to_string();
        let pr = node_to_pr(node, PrRole::Author);
        assert_eq!(pr.created_at, DateTime::<Utc>::default());
        assert_eq!(pr.updated_at, DateTime::<Utc>::default());
    }

    #[test]
    fn node_to_pr_unknown_review_decision() {
        let mut node = make_node("org", "repo", 1);
        node.review_decision = Some("FUTURE_DECISION".to_string());
        let pr = node_to_pr(node, PrRole::Author);
        assert_eq!(
            pr.review_decision,
            Some(ReviewDecision::Unknown("FUTURE_DECISION".to_string()))
        );
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
                client,
                "user".to_string(),
                Duration::from_secs(3600),
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
                client,
                "user".to_string(),
                Duration::from_secs(60),
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
                client,
                "user".to_string(),
                Duration::from_secs(3600),
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
