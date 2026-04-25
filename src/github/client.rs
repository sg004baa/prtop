use reqwest::Client;
use serde_json::json;

use crate::error::AppError;
use crate::github::query::SEARCH_PRS_QUERY;
use crate::github::types::{CheckRunsResponse, CombinedStatusResponse, GraphQlResponse, PrNode};
use crate::types::CiStatus;

const GITHUB_API_BASE: &str = "https://api.github.com";
const PAGE_SIZE: u64 = 50;
pub const MAX_PAGES: u64 = 4;

#[derive(Clone)]
pub struct GitHubClient {
    client: Client,
    token: String,
    api_base: String,
}

impl GitHubClient {
    pub fn new(token: String) -> Self {
        Self {
            client: Client::builder()
                .user_agent("prtop/0.1.0")
                .build()
                .expect("failed to build HTTP client"),
            token,
            api_base: GITHUB_API_BASE.to_string(),
        }
    }

    #[cfg(test)]
    pub fn new_with_base_url(token: String, api_base: String) -> Self {
        Self {
            client: Client::builder()
                .user_agent("prtop/0.1.0")
                .build()
                .expect("failed to build HTTP client"),
            token,
            api_base,
        }
    }

    pub async fn search_prs(
        &self,
        search_query: &str,
        max_pages: u64,
    ) -> Result<Vec<PrNode>, AppError> {
        let mut all_nodes = Vec::new();
        let mut cursor: Option<String> = None;
        let url = format!("{}/graphql", self.api_base);

        for _ in 0..max_pages {
            let variables = json!({
                "query": search_query,
                "first": PAGE_SIZE,
                "after": cursor,
            });

            let body = json!({
                "query": SEARCH_PRS_QUERY,
                "variables": variables,
            });

            let response = self
                .client
                .post(&url)
                .bearer_auth(&self.token)
                .json(&body)
                .send()
                .await?;

            let status = response.status();

            if status == reqwest::StatusCode::UNAUTHORIZED
                || status == reqwest::StatusCode::FORBIDDEN
            {
                let retry_after = response
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<u64>().ok());

                let rate_remaining = response
                    .headers()
                    .get("x-ratelimit-remaining")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<u64>().ok());

                if rate_remaining == Some(0) || retry_after.is_some() {
                    return Err(AppError::RateLimited {
                        retry_after_secs: retry_after.unwrap_or(60),
                    });
                }

                let text = response.text().await.unwrap_or_default();
                return Err(AppError::Auth(format!("{status}: {text}")));
            }

            if !status.is_success() {
                let text = response.text().await.unwrap_or_default();
                return Err(AppError::GraphQl(format!("{status}: {text}")));
            }

            let gql_response: GraphQlResponse = response.json().await?;

            if let Some(errors) = gql_response.errors {
                let msgs: Vec<String> = errors.into_iter().map(|e| e.message).collect();
                return Err(AppError::GraphQl(msgs.join("; ")));
            }

            let data = gql_response
                .data
                .ok_or_else(|| AppError::GraphQl("No data in response".to_string()))?;

            let search = data.search;
            all_nodes.extend(search.nodes);

            if !search.page_info.has_next_page {
                break;
            }
            cursor = search.page_info.end_cursor;
        }

        Ok(all_nodes)
    }

    /// Fetch the CI status for a commit by combining the legacy commit-status API
    /// (`/repos/{owner}/{repo}/commits/{sha}/status`) and the check-runs API
    /// (`/repos/{owner}/{repo}/commits/{sha}/check-runs`).
    ///
    /// Returns `None` when no CI is configured for the commit, when the SHA is empty,
    /// or when the token lacks permission to read these endpoints (404/403). Other
    /// errors propagate so they are visible to the caller.
    pub async fn fetch_ci_status(
        &self,
        owner: &str,
        repo: &str,
        sha: &str,
    ) -> Result<Option<CiStatus>, AppError> {
        if sha.is_empty() {
            return Ok(None);
        }

        let combined = self.fetch_combined_status(owner, repo, sha).await?;
        let check_runs = self.fetch_check_runs(owner, repo, sha).await?;

        Ok(compute_ci_status(combined.as_ref(), check_runs.as_ref()))
    }

    async fn fetch_combined_status(
        &self,
        owner: &str,
        repo: &str,
        sha: &str,
    ) -> Result<Option<CombinedStatusResponse>, AppError> {
        let url = format!(
            "{}/repos/{}/{}/commits/{}/status",
            self.api_base, owner, repo, sha
        );
        let response = self
            .client
            .get(&url)
            .bearer_auth(&self.token)
            .header("Accept", "application/vnd.github+json")
            .send()
            .await?;

        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND || status == reqwest::StatusCode::FORBIDDEN {
            return Ok(None);
        }
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(AppError::GraphQl(format!("{status}: {text}")));
        }
        Ok(Some(response.json().await?))
    }

    async fn fetch_check_runs(
        &self,
        owner: &str,
        repo: &str,
        sha: &str,
    ) -> Result<Option<CheckRunsResponse>, AppError> {
        let url = format!(
            "{}/repos/{}/{}/commits/{}/check-runs",
            self.api_base, owner, repo, sha
        );
        let response = self
            .client
            .get(&url)
            .bearer_auth(&self.token)
            .header("Accept", "application/vnd.github+json")
            .send()
            .await?;

        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND || status == reqwest::StatusCode::FORBIDDEN {
            return Ok(None);
        }
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(AppError::GraphQl(format!("{status}: {text}")));
        }
        Ok(Some(response.json().await?))
    }
}

fn compute_ci_status(
    combined: Option<&CombinedStatusResponse>,
    check_runs: Option<&CheckRunsResponse>,
) -> Option<CiStatus> {
    let combined_present = combined.is_some_and(|c| !c.statuses.is_empty());
    let check_runs_present = check_runs.is_some_and(|cr| cr.total_count > 0);
    if !combined_present && !check_runs_present {
        return None;
    }

    // Pending if any check run is not completed.
    if let Some(cr) = check_runs {
        for run in &cr.check_runs {
            if run.status != "completed" {
                return Some(CiStatus::Pending);
            }
        }
    }

    // Pending if combined-status is still running.
    if let Some(c) = combined
        && combined_present
        && c.state == "pending"
    {
        return Some(CiStatus::Pending);
    }

    // Failure if any check run failed (failure / cancelled / timed_out).
    if let Some(cr) = check_runs {
        for run in &cr.check_runs {
            if matches!(
                run.conclusion.as_deref(),
                Some("failure" | "cancelled" | "timed_out")
            ) {
                return Some(CiStatus::Failure);
            }
        }
    }

    // Failure if combined-status failed/errored.
    if let Some(c) = combined
        && (c.state == "failure" || c.state == "error")
    {
        return Some(CiStatus::Failure);
    }

    Some(CiStatus::Success)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn empty_response() -> serde_json::Value {
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

    fn paged_response() -> serde_json::Value {
        serde_json::json!({
            "data": {
                "search": {
                    "issueCount": 0,
                    "pageInfo": {"hasNextPage": true, "endCursor": "cursor123"},
                    "nodes": []
                }
            }
        })
    }

    #[tokio::test]
    async fn returns_empty_nodes_on_empty_response() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(ResponseTemplate::new(200).set_body_json(empty_response()))
            .mount(&server)
            .await;

        let client = GitHubClient::new_with_base_url("token".to_string(), server.uri());
        let nodes = client.search_prs("author:user", MAX_PAGES).await.unwrap();
        assert!(nodes.is_empty());
    }

    #[tokio::test]
    async fn returns_auth_error_on_401_without_rate_limit() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;

        let client = GitHubClient::new_with_base_url("bad_token".to_string(), server.uri());
        let result = client.search_prs("author:user", MAX_PAGES).await;
        assert!(matches!(result, Err(AppError::Auth(_))));
    }

    #[tokio::test]
    async fn returns_auth_error_on_403_without_rate_limit() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&server)
            .await;

        let client = GitHubClient::new_with_base_url("token".to_string(), server.uri());
        let result = client.search_prs("author:user", MAX_PAGES).await;
        assert!(matches!(result, Err(AppError::Auth(_))));
    }

    #[tokio::test]
    async fn returns_rate_limited_on_401_with_rate_remaining_zero() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(ResponseTemplate::new(401).append_header("x-ratelimit-remaining", "0"))
            .mount(&server)
            .await;

        let client = GitHubClient::new_with_base_url("token".to_string(), server.uri());
        let result = client.search_prs("author:user", MAX_PAGES).await;
        assert!(matches!(
            result,
            Err(AppError::RateLimited {
                retry_after_secs: 60
            })
        ));
    }

    #[tokio::test]
    async fn returns_rate_limited_with_retry_after_header() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(ResponseTemplate::new(403).append_header("retry-after", "30"))
            .mount(&server)
            .await;

        let client = GitHubClient::new_with_base_url("token".to_string(), server.uri());
        let result = client.search_prs("author:user", MAX_PAGES).await;
        assert!(matches!(
            result,
            Err(AppError::RateLimited {
                retry_after_secs: 30
            })
        ));
    }

    #[tokio::test]
    async fn returns_graphql_error_on_errors_in_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "errors": [{"message": "Something went wrong"}]
            })))
            .mount(&server)
            .await;

        let client = GitHubClient::new_with_base_url("token".to_string(), server.uri());
        let result = client.search_prs("author:user", MAX_PAGES).await;
        assert!(matches!(result, Err(AppError::GraphQl(_))));
    }

    #[tokio::test]
    async fn returns_graphql_error_when_no_data() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"data": null})),
            )
            .mount(&server)
            .await;

        let client = GitHubClient::new_with_base_url("token".to_string(), server.uri());
        let result = client.search_prs("author:user", MAX_PAGES).await;
        assert!(matches!(result, Err(AppError::GraphQl(_))));
    }

    #[tokio::test]
    async fn stops_at_max_pages() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(ResponseTemplate::new(200).set_body_json(paged_response()))
            .mount(&server)
            .await;

        let client = GitHubClient::new_with_base_url("token".to_string(), server.uri());
        let _result = client.search_prs("author:user", MAX_PAGES).await.unwrap();

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), MAX_PAGES as usize);
    }

    // --- fetch_ci_status ---

    #[tokio::test]
    async fn ci_status_empty_sha_returns_none() {
        let server = MockServer::start().await;
        let client = GitHubClient::new_with_base_url("token".to_string(), server.uri());
        let result = client.fetch_ci_status("o", "r", "").await.unwrap();
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn ci_status_no_statuses_or_check_runs_returns_none() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/commits/abc/status"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "state": "pending",
                "statuses": []
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/commits/abc/check-runs"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "total_count": 0,
                "check_runs": []
            })))
            .mount(&server)
            .await;

        let client = GitHubClient::new_with_base_url("token".to_string(), server.uri());
        let result = client.fetch_ci_status("o", "r", "abc").await.unwrap();
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn ci_status_pending_check_run_returns_pending() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/commits/abc/status"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "state": "success",
                "statuses": []
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/commits/abc/check-runs"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "total_count": 2,
                "check_runs": [
                    {"status": "in_progress", "conclusion": null},
                    {"status": "completed", "conclusion": "success"}
                ]
            })))
            .mount(&server)
            .await;

        let client = GitHubClient::new_with_base_url("token".to_string(), server.uri());
        let result = client.fetch_ci_status("o", "r", "abc").await.unwrap();
        assert_eq!(result, Some(CiStatus::Pending));
    }

    #[tokio::test]
    async fn ci_status_all_completed_success_returns_success() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/commits/abc/status"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "state": "success",
                "statuses": []
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/commits/abc/check-runs"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "total_count": 1,
                "check_runs": [
                    {"status": "completed", "conclusion": "success"}
                ]
            })))
            .mount(&server)
            .await;

        let client = GitHubClient::new_with_base_url("token".to_string(), server.uri());
        let result = client.fetch_ci_status("o", "r", "abc").await.unwrap();
        assert_eq!(result, Some(CiStatus::Success));
    }

    #[tokio::test]
    async fn ci_status_failed_check_run_returns_failure() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/commits/abc/status"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "state": "success",
                "statuses": []
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/commits/abc/check-runs"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "total_count": 1,
                "check_runs": [
                    {"status": "completed", "conclusion": "failure"}
                ]
            })))
            .mount(&server)
            .await;

        let client = GitHubClient::new_with_base_url("token".to_string(), server.uri());
        let result = client.fetch_ci_status("o", "r", "abc").await.unwrap();
        assert_eq!(result, Some(CiStatus::Failure));
    }

    #[tokio::test]
    async fn ci_status_combined_failure_returns_failure() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/commits/abc/status"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "state": "failure",
                "statuses": [{"state": "failure"}]
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/commits/abc/check-runs"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "total_count": 0,
                "check_runs": []
            })))
            .mount(&server)
            .await;

        let client = GitHubClient::new_with_base_url("token".to_string(), server.uri());
        let result = client.fetch_ci_status("o", "r", "abc").await.unwrap();
        assert_eq!(result, Some(CiStatus::Failure));
    }

    #[tokio::test]
    async fn ci_status_403_on_check_runs_falls_back_to_combined_only() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/commits/abc/status"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "state": "success",
                "statuses": [{"state": "success"}]
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/commits/abc/check-runs"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&server)
            .await;

        let client = GitHubClient::new_with_base_url("token".to_string(), server.uri());
        let result = client.fetch_ci_status("o", "r", "abc").await.unwrap();
        assert_eq!(result, Some(CiStatus::Success));
    }

    #[tokio::test]
    async fn ci_status_404_on_both_returns_none() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/commits/abc/status"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/commits/abc/check-runs"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let client = GitHubClient::new_with_base_url("token".to_string(), server.uri());
        let result = client.fetch_ci_status("o", "r", "abc").await.unwrap();
        assert_eq!(result, None);
    }
}
