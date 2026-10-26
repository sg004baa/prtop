use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

use chrono::{DateTime, Utc};
use reqwest::Client;
use serde_json::json;

use super::limits::{Limits, SharedLimits, for_viewer};
use crate::error::AppError;
use crate::github::query::{self, SEARCH_PRS_QUERY};
use crate::github::types::{GraphQlResponse, PrNode, RecentComment};
use crate::types::PrId;

const GITHUB_API_BASE: &str = "https://api.github.com";
const PAGE_SIZE: u64 = 50;
pub const MAX_PAGES: u64 = 4;

#[derive(Clone)]
pub struct GitHubClient {
    pub(super) client: Client,
    pub(super) token: String,
    pub(super) api_base: String,
    gate: Arc<Mutex<RequestGate>>,
}

#[derive(Default)]
struct RequestGate {
    limits: SharedLimits,
    services: Limits,
    viewer: Option<String>,
    disabled: bool,
    ci_disabled: bool,
    ci_warning: Option<String>,
}

impl GitHubClient {
    /// All clones share one request gate, including searches and review summaries.
    /// Keep the gate through body decoding so HTTP-200 GraphQL limits stop queued work.
    pub(super) async fn send_json<T: serde::de::DeserializeOwned>(
        &self,
        request: reqwest::RequestBuilder,
        allow_missing: bool,
    ) -> Result<Option<T>, AppError> {
        let mut gate = self.gate.lock().await;
        if gate.disabled {
            return Err(AppError::Auth(
                "Token disabled after authentication failure".into(),
            ));
        }
        let request = request.timeout(Duration::from_secs(30)).build()?;
        let endpoint = request.url().path().to_string();
        let resource = if endpoint == "/graphql" {
            "graphql"
        } else {
            "core"
        };
        if let Some(error) = gate.services.check(resource) {
            return Err(error);
        }
        if let Some(error) = gate.limits.lock().expect("rate limits").check(resource) {
            return Err(error);
        }
        let response = self.client.execute(request).await?;
        let status = response.status();
        let headers = response.headers();
        let retry = headers
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| {
                s.parse::<u64>().ok().or_else(|| {
                    chrono::DateTime::parse_from_rfc2822(s)
                        .ok()
                        .map(|d| (d.timestamp() - Utc::now().timestamp()).max(1) as u64)
                })
            });
        let reset_at = headers
            .get("x-ratelimit-reset")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<i64>().ok());
        let reset = reset_at.map(|t| t.saturating_sub(Utc::now().timestamp()).max(1) as u64);
        let remaining = headers
            .get("x-ratelimit-remaining")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok());
        let response_resource = headers
            .get("x-ratelimit-resource")
            .and_then(|v| v.to_str().ok())
            .filter(|r| matches!(*r, "core" | "graphql" | "search"))
            .unwrap_or(resource)
            .to_string();
        // Upstream/proxy failures often contain an entire HTML error page. Do
        // not read or expose it; classify by status and respect the retry header.
        let http_details = format!("HTTP {status}; endpoint={endpoint}");
        if status.is_server_error() {
            let cooldown = gate
                .services
                .record(resource, retry.unwrap_or(60), http_details, true);
            return Err(cooldown.error().expect("new cooldown"));
        }
        let exhausted = remaining == Some(0);
        let body = response.text().await?;
        let value = serde_json::from_str::<serde_json::Value>(&body);
        if let Some(viewer) = value
            .as_ref()
            .ok()
            .and_then(|v| v.pointer("/data/viewer/id"))
            .and_then(|v| v.as_str())
            && gate.viewer.as_deref() != Some(viewer)
        {
            gate.limits = for_viewer(&self.api_base, viewer, &gate.limits);
            gate.viewer = Some(viewer.to_string());
        }
        let gql_limited = value
            .as_ref()
            .ok()
            .and_then(|v| v.get("errors"))
            .and_then(|v| v.as_array())
            .is_some_and(|errors| {
                errors.iter().any(|e| {
                    e.get("type").and_then(|v| v.as_str()) == Some("RATE_LIMITED")
                        || e.get("message")
                            .and_then(|v| v.as_str())
                            .is_some_and(rate_limit_message)
                })
            });
        let error_text = if status.is_success() {
            value
                .as_ref()
                .ok()
                .and_then(|v| v.get("errors"))
                .and_then(|v| v.as_array())
                .map(|errors| {
                    errors
                        .iter()
                        .filter_map(|e| e.get("message").and_then(|m| m.as_str()))
                        .collect::<Vec<_>>()
                        .join("; ")
                })
                .unwrap_or_default()
        } else {
            value
                .as_ref()
                .ok()
                .and_then(|v| v.get("message"))
                .and_then(|m| m.as_str())
                .unwrap_or_default()
                .to_string()
        };
        let lower = error_text.to_ascii_lowercase();
        let secondary = lower.contains("secondary rate limit") || lower.contains("abuse detection");
        let limited = (matches!(status.as_u16(), 403 | 429)
            && (exhausted
                || retry.is_some()
                || rate_limit_message(&error_text)
                || status.as_u16() == 429))
            || (status.is_success() && gql_limited);
        let diagnostic = |kind: &str| {
            format!(
                "{kind}; HTTP {}; endpoint={endpoint}; resource={response_resource}; remaining={}; reset={}",
                status.as_u16(),
                remaining
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "unknown".into()),
                reset_at
                    .and_then(|t| chrono::DateTime::from_timestamp(t, 0))
                    .map(|d| d.to_rfc3339())
                    .unwrap_or_else(|| "unknown".into())
            )
        };
        if limited || (status.is_success() && exhausted) {
            // Unknown 403/429 limits are conservatively shared; GraphQL RATE_LIMITED
            // without secondary evidence applies to its own primary bucket.
            let primary = !secondary && (exhausted || (status.is_success() && gql_limited));
            let bucket = if primary {
                response_resource.as_str()
            } else {
                "*"
            };
            let seconds = if primary {
                retry.unwrap_or(0).max(reset.unwrap_or(60))
            } else {
                retry.unwrap_or(60).max(1)
            };
            let cooldown = gate.limits.lock().expect("rate limits").record(
                bucket,
                seconds,
                diagnostic(if primary {
                    "primary limit"
                } else {
                    "secondary/shared limit"
                }),
                false,
            );
            if limited {
                return Err(cooldown.error().expect("new cooldown"));
            }
        }
        if status.as_u16() == 401 || (status.as_u16() == 403 && !allow_missing) {
            gate.disabled = true;
            return Err(AppError::Auth(http_details));
        }
        if allow_missing && matches!(status.as_u16(), 403 | 404) {
            return Ok(None);
        }
        if !status.is_success() {
            return Err(AppError::Http(http_details));
        }
        let value = value
            .map_err(|_| AppError::InvalidResponse(format!("Expected JSON; {http_details}")))?;
        serde_json::from_value(value).map(Some).map_err(|_| {
            AppError::InvalidResponse(format!("Unexpected JSON structure; {http_details}"))
        })
    }

    pub fn new(token: String) -> Self {
        Self {
            client: Client::builder()
                .user_agent(concat!("prtop/", env!("CARGO_PKG_VERSION")))
                .build()
                .expect("failed to build HTTP client"),
            gate: Arc::new(Mutex::new(RequestGate::default())),
            token,
            api_base: GITHUB_API_BASE.to_string(),
        }
    }

    #[cfg(test)]
    pub fn new_with_base_url(token: String, api_base: String) -> Self {
        Self {
            client: Client::builder()
                .user_agent(concat!("prtop/", env!("CARGO_PKG_VERSION")))
                .build()
                .expect("failed to build HTTP client"),
            gate: Arc::new(Mutex::new(RequestGate::default())),
            token,
            api_base,
        }
    }

    pub async fn ci_warning(&self) -> Option<String> {
        self.gate.lock().await.ci_warning.clone()
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
            // Retry the same page once without optional CI fields on a permission
            // error. Inspect errors before typed decoding: denied fields may null
            // their parent objects, making an otherwise useful response unparseable.
            let data = loop {
                let include_ci = !self.gate.lock().await.ci_disabled;
                let variables = json!({
                    "query": search_query,
                    "first": PAGE_SIZE,
                    "after": cursor,
                    "includeCI": include_ci,
                });
                let body = json!({"query": SEARCH_PRS_QUERY, "variables": variables});
                let request = self.client.post(&url).bearer_auth(&self.token).json(&body);
                let value: serde_json::Value = self
                    .send_json(request, false)
                    .await?
                    .ok_or_else(|| AppError::GraphQl("Missing response".into()))?;
                if let Some(errors) = value.get("errors").and_then(|v| v.as_array())
                    && !errors.is_empty()
                {
                    let permission_only = errors.iter().all(|e| {
                        e.get("type").and_then(|v| v.as_str()) == Some("FORBIDDEN")
                            || e.get("message").and_then(|v| v.as_str()).is_some_and(|m| {
                                m.contains("Resource not accessible by personal access token")
                            })
                    });
                    if include_ci && permission_only {
                        self.gate.lock().await.ci_disabled = true;
                        continue;
                    }
                    let mut messages = Vec::new();
                    for error in errors {
                        let message = error
                            .get("message")
                            .and_then(|v| v.as_str())
                            .unwrap_or("Unknown GraphQL error");
                        if !messages.contains(&message) {
                            messages.push(message);
                        }
                    }
                    return Err(AppError::GraphQl(messages.join("; ")));
                }
                let response: GraphQlResponse = serde_json::from_value(value).map_err(|_| {
                    AppError::InvalidResponse(
                        "Unexpected PR search JSON structure; endpoint=/graphql".into(),
                    )
                })?;
                debug_assert!(
                    response
                        .errors
                        .as_ref()
                        .is_none_or(|errors| errors.is_empty())
                );
                let data = response
                    .data
                    .ok_or_else(|| AppError::GraphQl("No data in response".into()))?;
                if !include_ci {
                    self.gate.lock().await.ci_warning = Some(
                        "CI unavailable: token cannot access CI fields. PR list continues without CI; check repository access and Contents/Commit statuses/Checks read permissions, then restart prtop to retry CI.".into()
                    );
                }
                break data;
            };

            let search = data.search;
            all_nodes.extend(search.nodes);

            if !search.page_info.has_next_page {
                break;
            }
            cursor = search.page_info.end_cursor;
        }

        Ok(all_nodes)
    }

    /// dismiss 済み mentioned PR の直近 issue コメントを一括取得する。
    /// エイリアス付きの単一 GraphQL クエリで全 PR 分をまとめて引き、
    /// エイリアスキーが動的なためレスポンスは serde_json::Value で手動パースする。
    /// レビューコメント(コードコメント)内の再メンション検出は v1 スコープ外。
    pub async fn fetch_recent_comments(
        &self,
        prs: &[PrId],
    ) -> Result<HashMap<PrId, Vec<RecentComment>>, AppError> {
        if prs.is_empty() {
            return Ok(HashMap::new());
        }

        let url = format!("{}/graphql", self.api_base);
        let body = json!({ "query": query::recent_comments_query(prs) });

        let request = self.client.post(&url).bearer_auth(&self.token).json(&body);

        let value: serde_json::Value = self
            .send_json(request, false)
            .await?
            .ok_or_else(|| AppError::GraphQl("Missing response".into()))?;

        if let Some(errors) = value.get("errors").and_then(|e| e.as_array()) {
            let msgs: Vec<String> = errors
                .iter()
                .map(|e| {
                    e.get("message")
                        .and_then(|m| m.as_str())
                        .unwrap_or("unknown error")
                        .to_string()
                })
                .collect();
            return Err(AppError::GraphQl(msgs.join("; ")));
        }

        let data = value
            .get("data")
            .ok_or_else(|| AppError::GraphQl("No data in response".to_string()))?;

        let mut result = HashMap::with_capacity(prs.len());
        for (i, id) in prs.iter().enumerate() {
            let nodes = data
                .get(format!("pr{i}"))
                .and_then(|r| r.get("pullRequest"))
                .and_then(|p| p.get("comments"))
                .and_then(|c| c.get("nodes"))
                .and_then(|n| n.as_array());
            let mut comments = Vec::new();
            if let Some(nodes) = nodes {
                for node in nodes {
                    let body_text = node
                        .get("bodyText")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    let created_at = node
                        .get("createdAt")
                        .and_then(|v| v.as_str())
                        .and_then(|s| s.parse::<DateTime<Utc>>().ok())
                        .unwrap_or_default();
                    let author_login = node
                        .get("author")
                        .and_then(|a| a.get("login"))
                        .and_then(|v| v.as_str())
                        .map(str::to_string);
                    comments.push(RecentComment {
                        body_text,
                        created_at,
                        author_login,
                    });
                }
            }
            result.insert(id.clone(), comments);
        }
        Ok(result)
    }
}

fn rate_limit_message(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("rate limit")
        || message.contains("rate_limit")
        || message.contains("abuse detection")
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
    async fn server_errors_hide_bodies_and_pause_requests_without_disabling_token() {
        for status in [500, 502, 503, 504] {
            for retry in [None, Some("12")] {
                let server = MockServer::start().await;
                let mut response = ResponseTemplate::new(status)
                    .insert_header("content-type", "text/html")
                    .set_body_string(
                        "<!DOCTYPE html><html><style>raw-secret-marker</style></html>",
                    );
                if let Some(seconds) = retry {
                    response = response.insert_header("retry-after", seconds);
                }
                Mock::given(method("POST"))
                    .respond_with(response)
                    .mount(&server)
                    .await;
                let client = GitHubClient::new_with_base_url("token".into(), server.uri());
                let error = client.search_prs("author:me", 1).await.unwrap_err();
                let AppError::ServiceUnavailable {
                    retry_after_secs, ..
                } = &error
                else {
                    panic!("{error}")
                };
                assert_eq!(*retry_after_secs, if retry.is_some() { 12 } else { 60 });
                let text = error.to_string();
                assert!(text.contains(&format!("HTTP {status}")));
                assert!(text.contains("endpoint=/graphql"));
                assert!(!text.contains("html") && !text.contains("raw-secret-marker"));
                assert!(!client.gate.lock().await.disabled);
                assert!(matches!(
                    client.clone().search_prs("author:me", 1).await,
                    Err(AppError::ServiceUnavailable { .. })
                ));
                assert_eq!(server.received_requests().await.unwrap().len(), 1);
            }
        }
    }

    #[tokio::test]
    async fn http_and_invalid_responses_never_display_raw_bodies() {
        for status in [200, 401, 403, 404, 422] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(
                    ResponseTemplate::new(status).set_body_string("<html>raw-secret-marker</html>"),
                )
                .mount(&server)
                .await;
            let client = GitHubClient::new_with_base_url("token".into(), server.uri());
            let error = client.search_prs("author:me", 1).await.unwrap_err();
            match status {
                200 => assert!(matches!(error, AppError::InvalidResponse(_))),
                401 | 403 => assert!(matches!(error, AppError::Auth(_))),
                _ => assert!(matches!(error, AppError::Http(_))),
            }
            let text = error.to_string();
            assert!(text.contains(&format!("HTTP {status}")));
            assert!(!text.contains("html") && !text.contains("raw-secret-marker"));
        }
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"data":"<html>raw-secret-marker</html>"})),
            )
            .mount(&server)
            .await;
        let client = GitHubClient::new_with_base_url("token".into(), server.uri());
        let error = client.search_prs("author:me", 1).await.unwrap_err();
        assert!(matches!(error, AppError::InvalidResponse(_)));
        assert!(!error.to_string().contains("raw-secret-marker"));
    }

    #[tokio::test]
    async fn forbidden_ci_retries_same_page_and_disables_ci_for_token_clones() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(|request: &wiremock::Request| {
                let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
                if body["variables"]["includeCI"] == true {
                    // Decode errors before data: a denied field can null a parent.
                    ResponseTemplate::new(200).set_body_json(json!({
                        "data": {"search": null},
                        "errors": [{"type":"FORBIDDEN", "message":"Resource not accessible by personal access token"}]
                    }))
                } else {
                    let mut response: serde_json::Value = serde_json::from_str(include_str!("../../tests/fixtures/search_response.json")).unwrap();
                    for node in response["data"]["search"]["nodes"].as_array_mut().unwrap() {
                        node.as_object_mut().unwrap().remove("commits");
                    }
                    ResponseTemplate::new(200).set_body_json(response)
                }
            }).mount(&server).await;
        let client = GitHubClient::new_with_base_url("token".into(), server.uri());
        let prs = client.search_prs("author:me", 1).await.unwrap();
        assert_eq!(prs.len(), 2);
        assert!(prs.iter().all(|pr| pr.ci_status().is_none()));
        assert!(
            client
                .ci_warning()
                .await
                .unwrap()
                .contains("CI unavailable")
        );
        assert!(
            client
                .clone()
                .search_prs("review-requested:me", 1)
                .await
                .is_ok()
        );
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 3);
        let bodies: Vec<serde_json::Value> = requests
            .iter()
            .map(|r| serde_json::from_slice(&r.body).unwrap())
            .collect();
        assert_eq!(
            bodies[0]["variables"]["query"],
            bodies[1]["variables"]["query"]
        );
        assert_eq!(
            bodies[0]["variables"]["after"],
            bodies[1]["variables"]["after"]
        );
        assert_eq!(bodies[2]["variables"]["includeCI"], false);
        assert!(
            bodies[0]["query"]
                .as_str()
                .unwrap()
                .contains("@include(if: $includeCI)")
        );
        // Another token retains its CI capability.
        let other = GitHubClient::new_with_base_url("other".into(), server.uri());
        assert!(!other.gate.lock().await.ci_disabled);
    }

    #[tokio::test]
    async fn base_permission_errors_remain_fatal_and_are_deduplicated() {
        let server = MockServer::start().await;
        Mock::given(method("POST")).respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": null,
            "errors": [
                {"type":"FORBIDDEN", "message":"Resource not accessible by personal access token"},
                {"type":"FORBIDDEN", "message":"Resource not accessible by personal access token"}
            ]
        }))).mount(&server).await;
        let client = GitHubClient::new_with_base_url("token".into(), server.uri());
        let error = client
            .search_prs("author:me", 4)
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(error.matches("Resource not accessible").count(), 1);
        assert!(client.ci_warning().await.is_none());
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
        assert!(client.search_prs("author:me", 4).await.is_err());
        assert_eq!(server.received_requests().await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn non_permission_errors_do_not_disable_ci_or_trigger_fallback() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": null, "errors": [{"message":"Something went wrong"}]
            })))
            .mount(&server)
            .await;
        let client = GitHubClient::new_with_base_url("token".into(), server.uri());
        assert!(client.search_prs("author:me", 4).await.is_err());
        assert!(!client.gate.lock().await.ci_disabled);
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
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
    async fn auth_failure_is_not_a_rate_limit_even_with_zero_remaining() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(ResponseTemplate::new(401).append_header("x-ratelimit-remaining", "0"))
            .mount(&server)
            .await;

        let client = GitHubClient::new_with_base_url("token".to_string(), server.uri());
        let result = client.search_prs("author:user", MAX_PAGES).await;
        assert!(matches!(result, Err(AppError::Auth(_))));
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
                retry_after_secs: 30,
                ..
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

    // Exercise core-resource cooldowns directly, without a production REST CI path.
    impl GitHubClient {
        async fn test_core_requests(
            &self,
            owner: &str,
            repo: &str,
            sha: &str,
        ) -> Result<Option<serde_json::Value>, AppError> {
            for suffix in ["status", "check-runs"] {
                let request = self
                    .client
                    .get(format!(
                        "{}/repos/{owner}/{repo}/commits/{sha}/{suffix}",
                        self.api_base
                    ))
                    .bearer_auth(&self.token);
                self.send_json::<serde_json::Value>(request, true).await?;
            }
            Ok(None)
        }
    }

    #[tokio::test]
    async fn rate_limits_stop_all_endpoints_on_clones_until_cooldown_expires() {
        for (status, body) in [
            (429, serde_json::json!({"message":"slow down"})),
            (403, serde_json::json!({"message":"secondary rate limit"})),
        ] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(status).set_body_json(body))
                .mount(&server)
                .await;
            let client = GitHubClient::new_with_base_url("token".into(), server.uri());
            assert!(matches!(
                client.search_prs("author:a", 1).await,
                Err(AppError::RateLimited { .. })
            ));
            let clone = client.clone();
            assert!(matches!(
                clone.test_core_requests("o", "r", "sha").await,
                Err(AppError::RateLimited { .. })
            ));
            assert!(matches!(
                clone
                    .fetch_review_summary(&PrId {
                        owner: "o".into(),
                        repo: "r".into(),
                        number: 1
                    })
                    .await,
                Err(AppError::RateLimited { .. })
            ));
            assert_eq!(server.received_requests().await.unwrap().len(), 1);
            client.gate.lock().await.limits.lock().unwrap().clear();
            let _ = clone.search_prs("author:a", 1).await;
            assert_eq!(server.received_requests().await.unwrap().len(), 2);
        }
    }

    #[tokio::test]
    async fn rest_primary_limit_honors_reset_but_allows_graphql() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(403)
                    .append_header("x-ratelimit-remaining", "0")
                    .append_header(
                        "x-ratelimit-reset",
                        (Utc::now().timestamp() + 3600).to_string(),
                    )
                    .set_body_json(json!({"message":"API rate limit exceeded"})),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(empty_response()))
            .mount(&server)
            .await;
        let client = GitHubClient::new_with_base_url("token".into(), server.uri());
        match client.test_core_requests("o", "r", "sha").await {
            Err(AppError::RateLimited {
                retry_after_secs, ..
            }) => assert!(retry_after_secs > 3500),
            _ => panic!("expected primary limit"),
        }
        assert!(client.search_prs("author:a", 1).await.is_ok());
        assert!(matches!(
            client.test_core_requests("o", "r", "sha").await,
            Err(AppError::RateLimited { .. })
        ));
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn queued_requests_stop_after_first_limit() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(429))
            .mount(&server)
            .await;
        let client = GitHubClient::new_with_base_url("token".into(), server.uri());
        let (a, b, c) = tokio::join!(
            client.search_prs("a", 1),
            client.search_prs("b", 1),
            client.search_prs("c", 1)
        );
        assert!(a.is_err() && b.is_err() && c.is_err());
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn graphql_primary_limit_does_not_block_rest_and_keeps_diagnostics() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .append_header("x-ratelimit-resource", "graphql")
                    .append_header("x-ratelimit-remaining", "0")
                    .append_header(
                        "x-ratelimit-reset",
                        (Utc::now().timestamp() + 1986).to_string(),
                    )
                    .set_body_json(
                        json!({"errors":[{"type":"RATE_LIMITED","message":"quota exceeded"}]}),
                    ),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let client = GitHubClient::new_with_base_url("secret-token".into(), server.uri());
        let error = client.search_prs("author:a", 1).await.unwrap_err();
        let message = error.to_string();
        assert!(message.contains("primary limit"));
        assert!(message.contains("HTTP 200"));
        assert!(message.contains("resource=graphql"));
        assert!(message.contains("remaining=0"));
        assert!(message.contains("reset=20"));
        assert!(!message.contains("secret-token"));
        assert!(matches!(
            client
                .fetch_review_summary(&PrId {
                    owner: "o".into(),
                    repo: "r".into(),
                    number: 1
                })
                .await,
            Err(AppError::RateLimited { .. })
        ));
        assert_eq!(
            client.test_core_requests("o", "r", "sha").await.unwrap(),
            None
        );
        assert_eq!(server.received_requests().await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn service_retry_after_is_not_rate_limit_or_auth_failure() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(503)
                    .append_header("retry-after", "120")
                    .set_body_string("backend unavailable"),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let client = GitHubClient::new_with_base_url("token".into(), server.uri());
        let error = client.search_prs("a", 1).await.unwrap_err();
        assert!(matches!(
            error,
            AppError::ServiceUnavailable {
                retry_after_secs: 120,
                ..
            }
        ));
        assert!(error.to_string().contains("HTTP 503"));
        assert!(!error.to_string().contains("rate limited"));
        assert!(matches!(
            client.search_prs("a", 1).await,
            Err(AppError::ServiceUnavailable { .. })
        ));
        assert!(client.test_core_requests("o", "r", "sha").await.is_ok());
        assert_eq!(server.received_requests().await.unwrap().len(), 3);
        assert!(!client.gate.lock().await.disabled);
    }

    #[tokio::test]
    async fn successful_last_quota_response_is_delivered_before_resource_cooldown() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .append_header("x-ratelimit-remaining", "0")
                    .set_body_json(empty_response()),
            )
            .mount(&server)
            .await;
        let client = GitHubClient::new_with_base_url("token".into(), server.uri());
        assert!(client.search_prs("a", 1).await.is_ok());
        assert!(matches!(
            client.search_prs("a", 1).await,
            Err(AppError::RateLimited { .. })
        ));
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn verified_same_user_tokens_share_limits_but_other_users_do_not() {
        let server = MockServer::start().await;
        for (token, viewer) in [("a", "user-1"), ("b", "user-1"), ("c", "user-2")] {
            let mut body = empty_response();
            body["data"]["viewer"] = json!({"id":viewer});
            Mock::given(method("POST"))
                .and(wiremock::matchers::header(
                    "authorization",
                    format!("Bearer {token}"),
                ))
                .respond_with(ResponseTemplate::new(200).set_body_json(body))
                .mount(&server)
                .await;
        }
        Mock::given(method("GET"))
            .and(wiremock::matchers::header("authorization", "Bearer a"))
            .respond_with(
                ResponseTemplate::new(403)
                    .append_header("x-ratelimit-remaining", "0")
                    .append_header("x-ratelimit-resource", "core")
                    .set_body_json(json!({"message":"API rate limit exceeded"})),
            )
            .mount(&server)
            .await;
        let a = GitHubClient::new_with_base_url("a".into(), server.uri());
        let b = GitHubClient::new_with_base_url("b".into(), server.uri());
        let c = GitHubClient::new_with_base_url("c".into(), server.uri());
        for client in [&a, &b, &c] {
            client.search_prs("a", 1).await.unwrap();
        }
        assert!(matches!(
            a.test_core_requests("o", "r", "sha").await,
            Err(AppError::RateLimited { .. })
        ));
        assert!(matches!(
            b.test_core_requests("o", "r", "sha").await,
            Err(AppError::RateLimited { .. })
        ));
        assert_eq!(server.received_requests().await.unwrap().len(), 4);
        // The independent user's REST calls reach the server (404 is allowed).
        assert!(c.test_core_requests("o", "r", "sha").await.is_ok());
        assert_eq!(server.received_requests().await.unwrap().len(), 6);
        assert!(b.search_prs("a", 1).await.is_ok());
    }
}
