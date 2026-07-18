pub const SEARCH_PRS_QUERY: &str = r#"
query($query: String!, $first: Int!, $after: String) {
  search(query: $query, type: ISSUE, first: $first, after: $after) {
    issueCount
    pageInfo {
      hasNextPage
      endCursor
    }
    nodes {
      ... on PullRequest {
        number
        title
        url
        state
        isDraft
        createdAt
        updatedAt
        reviewDecision
        headRefOid
        author {
          login
        }
        repository {
          name
          owner {
            login
          }
        }
        comments(last: 1) {
          totalCount
          nodes {
            author {
              login
            }
          }
        }
        reviewThreads {
          totalCount
        }
      }
    }
  }
}
"#;

pub fn author_search_query(username: &str) -> String {
    format!("is:pr is:open author:{username}")
}

pub fn author_closed_search_query(username: &str) -> String {
    format!("is:pr -is:open author:{username}")
}

pub fn review_requested_search_query(username: &str) -> String {
    format!("is:pr is:open review-requested:{username}")
}

pub fn review_requested_closed_search_query(username: &str) -> String {
    format!("is:pr -is:open review-requested:{username}")
}

pub fn mentions_search_query(username: &str) -> String {
    format!("is:pr is:open mentions:{username}")
}

pub fn mentions_closed_search_query(username: &str) -> String {
    format!("is:pr -is:open mentions:{username}")
}

/// dismiss 済み mentioned PR の直近コメントを一括取得するクエリを組み立てる。
/// PR ごとに `pr{i}` エイリアスを振るため、レスポンスのキーは動的になる
/// (パース側は serde ではなく serde_json::Value で処理する)。
pub fn recent_comments_query(prs: &[crate::types::PrId]) -> String {
    let mut q = String::from("query {");
    for (i, id) in prs.iter().enumerate() {
        q.push_str(&format!(
            " pr{i}: repository(owner: \"{}\", name: \"{}\") {{ pullRequest(number: {}) {{ comments(last: 20) {{ nodes {{ bodyText createdAt author {{ login }} }} }} }} }}",
            id.owner, id.repo, id.number
        ));
    }
    q.push_str(" }");
    q
}
