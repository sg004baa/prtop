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
