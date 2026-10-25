use chrono::{DateTime, Utc};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct GraphQlResponse {
    pub data: Option<ResponseData>,
    pub errors: Option<Vec<GraphQlError>>,
}

#[derive(Debug, Deserialize)]
pub struct GraphQlError {
    pub message: String,
}

#[derive(Debug, Deserialize)]
pub struct ResponseData {
    pub search: SearchResult,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchResult {
    #[allow(dead_code)]
    pub issue_count: u64,
    pub page_info: PageInfo,
    pub nodes: Vec<PrNode>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PageInfo {
    pub has_next_page: bool,
    pub end_cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TotalCount {
    pub total_count: u64,
}

#[derive(Debug, Deserialize)]
pub struct CommentNode {
    pub author: Option<ActorNode>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommentsConnection {
    pub total_count: u64,
    #[serde(default)]
    pub nodes: Vec<CommentNode>,
}

/// [`crate::github::client::GitHubClient::fetch_recent_comments`] が返す直近コメント。
/// レスポンスはエイリアスキーが動的なため serde ではなく手動パースで組み立てる。
#[derive(Debug, Clone)]
pub struct RecentComment {
    pub body_text: String,
    pub created_at: DateTime<Utc>,
    pub author_login: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrNode {
    pub number: u64,
    pub title: String,
    pub url: String,
    pub state: String,
    pub is_draft: bool,
    pub created_at: String,
    pub updated_at: String,
    pub review_decision: Option<String>,
    #[serde(default)]
    pub head_ref_oid: String,
    #[serde(default)]
    pub commits: Option<CommitConnection>,
    pub author: Option<ActorNode>,
    pub repository: RepoNode,
    pub comments: CommentsConnection,
    pub review_threads: TotalCount,
}

#[derive(Debug, Deserialize)]
pub struct ActorNode {
    pub login: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RepoNode {
    pub name: String,
    pub owner: RepoOwnerNode,
}

#[derive(Debug, Deserialize)]
pub struct RepoOwnerNode {
    pub login: String,
}

/// Only the last PR commit is requested; verify its OID before using its CI state.
#[derive(Debug, Deserialize)]
pub struct CommitConnection {
    pub nodes: Option<Vec<Option<CommitNode>>>,
}
#[derive(Debug, Deserialize)]
pub struct CommitNode {
    pub commit: Commit,
}
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Commit {
    pub oid: String,
    pub status_check_rollup: Option<StatusCheckRollup>,
}
#[derive(Debug, Deserialize)]
pub struct StatusCheckRollup {
    pub state: String,
    pub contexts: TotalCount,
}
impl PrNode {
    pub fn ci_status(&self) -> Option<crate::types::CiStatus> {
        use crate::types::CiStatus;
        let commit = &self
            .commits
            .as_ref()?
            .nodes
            .as_ref()?
            .last()?
            .as_ref()?
            .commit;
        if self.head_ref_oid.is_empty() || commit.oid != self.head_ref_oid {
            return None;
        }
        let rollup = commit.status_check_rollup.as_ref()?;
        if rollup.contexts.total_count == 0 {
            return None;
        }
        match rollup.state.as_str() {
            "SUCCESS" => Some(CiStatus::Success),
            "FAILURE" | "ERROR" => Some(CiStatus::Failure),
            "PENDING" | "EXPECTED" => Some(CiStatus::Pending),
            _ => None,
        }
    }
}
