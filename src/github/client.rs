use reqwest::Client;
use serde_json::json;

use crate::error::AppError;
use crate::github::query::{SEARCH_PRS_QUERY, SEARCH_PRS_QUERY_BASIC};
use crate::github::types::{GraphQlResponse, PrNode};

const GITHUB_GRAPHQL_URL: &str = "https://api.github.com/graphql";
const PAGE_SIZE: u64 = 50;
pub const MAX_PAGES: u64 = 4;

pub struct GitHubClient {
    client: Client,
    token: String,
    base_url: String,
}

impl GitHubClient {
    pub fn new(token: String) -> Self {
        Self {
            client: Client::builder()
                .user_agent("prtop/0.1.0")
                .build()
                .expect("failed to build HTTP client"),
            token,
            base_url: GITHUB_GRAPHQL_URL.to_string(),
        }
    }

    #[cfg(test)]
    pub fn new_with_base_url(token: String, base_url: String) -> Self {
        Self {
            client: Client::builder()
                .user_agent("prtop/0.1.0")
                .build()
                .expect("failed to build HTTP client"),
            token,
            base_url,
        }
    }

    pub async fn search_prs(
        &self,
        search_query: &str,
        max_pages: u64,
    ) -> Result<Vec<PrNode>, AppError> {
        match self
            .search_prs_with_query(search_query, max_pages, SEARCH_PRS_QUERY)
            .await
        {
            Ok(nodes) => Ok(nodes),
            Err(AppError::GraphQl(msg)) if msg.contains("Resource not accessible") => {
                // commits field requires Contents read permission which may not
                // be available for repos where the user is only a reviewer.
                // Fall back to the basic query without the commits field.
                self.search_prs_with_query(search_query, max_pages, SEARCH_PRS_QUERY_BASIC)
                    .await
            }
            Err(e) => Err(e),
        }
    }

    async fn search_prs_with_query(
        &self,
        search_query: &str,
        max_pages: u64,
        graphql_query: &str,
    ) -> Result<Vec<PrNode>, AppError> {
        let mut all_nodes = Vec::new();
        let mut cursor: Option<String> = None;

        for _ in 0..max_pages {
            let variables = json!({
                "query": search_query,
                "first": PAGE_SIZE,
                "after": cursor,
            });

            let body = json!({
                "query": graphql_query,
                "variables": variables,
            });

            let response = self
                .client
                .post(&self.base_url)
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::method;
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
            .respond_with(ResponseTemplate::new(200).set_body_json(empty_response()))
            .mount(&server)
            .await;

        let client = GitHubClient::new_with_base_url("token".to_string(), server.uri());
        let result = client.search_prs("author:user", MAX_PAGES).await.unwrap();
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn returns_auth_error_on_401_without_rate_limit() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
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
    async fn falls_back_to_basic_query_on_resource_not_accessible() {
        use wiremock::matchers::body_string_contains;

        let server = MockServer::start().await;

        // First call (full query with commits) returns "Resource not accessible" error
        Mock::given(method("POST"))
            .and(body_string_contains("statusCheckRollup"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": null,
                "errors": [{"message": "Resource not accessible by personal access token"}]
            })))
            .up_to_n_times(1)
            .mount(&server)
            .await;

        // Second call (basic query without commits) returns success
        Mock::given(method("POST"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(empty_response()))
            .mount(&server)
            .await;

        let client = GitHubClient::new_with_base_url("token".to_string(), server.uri());
        let result = client.search_prs("author:user", MAX_PAGES).await;
        assert!(result.is_ok(), "expected fallback to succeed: {result:?}");

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 2, "expected 2 requests (full + fallback)");
    }

    #[tokio::test]
    async fn non_resource_graphql_error_does_not_fallback() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "errors": [{"message": "Some other error"}]
            })))
            .mount(&server)
            .await;

        let client = GitHubClient::new_with_base_url("token".to_string(), server.uri());
        let result = client.search_prs("author:user", MAX_PAGES).await;
        assert!(
            matches!(result, Err(AppError::GraphQl(ref msg)) if msg.contains("Some other error"))
        );

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1, "should not retry on non-resource error");
    }

    #[tokio::test]
    async fn stops_at_max_pages() {
        let server = MockServer::start().await;
        // Always return hasNextPage: true to force MAX_PAGES cutoff
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(paged_response()))
            .mount(&server)
            .await;

        let client = GitHubClient::new_with_base_url("token".to_string(), server.uri());
        let _result = client.search_prs("author:user", MAX_PAGES).await.unwrap();

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), MAX_PAGES as usize);
    }
}
