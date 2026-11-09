use prtop::github::types::GraphQlResponse;

#[test]
fn deserialize_search_response() {
    let json = include_str!("fixtures/search_response.json");
    let response: GraphQlResponse = serde_json::from_str(json).unwrap();

    let data = response.data.unwrap();
    assert!(response.errors.is_none());

    let search = data.search;
    assert_eq!(search.issue_count, 2);
    assert!(!search.page_info.has_next_page);
    assert_eq!(
        search.page_info.end_cursor,
        Some("Y3Vyc29yOjI=".to_string())
    );
    assert_eq!(search.nodes.len(), 2);

    let first = &search.nodes[0];
    assert_eq!(first.number, 1234);
    assert_eq!(first.ci_status(), Some(prtop::types::CiStatus::Success));
    assert_eq!(first.title, "Fix rendering bug");
    assert_eq!(first.state, "OPEN");
    assert!(!first.is_draft);
    assert_eq!(first.review_decision, Some("APPROVED".to_string()));
    assert_eq!(first.author.as_ref().unwrap().login, "testuser");
    assert_eq!(first.repository.name, "ratatui");
    assert_eq!(first.repository.owner.login, "ratatui");

    let second = &search.nodes[1];
    assert_eq!(second.number, 5678);
    assert_eq!(second.ci_status(), None);
    assert!(second.is_draft);
    assert_eq!(second.review_decision, None);
}

#[test]
fn deserialize_error_response() {
    let json = r#"{
        "data": null,
        "errors": [
            {"message": "Something went wrong"}
        ]
    }"#;
    let response: GraphQlResponse = serde_json::from_str(json).unwrap();
    assert!(response.data.is_none());
    let errors = response.errors.unwrap();
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].message, "Something went wrong");
}

#[test]
fn null_commit_nodes_do_not_discard_other_prs() {
    let original: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/search_response.json")).unwrap();
    for nodes in [
        serde_json::Value::Null,
        serde_json::json!([null]),
        serde_json::json!([]),
    ] {
        let mut value = original.clone();
        value["data"]["search"]["nodes"][1]["commits"]["nodes"] = nodes;
        let response: GraphQlResponse = serde_json::from_value(value).unwrap();
        let prs = response.data.unwrap().search.nodes;
        assert_eq!(prs.len(), 2);
        assert_eq!(prs[0].ci_status(), Some(prtop::types::CiStatus::Success));
        assert_eq!(prs[1].ci_status(), None);
    }
    // Never use an earlier commit's successful CI when the last node is null.
    let mut value = original;
    value["data"]["search"]["nodes"][0]["commits"]["nodes"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::Value::Null);
    let response: GraphQlResponse = serde_json::from_value(value).unwrap();
    assert_eq!(response.data.unwrap().search.nodes[0].ci_status(), None);
}
