use serde::Deserialize;
use serde_json::json;

use super::client::{GitHubClient, ensure_graphql_http_success};
use super::types::{ActorNode, GraphQlError, PageInfo};
use crate::error::AppError;
use crate::review::{ReviewSummary, SubmittedReview, summarize_reviews};
use crate::types::PrId;

const QUERY: &str = r#"
query($owner: String!, $repo: String!, $number: Int!, $reviewsAfter: String, $requestsAfter: String) {
  repository(owner: $owner, name: $repo) {
    pullRequest(number: $number) {
      headRefOid updatedAt state isDraft author { login }
      reviews(first: 100, after: $reviewsAfter) {
        pageInfo { hasNextPage endCursor }
        nodes { state submittedAt author { login } }
      }
      reviewRequests(first: 100, after: $requestsAfter) {
        pageInfo { hasNextPage endCursor }
        nodes { requestedReviewer {
          __typename
          ... on User { login }
          ... on Bot { login }
          ... on EnterpriseTeam { id }
          ... on Mannequin { login }
          ... on Team { id }
        } }
      }
    }
  }
}
"#;

#[derive(Deserialize)]
struct Response {
    data: Option<Data>,
    errors: Option<Vec<GraphQlError>>,
}
#[derive(Deserialize)]
struct Data {
    repository: Option<Repository>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Repository {
    pull_request: Option<ReviewPr>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReviewPr {
    head_ref_oid: String,
    updated_at: String,
    state: String,
    is_draft: bool,
    author: Option<ActorNode>,
    reviews: Connection<Review>,
    review_requests: Connection<Request>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Connection<T> {
    page_info: PageInfo,
    nodes: Vec<T>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Review {
    state: String,
    submitted_at: Option<String>,
    author: Option<ActorNode>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Request {
    requested_reviewer: Option<Reviewer>,
}
#[derive(Deserialize)]
struct Reviewer {
    #[serde(rename = "__typename")]
    kind: String,
    login: Option<String>,
    id: Option<String>,
}

pub struct ReviewSnapshot {
    pub head_sha: String,
    pub open: bool,
    pub draft: bool,
    pub summary: ReviewSummary,
}

fn invalid(message: &str) -> AppError {
    AppError::GraphQl(format!("Incomplete review snapshot: {message}"))
}

fn advance(page: &PageInfo, cursor: &mut Option<String>) -> Result<bool, AppError> {
    if !page.has_next_page {
        return Ok(true);
    }
    let next = page
        .end_cursor
        .as_ref()
        .filter(|c| !c.is_empty())
        .ok_or_else(|| invalid("missing page cursor"))?;
    if cursor.as_ref() == Some(next) {
        return Err(invalid("repeated page cursor"));
    }
    *cursor = Some(next.clone());
    Ok(false)
}

impl GitHubClient {
    /// Fetch every page of both connections. Never publish a partial count.
    pub async fn fetch_review_summary(&self, id: &PrId) -> Result<ReviewSnapshot, AppError> {
        let mut reviews_cursor = None;
        let mut requests_cursor = None;
        let mut reviews_done = false;
        let mut requests_done = false;
        let mut reviews = Vec::new();
        let mut requested = Vec::new();
        let mut teams = Vec::new();
        let mut version = None;
        // Bound pathological responses, but fail closed instead of using truncated data.
        for _ in 0..100 {
            let response = self
                .client
                .post(format!("{}/graphql", self.api_base))
                .timeout(std::time::Duration::from_secs(30))
                .bearer_auth(&self.token)
                .json(&json!({"query": QUERY, "variables": {
                    "owner": id.owner, "repo": id.repo, "number": id.number,
                    "reviewsAfter": reviews_cursor, "requestsAfter": requests_cursor
                }}))
                .send()
                .await?;
            let response: Response = ensure_graphql_http_success(response).await?.json().await?;
            if let Some(errors) = response.errors
                && !errors.is_empty()
            {
                return Err(AppError::GraphQl(
                    errors
                        .into_iter()
                        .map(|e| e.message)
                        .collect::<Vec<_>>()
                        .join("; "),
                ));
            }
            let pr = response
                .data
                .and_then(|d| d.repository)
                .and_then(|r| r.pull_request)
                .ok_or_else(|| invalid("missing pull request"))?;
            let author = pr
                .author
                .as_ref()
                .ok_or_else(|| invalid("missing author"))?;
            let current_version = (
                pr.head_ref_oid.clone(),
                pr.updated_at,
                pr.state.clone(),
                pr.is_draft,
            );
            if version.as_ref().is_some_and(|v| v != &current_version) {
                return Err(invalid("pull request changed while paging"));
            }
            version = Some(current_version);
            if !reviews_done {
                for review in pr.reviews.nodes {
                    if review.state == "PENDING" {
                        continue;
                    }
                    if !matches!(
                        review.state.as_str(),
                        "APPROVED" | "CHANGES_REQUESTED" | "COMMENTED" | "DISMISSED"
                    ) {
                        return Err(invalid("unknown review state"));
                    }
                    reviews.push(SubmittedReview {
                        login: review
                            .author
                            .ok_or_else(|| invalid("missing reviewer"))?
                            .login,
                        state: review.state,
                        submitted_at: review
                            .submitted_at
                            .ok_or_else(|| invalid("missing submission time"))?,
                    });
                }
                reviews_done = advance(&pr.reviews.page_info, &mut reviews_cursor)?;
            }
            if !requests_done {
                for request in pr.review_requests.nodes {
                    let reviewer = request
                        .requested_reviewer
                        .ok_or_else(|| invalid("missing requested reviewer"))?;
                    match reviewer.kind.as_str() {
                        "Team" | "EnterpriseTeam" => {
                            teams.push(reviewer.id.ok_or_else(|| invalid("missing team id"))?)
                        }
                        "User" | "Bot" | "Mannequin" => {
                            requested.push(reviewer.login.ok_or_else(|| invalid("missing login"))?)
                        }
                        _ => return Err(invalid("unknown requested reviewer type")),
                    }
                }
                requests_done = advance(&pr.review_requests.page_info, &mut requests_cursor)?;
            }
            if reviews_done && requests_done {
                reviews.sort_by(|a, b| a.submitted_at.cmp(&b.submitted_at));
                return Ok(ReviewSnapshot {
                    head_sha: pr.head_ref_oid,
                    open: pr.state == "OPEN",
                    draft: pr.is_draft,
                    summary: summarize_reviews(&author.login, &reviews, &requested, &teams),
                });
            }
        }
        Err(invalid("pagination limit reached"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_partial_json, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn page(reviews: serde_json::Value, requests: serde_json::Value) -> serde_json::Value {
        json!({"data": {"repository": {"pullRequest": {
            "headRefOid": "sha", "updatedAt": "2026-10-22T00:00:00Z",
            "state": "OPEN", "isDraft": false, "author": {"login": "author"},
            "reviews": {"nodes": reviews, "pageInfo": {"hasNextPage": false, "endCursor": null}},
            "reviewRequests": {"nodes": requests, "pageInfo": {"hasNextPage": false, "endCursor": null}}
        }}}})
    }
    fn review(login: &str, state: &str) -> serde_json::Value {
        json!({"author": {"login": login}, "state": state, "submittedAt": "2026-10-21T00:00:00Z"})
    }
    fn id() -> PrId {
        PrId {
            owner: "org".into(),
            repo: "repo".into(),
            number: 1,
        }
    }

    #[tokio::test]
    async fn pages_both_connections_without_counting_completed_pages_twice() {
        let server = MockServer::start().await;
        let mut first = page(json!([review("alice", "APPROVED")]), json!([]));
        first["data"]["repository"]["pullRequest"]["reviews"]["pageInfo"] =
            json!({"hasNextPage": true, "endCursor": "r1"});
        first["data"]["repository"]["pullRequest"]["reviewRequests"]["pageInfo"] =
            json!({"hasNextPage": true, "endCursor": "q1"});
        let mut second = page(
            json!([review("bob", "APPROVED")]),
            json!([
                {"requestedReviewer": {"__typename": "User", "login": "carol"}}
            ]),
        );
        second["data"]["repository"]["pullRequest"]["reviewRequests"]["pageInfo"] =
            json!({"hasNextPage": true, "endCursor": "q2"});
        // Reviews are complete; this repeated page must not duplicate bob or reset alice.
        let third = page(
            json!([review("bob", "APPROVED")]),
            json!([
                {"requestedReviewer": {"__typename": "Team", "id": "team1"}}
            ]),
        );
        for (r, q, body) in [
            (None, None, first),
            (Some("r1"), Some("q1"), second),
            (Some("r1"), Some("q2"), third),
        ] {
            Mock::given(method("POST"))
                .and(body_partial_json(
                    json!({"variables": {"reviewsAfter": r, "requestsAfter": q}}),
                ))
                .respond_with(ResponseTemplate::new(200).set_body_json(body))
                .expect(1)
                .mount(&server)
                .await;
        }
        let client = GitHubClient::new_with_base_url("token".into(), server.uri());
        let snapshot = client.fetch_review_summary(&id()).await.unwrap();
        assert_eq!(
            snapshot.summary,
            ReviewSummary {
                approved: 2,
                total: 3,
                changes_requested: false,
                pending_teams: 1
            }
        );
        assert!(!snapshot.summary.all_approved());
    }

    #[tokio::test]
    async fn later_page_errors_and_concurrent_changes_discard_partial_approval() {
        for mode in ["graphql", "http", "changed", "cursor"] {
            let server = MockServer::start().await;
            let mut first = page(json!([review("alice", "APPROVED")]), json!([]));
            first["data"]["repository"]["pullRequest"]["reviews"]["pageInfo"] =
                json!({"hasNextPage": true, "endCursor": "r1"});
            Mock::given(body_partial_json(
                json!({"variables": {"reviewsAfter": null}}),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(first))
            .mount(&server)
            .await;
            let mut second = page(json!([]), json!([]));
            match mode {
                "graphql" => second["errors"] = json!([{"message": "permission denied"}]),
                "changed" => {
                    second["data"]["repository"]["pullRequest"]["updatedAt"] =
                        "2026-10-22T00:01:00Z".into()
                }
                "cursor" => {
                    second["data"]["repository"]["pullRequest"]["reviews"]["pageInfo"] =
                        json!({"hasNextPage": true, "endCursor": "r1"})
                }
                _ => {}
            }
            Mock::given(body_partial_json(
                json!({"variables": {"reviewsAfter": "r1"}}),
            ))
            .respond_with(
                ResponseTemplate::new(if mode == "http" { 403 } else { 200 }).set_body_json(second),
            )
            .mount(&server)
            .await;
            let client = GitHubClient::new_with_base_url("token".into(), server.uri());
            assert!(client.fetch_review_summary(&id()).await.is_err(), "{mode}");
        }
    }

    #[tokio::test]
    async fn incomplete_or_unknown_review_data_is_not_approved() {
        for bad in [
            json!({"state": "APPROVED", "submittedAt": "2026-01-01", "author": null}),
            json!({"state": "FUTURE", "submittedAt": "2026-01-01", "author": {"login": "alice"}}),
        ] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(page(json!([bad]), json!([]))),
                )
                .mount(&server)
                .await;
            let client = GitHubClient::new_with_base_url("token".into(), server.uri());
            assert!(client.fetch_review_summary(&id()).await.is_err());
        }
    }

    #[tokio::test]
    async fn bot_and_enterprise_team_requests_block_completion() {
        let server = MockServer::start().await;
        let body = page(
            json!([review("alice", "APPROVED")]),
            json!([
                {"requestedReviewer": {"__typename": "Bot", "login": "review-bot"}},
                {"requestedReviewer": {"__typename": "EnterpriseTeam", "id": "enterprise-team"}}
            ]),
        );
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;
        let client = GitHubClient::new_with_base_url("token".into(), server.uri());
        let summary = client.fetch_review_summary(&id()).await.unwrap().summary;
        assert_eq!(summary.approved, 1);
        assert_eq!(summary.total, 2);
        assert_eq!(summary.pending_teams, 1);
        assert!(!summary.all_approved());
    }
}
