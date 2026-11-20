use serde::Deserialize;
use serde_json::json;

use super::{
    client::GitHubClient,
    types::{CommentNode, ReviewThreadNode},
};
use crate::{error::AppError, types::PrId};

#[derive(Clone, Copy)]
pub(crate) enum CommentSource {
    Conversation,
    ReviewThread,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ActivityPage<T> {
    total_count: u64,
    nodes: Option<Vec<Option<T>>>,
    page_info: ActivityPageInfo,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ActivityPageInfo {
    has_previous_page: bool,
    start_cursor: Option<String>,
}

impl GitHubClient {
    /// Fetch the newest entries for a batch that did not fit in the PR search response.
    pub(crate) async fn fetch_comment_authors(
        &self,
        id: &PrId,
        source: CommentSource,
        total_count: u64,
        mut remaining: u64,
    ) -> Result<Vec<Option<String>>, AppError> {
        let (connection, selection) = match source {
            CommentSource::Conversation => ("comments", "author { login }"),
            CommentSource::ReviewThread => (
                "reviewThreads",
                "comments(first: 1) { nodes { author { login } } }",
            ),
        };
        let query = format!(
            "query($owner: String!, $name: String!, $number: Int!, $last: Int!, $before: String) {{
                repository(owner: $owner, name: $name) {{
                    pullRequest(number: $number) {{
                        activity: {connection}(last: $last, before: $before) {{
                            totalCount
                            pageInfo {{ hasPreviousPage startCursor }}
                            nodes {{ {selection} }}
                        }}
                    }}
                }}
            }}"
        );
        let url = format!("{}/graphql", self.api_base);
        let mut authors = Vec::new();
        let mut before: Option<String> = None;
        while remaining > 0 {
            let last = remaining.min(100);
            let request = self
                .client
                .post(&url)
                .bearer_auth(&self.token)
                .json(&json!({
                    "query": query,
                    "variables": {
                        "owner": id.owner,
                        "name": id.repo,
                        "number": id.number,
                        "last": last,
                        "before": before,
                    }
                }));
            let mut response: serde_json::Value = self
                .send_json(request, false)
                .await?
                .ok_or_else(|| AppError::GraphQl("Missing comment activity response".into()))?;
            if response
                .get("errors")
                .and_then(serde_json::Value::as_array)
                .is_some_and(|errors| !errors.is_empty())
            {
                return Err(AppError::GraphQl(
                    "GitHub could not read PR comment activity".into(),
                ));
            }
            let activity = response
                .pointer_mut("/data/repository/pullRequest/activity")
                .ok_or_else(|| AppError::GraphQl("Missing PR comment activity".into()))?
                .take();
            let (count, page_info, page_authors) = match source {
                CommentSource::Conversation => parse_page(activity, CommentNode::author_login)?,
                CommentSource::ReviewThread => {
                    parse_page(activity, ReviewThreadNode::author_login)?
                }
            };
            if count != total_count {
                return Err(AppError::CommentActivityChanged);
            }
            if page_authors.len() as u64 != last {
                return Err(AppError::InvalidResponse(
                    "Incomplete PR comment activity page".into(),
                ));
            }
            authors.extend(page_authors);
            remaining -= last;
            if remaining > 0 {
                before = page_info.start_cursor.filter(|cursor| {
                    page_info.has_previous_page && Some(cursor) != before.as_ref()
                });
                if before.is_none() {
                    return Err(AppError::InvalidResponse(
                        "Missing PR comment pagination cursor".into(),
                    ));
                }
            }
        }
        Ok(authors)
    }
}

fn parse_page<T: serde::de::DeserializeOwned>(
    activity: serde_json::Value,
    author_login: impl Fn(&T) -> Option<String>,
) -> Result<(u64, ActivityPageInfo, Vec<Option<String>>), AppError> {
    let page: ActivityPage<T> = serde_json::from_value(activity)
        .map_err(|_| AppError::InvalidResponse("Unexpected PR comment activity response".into()))?;
    let authors = page
        .nodes
        .unwrap_or_default()
        .iter()
        .rev()
        .map(|node| node.as_ref().and_then(&author_login))
        .collect();
    Ok((page.total_count, page.page_info, authors))
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_partial_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn comment_batch_follows_the_backward_cursor() {
        let server = MockServer::start().await;
        let first_page = vec![json!({"author": {"login": "self"}}); 100];
        for (before, nodes, cursor) in [
            (None, first_page, Some("older")),
            (
                Some("older"),
                vec![json!({"author": {"login": "other"}})],
                None,
            ),
        ] {
            Mock::given(method("POST"))
                .and(path("/graphql"))
                .and(body_partial_json(json!({
                    "variables": {"before": before, "last": if before.is_none() { 100 } else { 1 }}
                })))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "data": {"repository": {"pullRequest": {"activity": {
                        "totalCount": 150,
                        "nodes": nodes,
                        "pageInfo": {"hasPreviousPage": cursor.is_some(), "startCursor": cursor}
                    }}}}
                })))
                .expect(1)
                .mount(&server)
                .await;
        }
        let client = GitHubClient::new_with_base_url("token".into(), server.uri());
        let id = PrId {
            owner: "org".into(),
            repo: "repo".into(),
            number: 1,
        };
        let authors = client
            .fetch_comment_authors(&id, CommentSource::Conversation, 150, 101)
            .await
            .unwrap();
        assert_eq!(authors.len(), 101);
        assert!(
            authors[..100]
                .iter()
                .all(|author| author.as_deref() == Some("self"))
        );
        assert_eq!(authors[100].as_deref(), Some("other"));
    }
}
