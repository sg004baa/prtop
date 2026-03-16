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
    pub author: Option<ActorNode>,
    pub repository: RepoNode,
    pub comments: TotalCount,
    pub review_comments: TotalCount,
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
