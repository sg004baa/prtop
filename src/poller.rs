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

pub enum PollEvent {
    Snapshot(PollPayload),
    Ci(CiPayload),
    Reviews(ReviewPayload),
}

pub struct PollPayload {
    pub generation: u64,
    pub prs: IndexMap<PrId, PullRequest>,
    pub head_shas: HashMap<PrId, String>,
    pub polled_at: DateTime<Utc>,
}
pub struct CiPayload {
    pub generation: u64,
    pub statuses: HashMap<PrId, CiUpdate>,
}

pub struct CiUpdate {
    pub head_sha: String,
    pub status: Option<crate::types::CiStatus>,
}
pub struct ReviewPayload {
    pub generation: u64,
    pub statuses: HashMap<PrId, Option<crate::github::reviews::ReviewSnapshot>>,
}

#[derive(Clone, PartialEq, Eq)]
struct ReviewVersion {
    updated_at: DateTime<Utc>,
    head_sha: String,
    decision: Option<ReviewDecision>,
}
struct ReviewCacheEntry {
    version: ReviewVersion,
    fresh_until: tokio::time::Instant,
    retry_at: tokio::time::Instant,
}
const REVIEW_TTL: Duration = Duration::from_secs(300);
const FAILURE_RETRY: Duration = Duration::from_secs(60);

async fn fetch_reviews(
    states: &[TokenState],
    owners: &HashMap<PrId, usize>,
    prs: &IndexMap<PrId, PullRequest>,
    shas: &HashMap<PrId, String>,
    cache: &mut HashMap<PrId, ReviewCacheEntry>,
    error_tx: &mpsc::Sender<String>,
) -> HashMap<PrId, Option<crate::github::reviews::ReviewSnapshot>> {
    cache.retain(|id, _| prs.get(id).is_some_and(|pr| pr.state == PrState::Open));
    let mut statuses = HashMap::new();
    let mut reported = HashSet::new();
    for (id, pr) in prs {
        if pr.state != PrState::Open {
            continue;
        }
        let Some(&index) = owners.get(id) else {
            continue;
        };
        let Some(state) = states.get(index) else {
            continue;
        };
        let now = tokio::time::Instant::now();
        let version = ReviewVersion {
            updated_at: pr.updated_at,
            head_sha: shas.get(id).cloned().unwrap_or_default(),
            decision: pr.review_decision.clone(),
        };
        if state.disabled || state.retrying {
            if let Some(cached) = cache.get_mut(id) {
                cached.fresh_until = now;
            }
            statuses.insert(id.clone(), None);
            continue;
        }
        if cache
            .get(id)
            .is_some_and(|c| now < c.retry_at || (c.version == version && now < c.fresh_until))
        {
            continue;
        }
        let result = state.client.fetch_review_summary(id).await;
        let now = tokio::time::Instant::now();
        match result {
            Ok(snapshot) if snapshot.head_sha == version.head_sha => {
                cache.insert(
                    id.clone(),
                    ReviewCacheEntry {
                        version,
                        fresh_until: now + REVIEW_TTL,
                        retry_at: now,
                    },
                );
                statuses.insert(id.clone(), Some(snapshot));
            }
            result => {
                let error = match result {
                    Err(e) => e.to_string(),
                    _ => "PR changed during review fetch".into(),
                };
                if reported.insert(index) {
                    let _ = error_tx.try_send(format!(
                        "GitHub token #{}: Review fetch failed for {id}: {error}",
                        index + 1
                    ));
                }
                cache.insert(
                    id.clone(),
                    ReviewCacheEntry {
                        version,
                        fresh_until: now,
                        retry_at: now + FAILURE_RETRY,
                    },
                );
                statuses.insert(id.clone(), None);
            }
        }
    }
    statuses
}

fn parse_state(s: &str) -> PrState {
    match s {
        "OPEN" => PrState::Open,
        "CLOSED" => PrState::Closed,
        "MERGED" => PrState::Merged,
        _ => PrState::Open,
    }
}

/// Convert a PR and its head-commit CI rollup from the same search response.
fn node_to_pr(node: &PrNode, role: PrRole) -> (PullRequest, String) {
    let id = PrId {
        owner: node.repository.owner.login.clone(),
        repo: node.repository.name.clone(),
        number: node.number,
    };
    let head_sha = node.head_ref_oid.clone();
    let pr = PullRequest {
        id,
        title: node.title.clone(),
        url: node.url.clone(),
        author_login: node
            .author
            .as_ref()
            .map(|a| a.login.clone())
            .unwrap_or_default(),
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
        ci_status: node.ci_status(),
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

#[cfg(test)]
pub fn merge_and_convert(
    author_nodes: &[PrNode],
    review_nodes: &[PrNode],
    mentioned_nodes: &[PrNode],
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
#[derive(Default)]
struct RawPoll {
    author: Vec<PrNode>,
    review: Vec<PrNode>,
    mentioned: Vec<PrNode>,
}

async fn fetch_raw(
    client: &GitHubClient,
    username: &str,
    include_closed: bool,
) -> Result<RawPoll, AppError> {
    let author_query = query::author_search_query(username);
    let author_closed_query = query::author_closed_search_query(username);
    let review_query = query::review_requested_search_query(username);
    let review_closed_query = query::review_requested_closed_search_query(username);
    let mentions_query = query::mentions_search_query(username);
    let mentions_closed_query = query::mentions_closed_search_query(username);
    let closed = |query: String| async move {
        if include_closed {
            client.search_prs(&query, 1).await
        } else {
            Ok(Vec::new())
        }
    };
    let (author_open, author_closed, review_open, review_closed, mentions_open, mentions_closed) = tokio::join!(
        client.search_prs(&author_query, MAX_PAGES),
        closed(author_closed_query),
        client.search_prs(&review_query, MAX_PAGES),
        closed(review_closed_query),
        client.search_prs(&mentions_query, MAX_PAGES),
        closed(mentions_closed_query),
    );
    let mut author = author_open?;
    author.extend(author_closed?);
    let mut review = review_open?;
    review.extend(review_closed?);
    let mut mentioned = mentions_open?;
    mentioned.extend(mentions_closed?);
    Ok(RawPoll {
        author,
        review,
        mentioned,
    })
}

struct TokenState {
    client: GitHubClient,
    cache: Option<RawPoll>,
    next_poll: tokio::time::Instant,
    backoff_secs: u64,
    retrying: bool,
    disabled: bool,
}

struct CacheMerge {
    prs: IndexMap<PrId, PullRequest>,
    shas: HashMap<PrId, String>,
    owners: HashMap<PrId, usize>,
    mention_ids: HashSet<PrId>,
}

fn merge_caches<'a>(caches: impl Iterator<Item = (usize, &'a RawPoll)>) -> CacheMerge {
    let caches: Vec<(usize, &RawPoll)> = caches.collect();
    let mut result: IndexMap<PrId, PullRequest> = IndexMap::new();
    let mut shas = HashMap::new();
    let mut owners = HashMap::new();
    let mut mentions = HashSet::new();
    for (_, raw) in &caches {
        for node in &raw.mentioned {
            mentions.insert(node_id(node));
        }
    }
    for role in [PrRole::Author, PrRole::ReviewRequested, PrRole::Mentioned] {
        for (client_index, raw) in &caches {
            let nodes = match role {
                PrRole::Author => &raw.author,
                PrRole::ReviewRequested => &raw.review,
                PrRole::Mentioned => &raw.mentioned,
            };
            for node in nodes {
                let (pr, sha) = node_to_pr(node, role);
                if result.get(&pr.id).is_some_and(|existing| {
                    existing.role != role || existing.updated_at >= pr.updated_at
                }) {
                    continue;
                }
                owners.insert(pr.id.clone(), *client_index);
                shas.insert(pr.id.clone(), sha);
                result.insert(pr.id.clone(), pr);
            }
        }
    }
    CacheMerge {
        prs: result,
        shas,
        owners,
        mention_ids: mentions,
    }
}

/// polling_loop の実行時依存(クライアント・対象ユーザー・ポーリング間隔・dismiss 状態)。
pub struct PollerContext {
    pub clients: Vec<GitHubClient>,
    pub username: String,
    pub interval: Duration,
    pub dismiss_store: Arc<Mutex<DismissStore>>,
}

pub async fn polling_loop(
    ctx: PollerContext,
    tx: mpsc::Sender<PollEvent>,
    error_tx: mpsc::Sender<String>,
    cancel: CancellationToken,
    mut refresh_rx: mpsc::Receiver<()>,
) {
    let username = ctx.username.clone();
    let interval = ctx.interval;
    let dismiss_store = ctx.dismiss_store.clone();
    let mut generation = 0u64;
    let mut review_cache = HashMap::new();
    let mut states: Vec<TokenState> = ctx
        .clients
        .into_iter()
        .map(|client| TokenState {
            client,
            cache: None,
            next_poll: tokio::time::Instant::now(),
            backoff_secs: 0,
            retrying: false,
            disabled: false,
        })
        .collect();

    loop {
        let now = tokio::time::Instant::now();
        let due: Vec<usize> = states
            .iter()
            .enumerate()
            .filter(|(_, state)| !state.disabled && state.next_poll <= now)
            .map(|(i, _)| i)
            .collect();
        if !due.is_empty() {
            let mut jobs = JoinSet::new();
            for &i in &due {
                let client = states[i].client.clone();
                let username = username.clone();
                let include_closed = states[i].cache.is_some();
                jobs.spawn(async move { (i, fetch_raw(&client, &username, include_closed).await) });
            }
            while let Some(joined) = jobs.join_next().await {
                let Ok((i, result)) = joined else { continue };
                match result {
                    Ok(raw) => {
                        states[i].cache = Some(raw);
                        states[i].backoff_secs = 0;
                        states[i].retrying = false;
                        states[i].next_poll = tokio::time::Instant::now() + interval;
                    }
                    Err(AppError::Auth(msg)) => {
                        states[i].disabled = true;
                        states[i].retrying = false;
                        let _ = error_tx
                            .send(format!("GitHub token #{}: Auth error: {msg}", i + 1))
                            .await;
                    }
                    Err(
                        error
                        @ (AppError::RateLimited { .. } | AppError::ServiceUnavailable { .. }),
                    ) => {
                        let retry_after_secs = match &error {
                            AppError::RateLimited {
                                retry_after_secs, ..
                            }
                            | AppError::ServiceUnavailable {
                                retry_after_secs, ..
                            } => *retry_after_secs,
                            _ => unreachable!(),
                        };
                        states[i].retrying = true;
                        // Search, CI and review updates all use GraphQL now.
                        states[i].next_poll = tokio::time::Instant::now()
                            + Duration::from_secs(retry_after_secs.max(1));
                        let _ = error_tx
                            .send(format!("GitHub token #{}: {error}", i + 1))
                            .await;
                    }
                    Err(e) => {
                        states[i].retrying = true;
                        states[i].backoff_secs = if states[i].backoff_secs == 0 {
                            2
                        } else {
                            (states[i].backoff_secs * 2).min(60)
                        };
                        states[i].next_poll = tokio::time::Instant::now()
                            + Duration::from_secs(states[i].backoff_secs);
                        let _ = error_tx
                            .send(format!(
                                "GitHub token #{}: {e} (retry in {}s)",
                                i + 1,
                                states[i].backoff_secs
                            ))
                            .await;
                    }
                }
            }
            if states.iter().any(|s| s.cache.is_some()) {
                let merged = merge_caches(
                    states
                        .iter()
                        .enumerate()
                        .filter_map(|(i, s)| s.cache.as_ref().map(|raw| (i, raw))),
                );
                let mut prs = merged.prs;
                let shas = merged.shas;
                let owners = merged.owners;
                let mention_ids = merged.mention_ids;
                apply_dismissals(
                    &dismiss_store,
                    &username,
                    &error_tx,
                    &mut prs,
                    mention_ids,
                    &owners,
                    &states,
                )
                .await;
                // Keep the existing CI event/notification path, but obtain all values
                // from this search snapshot. No per-PR CI network calls are needed.
                let statuses = prs
                    .iter_mut()
                    .filter_map(|(id, pr)| {
                        let status = pr.ci_status.take();
                        (pr.state == PrState::Open).then(|| {
                            (
                                id.clone(),
                                CiUpdate {
                                    head_sha: shas.get(id).cloned().unwrap_or_default(),
                                    status,
                                },
                            )
                        })
                    })
                    .collect();
                let review_prs = prs.clone();
                generation = generation.wrapping_add(1);
                let snapshot_at = Utc::now();
                let _ = tx
                    .send(PollEvent::Snapshot(PollPayload {
                        generation,
                        prs,
                        head_shas: shas.clone(),
                        polled_at: snapshot_at,
                    }))
                    .await;
                let _ = tx
                    .send(PollEvent::Ci(CiPayload {
                        generation,
                        statuses,
                    }))
                    .await;
                let enrich = async {
                    let statuses = fetch_reviews(
                        &states,
                        &owners,
                        &review_prs,
                        &shas,
                        &mut review_cache,
                        &error_tx,
                    )
                    .await;
                    let _ = tx
                        .send(PollEvent::Reviews(ReviewPayload {
                            generation,
                            statuses,
                        }))
                        .await;
                };
                tokio::select! {
                    _ = enrich => {},
                    _ = cancel.cancelled() => return,
                }
            }
        }
        let next = states
            .iter()
            .filter(|s| !s.disabled)
            .map(|s| s.next_poll)
            .min()
            .unwrap_or_else(|| tokio::time::Instant::now() + interval);
        tokio::select! {
            _ = tokio::time::sleep_until(next) => {},
            _ = refresh_rx.recv() => {
                let now = tokio::time::Instant::now();
                for s in &mut states { if !s.disabled && !s.retrying { s.next_poll = now; } }
            },
            _ = cancel.cancelled() => return,
        }
    }
}

async fn apply_dismissals(
    dismiss_store: &Arc<Mutex<DismissStore>>,
    username: &str,
    error_tx: &mpsc::Sender<String>,
    prs: &mut IndexMap<PrId, PullRequest>,
    mention_ids: HashSet<PrId>,
    owners: &HashMap<PrId, usize>,
    states: &[TokenState],
) {
    let dismissed = {
        let mut store = dismiss_store.lock().expect("dismiss store lock poisoned");
        store.retain_ids(&mention_ids);
        store.snapshot()
    };
    let candidates: Vec<(PrId, DateTime<Utc>)> = prs
        .iter()
        .filter(|(_, pr)| pr.role == PrRole::Mentioned)
        .filter_map(|(id, pr)| {
            dismissed
                .get(id)
                .copied()
                .filter(|at| pr.updated_at > *at)
                .map(|at| (id.clone(), at))
        })
        .collect();
    let mut undismissed = HashSet::new();
    let mut grouped: HashMap<usize, Vec<(PrId, DateTime<Utc>)>> = HashMap::new();
    for candidate in candidates {
        if let Some(&index) = owners.get(&candidate.0) {
            grouped.entry(index).or_default().push(candidate);
        }
    }
    for (index, candidates) in grouped {
        let Some(client) = states
            .get(index)
            .filter(|s| !s.disabled && !s.retrying)
            .map(|s| &s.client)
        else {
            continue;
        };
        let ids: Vec<PrId> = candidates.iter().map(|(id, _)| id.clone()).collect();
        match client.fetch_recent_comments(&ids).await {
            Ok(comments) => {
                for (id, dismissed_at) in &candidates {
                    if comments.get(id).is_some_and(|list| {
                        list.iter().any(|c| {
                            c.created_at > *dismissed_at
                                && !c
                                    .author_login
                                    .as_deref()
                                    .is_some_and(|a| a.eq_ignore_ascii_case(username))
                                && contains_mention(&c.body_text, username)
                        })
                    }) {
                        undismissed.insert(id.clone());
                    }
                }
            }
            Err(e) => {
                let _ = error_tx
                    .send(format!(
                        "GitHub token #{}: Mention re-check failed: {e}",
                        index + 1
                    ))
                    .await;
            }
        }
    }
    let (still_dismissed, save_error) = {
        let mut store = dismiss_store.lock().expect("dismiss store lock poisoned");
        for id in &undismissed {
            store.undismiss(id);
        }
        let save_error = if store.is_dirty() {
            store
                .save()
                .err()
                .map(|e| format!("Failed to save dismissals: {e}"))
        } else {
            None
        };
        (store.dismissed_ids(), save_error)
    };
    if let Some(message) = save_error {
        let _ = error_tx.send(message).await;
    }
    prs.retain(|id, pr| pr.role != PrRole::Mentioned || !still_dismissed.contains(id));
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
            commits: None,
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
        let (result, _) = merge_and_convert(&author, &[], &[]);
        assert_eq!(result.len(), 1);
        let pr = result.values().next().unwrap();
        assert_eq!(pr.role, PrRole::Author);
    }

    #[test]
    fn merge_review_only() {
        let review = vec![make_node("org", "repo", 1)];
        let (result, _) = merge_and_convert(&[], &review, &[]);
        assert_eq!(result.len(), 1);
        let pr = result.values().next().unwrap();
        assert_eq!(pr.role, PrRole::ReviewRequested);
    }

    #[test]
    fn merge_mentioned_only() {
        let mentioned = vec![make_node("org", "repo", 1)];
        let (result, _) = merge_and_convert(&[], &[], &mentioned);
        assert_eq!(result.len(), 1);
        let pr = result.values().next().unwrap();
        assert_eq!(pr.role, PrRole::Mentioned);
    }

    #[test]
    fn merge_author_wins_over_review_and_mentioned() {
        let author = vec![make_node("org", "repo", 1)];
        let review = vec![make_node("org", "repo", 1)];
        let mentioned = vec![make_node("org", "repo", 1)];
        let (result, _) = merge_and_convert(&author, &review, &mentioned);
        assert_eq!(result.len(), 1);
        let pr = result.values().next().unwrap();
        assert_eq!(pr.role, PrRole::Author);
    }

    #[test]
    fn merge_review_wins_over_mentioned() {
        let review = vec![make_node("org", "repo", 1)];
        let mentioned = vec![make_node("org", "repo", 1)];
        let (result, _) = merge_and_convert(&[], &review, &mentioned);
        assert_eq!(result.len(), 1);
        let pr = result.values().next().unwrap();
        assert_eq!(pr.role, PrRole::ReviewRequested);
    }

    #[test]
    fn merge_distinct_prs() {
        let author = vec![make_node("org", "repo", 1)];
        let review = vec![make_node("org", "repo", 2)];
        let mentioned = vec![make_node("org", "repo", 3)];
        let (result, _) = merge_and_convert(&author, &review, &mentioned);
        assert_eq!(result.len(), 3);
    }
    #[test]
    fn merge_caches_unions_clients_and_applies_role_priority() {
        let mut first = RawPoll::default();
        first.mentioned.push(make_node("org", "repo", 1));
        let mut second = RawPoll::default();
        second.author.push(make_node("org", "repo", 1));
        second.review.push(make_node("org", "repo", 2));

        let merged = merge_caches(vec![(0, &first), (1, &second)].into_iter());
        let prs = &merged.prs;
        let owners = &merged.owners;
        let mentions = &merged.mention_ids;
        assert_eq!(prs.len(), 2);
        assert_eq!(
            prs[&PrId {
                owner: "org".into(),
                repo: "repo".into(),
                number: 1
            }]
                .role,
            PrRole::Author
        );
        assert_eq!(
            owners[&PrId {
                owner: "org".into(),
                repo: "repo".into(),
                number: 1
            }],
            1
        );
        assert!(mentions.contains(&PrId {
            owner: "org".into(),
            repo: "repo".into(),
            number: 1
        }));
    }

    #[test]
    fn merge_caches_retains_previous_success_when_other_cache_is_absent() {
        let mut cached = RawPoll::default();
        cached.author.push(make_node("org", "repo", 7));
        let prs = merge_caches(vec![(0, &cached)].into_iter()).prs;
        assert!(prs.contains_key(&PrId {
            owner: "org".into(),
            repo: "repo".into(),
            number: 7
        }));
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
        let (pr, _) = node_to_pr(&node, PrRole::Author);
        assert_eq!(pr.author_login, "");
    }

    #[test]
    fn node_to_pr_invalid_timestamp_defaults_to_epoch() {
        let mut node = make_node("org", "repo", 1);
        node.created_at = "not-a-timestamp".to_string();
        node.updated_at = "also-invalid".to_string();
        let (pr, _) = node_to_pr(&node, PrRole::Author);
        assert_eq!(pr.created_at, DateTime::<Utc>::default());
        assert_eq!(pr.updated_at, DateTime::<Utc>::default());
    }

    #[test]
    fn node_to_pr_unknown_review_decision() {
        let mut node = make_node("org", "repo", 1);
        node.review_decision = Some("FUTURE_DECISION".to_string());
        let (pr, _) = node_to_pr(&node, PrRole::Author);
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
        let (pr, _) = node_to_pr(&node, PrRole::Author);
        assert_eq!(pr.last_commenter.as_deref(), Some("reviewer1"));
        assert_eq!(pr.total_comments, 5); // review_threads(0) + comments(5)
    }

    #[test]
    fn node_to_pr_empty_comments_gives_none() {
        let node = make_node("org", "repo", 1);
        let (pr, _) = node_to_pr(&node, PrRole::Author);
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
        let (pr, _) = node_to_pr(&node, PrRole::Author);
        assert_eq!(pr.last_commenter, None);
    }

    #[test]
    fn node_to_pr_propagates_head_sha() {
        let mut node = make_node("org", "repo", 1);
        node.head_ref_oid = "deadbeef".to_string();
        let (_, sha) = node_to_pr(&node, PrRole::Author);
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
    async fn recv_snapshot(rx: &mut mpsc::Receiver<PollEvent>) -> PollPayload {
        loop {
            match rx.recv().await.expect("poll event channel closed") {
                PollEvent::Snapshot(payload) => return payload,
                PollEvent::Ci(_) | PollEvent::Reviews(_) => {}
            }
        }
    }
    fn graphql_response_with_pr(number: u64) -> serde_json::Value {
        serde_json::json!({
            "data": {"search": {
                "issueCount": 1,
                "pageInfo": {"hasNextPage": false, "endCursor": null},
                "nodes": [{
                    "number": number, "title": format!("PR #{number}"),
                    "url": format!("https://github.com/org/repo/pull/{number}"),
                    "state": "OPEN", "isDraft": false,
                    "createdAt": "2024-01-01T00:00:00Z",
                    "updatedAt": "2024-01-01T00:00:00Z",
                    "reviewDecision": null, "headRefOid": "",
                    "author": {"login": "user"},
                    "repository": {"name": "repo", "owner": {"login": "org"}},
                    "comments": {"totalCount": 0, "nodes": []},
                    "reviewThreads": {"totalCount": 0}
                }]
            }}
        })
    }

    #[tokio::test]
    async fn polling_loop_retains_failed_token_cache_on_refresh() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(wiremock::matchers::header("authorization", "Bearer good"))
            .and(wiremock::matchers::body_string_contains("search(query:"))
            .respond_with(ResponseTemplate::new(200).set_body_json(graphql_response_with_pr(42)))
            .up_to_n_times(3)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(wiremock::matchers::header("authorization", "Bearer good"))
            .and(wiremock::matchers::body_string_contains("search(query:"))
            .respond_with(ResponseTemplate::new(200).set_body_json(graphql_response_with_pr(43)))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(wiremock::matchers::header("authorization", "Bearer bad"))
            .and(wiremock::matchers::body_string_contains("search(query:"))
            .respond_with(ResponseTemplate::new(200).set_body_json(graphql_response_with_pr(7)))
            .up_to_n_times(3)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(wiremock::matchers::header("authorization", "Bearer bad"))
            .and(wiremock::matchers::body_string_contains("search(query:"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;

        let ctx = PollerContext {
            clients: vec![
                GitHubClient::new_with_base_url("good".into(), server.uri()),
                GitHubClient::new_with_base_url("bad".into(), server.uri()),
            ],
            username: "user".into(),
            interval: Duration::from_secs(3600),
            dismiss_store: test_dismiss_store("partial-cache"),
        };
        let (tx, mut rx) = mpsc::channel(4);
        let (error_tx, mut error_rx) = mpsc::channel(4);
        let (refresh_tx, refresh_rx) = mpsc::channel(1);
        let cancel = CancellationToken::new();
        let task_cancel = cancel.clone();
        let handle = tokio::spawn(polling_loop(ctx, tx, error_tx, task_cancel, refresh_rx));

        let initial = tokio::time::timeout(Duration::from_secs(5), recv_snapshot(&mut rx))
            .await
            .expect("timeout waiting for initial payload");
        assert!(initial.prs.contains_key(&PrId {
            owner: "org".into(),
            repo: "repo".into(),
            number: 42
        }));
        assert!(initial.prs.contains_key(&PrId {
            owner: "org".into(),
            repo: "repo".into(),
            number: 7
        }));
        refresh_tx.send(()).await.unwrap();
        let refreshed = tokio::time::timeout(Duration::from_secs(5), recv_snapshot(&mut rx))
            .await
            .expect("timeout waiting for refreshed payload");
        assert!(refreshed.prs.contains_key(&PrId {
            owner: "org".into(),
            repo: "repo".into(),
            number: 43
        }));
        assert!(refreshed.prs.contains_key(&PrId {
            owner: "org".into(),
            repo: "repo".into(),
            number: 7
        }));
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let error = error_rx.recv().await.expect("error channel closed");
                if error.contains("GitHub token #2") && error.contains("Auth error") {
                    break;
                }
            }
        })
        .await
        .expect("timeout waiting for labelled auth error");
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .unwrap()
            .unwrap();
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
                    clients: vec![client],
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
        tokio::time::timeout(Duration::from_secs(5), recv_snapshot(&mut rx))
            .await
            .expect("timeout waiting for first poll result");

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
                    clients: vec![client],
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

    async fn recv_non_review(rx: &mut mpsc::Receiver<PollEvent>) -> Option<PollEvent> {
        loop {
            match rx.recv().await? {
                PollEvent::Reviews(_) => continue,
                event => return Some(event),
            }
        }
    }

    fn with_ci(
        mut response: serde_json::Value,
        sha: &str,
        state: Option<&str>,
    ) -> serde_json::Value {
        let node = &mut response["data"]["search"]["nodes"][0];
        node["headRefOid"] = sha.into();
        node["commits"] = serde_json::json!({"nodes": [{"commit": {
            "oid": sha,
            "statusCheckRollup": state.map(|state| serde_json::json!({
                "state": state, "contexts": {"totalCount": 1}
            }))
        }}]});
        response
    }

    #[test]
    fn ci_rollup_maps_states_and_rejects_empty_or_mismatched_heads() {
        use crate::types::CiStatus;
        for (state, expected) in [
            (Some("SUCCESS"), Some(CiStatus::Success)),
            (Some("FAILURE"), Some(CiStatus::Failure)),
            (Some("ERROR"), Some(CiStatus::Failure)),
            (Some("PENDING"), Some(CiStatus::Pending)),
            (Some("EXPECTED"), Some(CiStatus::Pending)),
            (Some("FUTURE_STATE"), None),
            (None, None),
        ] {
            let response = with_ci(graphql_response_with_pr(1), "sha", state);
            let node: PrNode =
                serde_json::from_value(response["data"]["search"]["nodes"][0].clone()).unwrap();
            assert_eq!(node_to_pr(&node, PrRole::Author).0.ci_status, expected);
        }
        let response = with_ci(graphql_response_with_pr(1), "sha", Some("SUCCESS"));
        let base = response["data"]["search"]["nodes"][0].clone();
        for replacement in [
            serde_json::Value::Null,
            serde_json::json!({"nodes": []}),
            serde_json::json!({"nodes": null}),
            serde_json::json!({"nodes": [null]}),
        ] {
            let mut node = base.clone();
            node["commits"] = replacement;
            assert_eq!(
                serde_json::from_value::<PrNode>(node).unwrap().ci_status(),
                None
            );
        }
        for sha in ["different", ""] {
            let mut node = base.clone();
            node["headRefOid"] = sha.into();
            assert_eq!(
                serde_json::from_value::<PrNode>(node).unwrap().ci_status(),
                None
            );
        }
        let mut node = base;
        node["commits"]["nodes"][0]["commit"]["statusCheckRollup"]["contexts"]["totalCount"] =
            0.into();
        assert_eq!(
            serde_json::from_value::<PrNode>(node).unwrap().ci_status(),
            None
        );
    }

    #[tokio::test]
    async fn polling_ci_tracks_same_sha_reruns_without_rest_requests() {
        use crate::types::CiStatus;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let server = MockServer::start().await;
        let phase = Arc::new(AtomicUsize::new(0));
        let response_phase = phase.clone();
        Mock::given(method("POST"))
            .and(wiremock::matchers::body_string_contains("search(query:"))
            .respond_with(move |request: &wiremock::Request| {
                let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
                assert!(
                    body["query"]
                        .as_str()
                        .unwrap()
                        .contains("statusCheckRollup")
                );
                let phase = response_phase.load(Ordering::SeqCst);
                let state = [
                    Some("PENDING"),
                    Some("SUCCESS"),
                    Some("PENDING"),
                    Some("FAILURE"),
                    None,
                ][phase];
                ResponseTemplate::new(200).set_body_json(with_ci(
                    graphql_response_with_pr(1),
                    if phase == 4 { "new-sha" } else { "sha" },
                    state,
                ))
            })
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(wiremock::matchers::body_string_contains("reviewRequests"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": {"repository": {"pullRequest": {
                    "headRefOid":"sha", "updatedAt":"2024-01-01T00:00:00Z", "state":"OPEN", "isDraft":false,
                    "author":{"login":"user"},
                    "reviews":{"nodes":[], "pageInfo":{"hasNextPage":false,"endCursor":null}},
                    "reviewRequests":{"nodes":[], "pageInfo":{"hasNextPage":false,"endCursor":null}}
                }}}
            }))).mount(&server).await;
        let ctx = PollerContext {
            clients: vec![GitHubClient::new_with_base_url(
                "token".into(),
                server.uri(),
            )],
            username: "user".into(),
            interval: Duration::from_secs(3600),
            dismiss_store: test_dismiss_store("graphql-ci"),
        };
        let (tx, mut rx) = mpsc::channel(16);
        let (error_tx, _errors) = mpsc::channel(16);
        let (refresh_tx, refresh_rx) = mpsc::channel(1);
        let cancel = CancellationToken::new();
        let handle = tokio::spawn(polling_loop(ctx, tx, error_tx, cancel.clone(), refresh_rx));
        for (step, expected) in [
            Some(CiStatus::Pending),
            Some(CiStatus::Success),
            Some(CiStatus::Pending),
            Some(CiStatus::Failure),
            None,
        ]
        .into_iter()
        .enumerate()
        {
            if step > 0 {
                phase.store(step, Ordering::SeqCst);
                refresh_tx.send(()).await.unwrap();
            }
            let snapshot = tokio::time::timeout(Duration::from_secs(5), recv_snapshot(&mut rx))
                .await
                .unwrap();
            let event = tokio::time::timeout(Duration::from_secs(5), recv_non_review(&mut rx))
                .await
                .unwrap()
                .unwrap();
            let PollEvent::Ci(ci) = event else {
                panic!("expected CI update after snapshot")
            };
            assert_eq!(ci.generation, snapshot.generation);
            assert_eq!(ci.statuses.len(), 1);
            let update = ci.statuses.values().next().unwrap();
            assert_eq!(update.status, expected);
            assert_eq!(update.head_sha, if step == 4 { "new-sha" } else { "sha" });
        }
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .unwrap()
            .unwrap();
        let requests = server.received_requests().await.unwrap();
        assert!(requests.iter().all(|r| r.method == "POST"));
        assert_eq!(
            requests
                .iter()
                .filter(|r| String::from_utf8_lossy(&r.body).contains("search(query:"))
                .count(),
            3 + 4 * 6
        );
    }

    #[tokio::test]
    async fn review_cache_rechecks_changes_and_ttl_but_skips_closed_history() {
        let server = MockServer::start().await;
        Mock::given(method("POST")).respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": {"repository": {"pullRequest": {
                "headRefOid":"sha", "updatedAt":"2026-01-01T00:00:00Z", "state":"OPEN", "isDraft":false,
                "author":{"login":"author"},
                "reviews":{"nodes":[], "pageInfo":{"hasNextPage":false,"endCursor":null}},
                "reviewRequests":{"nodes":[], "pageInfo":{"hasNextPage":false,"endCursor":null}}
            }}}
        }))).mount(&server).await;
        let states = vec![TokenState {
            client: GitHubClient::new_with_base_url("token".into(), server.uri()),
            cache: None,
            next_poll: tokio::time::Instant::now(),
            backoff_secs: 0,
            retrying: false,
            disabled: false,
        }];
        let mut node = make_node("org", "repo", 1);
        node.head_ref_oid = "sha".into();
        let (pr, sha) = node_to_pr(&node, PrRole::Author);
        let id = pr.id.clone();
        let mut closed = pr.clone();
        closed.id.number = 2;
        closed.state = PrState::Merged;
        let mut prs = IndexMap::from([(id.clone(), pr), (closed.id.clone(), closed.clone())]);
        let owners = HashMap::from([(id.clone(), 0), (closed.id.clone(), 0)]);
        let mut shas = HashMap::from([(id.clone(), sha), (closed.id.clone(), "sha".into())]);
        let mut cache = HashMap::new();
        let (tx, _rx) = mpsc::channel(10);
        assert_eq!(
            fetch_reviews(&states, &owners, &prs, &shas, &mut cache, &tx)
                .await
                .len(),
            1
        );
        assert!(
            fetch_reviews(&states, &owners, &prs, &shas, &mut cache, &tx)
                .await
                .is_empty()
        );
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
        prs.get_mut(&id).unwrap().updated_at += chrono::Duration::seconds(1);
        assert_eq!(
            fetch_reviews(&states, &owners, &prs, &shas, &mut cache, &tx)
                .await
                .len(),
            1
        );
        cache.get_mut(&id).unwrap().fresh_until = tokio::time::Instant::now();
        assert_eq!(
            fetch_reviews(&states, &owners, &prs, &shas, &mut cache, &tx)
                .await
                .len(),
            1
        );
        // A changed SHA invalidates the cache; a response for the old SHA is a failure.
        shas.insert(id.clone(), "new-sha".into());
        let result = fetch_reviews(&states, &owners, &prs, &shas, &mut cache, &tx).await;
        assert!(result[&id].is_none());
        assert!(
            fetch_reviews(&states, &owners, &prs, &shas, &mut cache, &tx)
                .await
                .is_empty()
        );
        assert_eq!(server.received_requests().await.unwrap().len(), 4);
    }

    #[tokio::test]
    async fn enrichment_skips_disabled_and_retrying_tokens() {
        let server = MockServer::start().await;
        let (pr, _) = node_to_pr(&make_node("o", "r", 1), PrRole::Author);
        let id = pr.id.clone();
        let prs = IndexMap::from([(id.clone(), pr)]);
        let owners = HashMap::from([(id.clone(), 0)]);
        let shas = HashMap::from([(id.clone(), "sha".into())]);
        let (tx, _rx) = mpsc::channel(10);
        for disabled in [true, false] {
            let states = vec![TokenState {
                client: GitHubClient::new_with_base_url("token".into(), server.uri()),
                cache: None,
                next_poll: tokio::time::Instant::now(),
                backoff_secs: 0,
                retrying: !disabled,
                disabled,
            }];
            let mut cache = HashMap::new();
            assert!(
                fetch_reviews(&states, &owners, &prs, &shas, &mut cache, &tx).await[&id].is_none()
            );
        }
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn startup_omits_closed_searches_but_refresh_detects_closed_transitions() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(empty_graphql_response()))
            .mount(&server)
            .await;
        let client = GitHubClient::new_with_base_url("token".into(), server.uri());
        fetch_raw(&client, "user", false).await.unwrap();
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 3);
        assert!(requests.iter().all(|r| {
            !r.body_json::<serde_json::Value>().unwrap()["variables"]["query"]
                .as_str()
                .unwrap()
                .contains("-is:open")
        }));
        fetch_raw(&client, "user", true).await.unwrap();
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 9);
        assert_eq!(
            requests
                .iter()
                .filter(
                    |r| r.body_json::<serde_json::Value>().unwrap()["variables"]["query"]
                        .as_str()
                        .unwrap()
                        .contains("-is:open")
                )
                .count(),
            3
        );
    }
}
