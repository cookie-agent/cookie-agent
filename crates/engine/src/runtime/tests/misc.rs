use super::support::*;

#[test]
fn scripted_matchers_ignore_only_trailing_subagent_notifications() {
    fn request(messages: serde_json::Value) -> Vec<u8> {
        let body = serde_json::json!({"messages": messages}).to_string();
        format!(
            "POST /v1 HTTP/1.1\r\ncontent-length: {}\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    let post_tool = request(serde_json::json!([
        {"role":"user","content":"delegate"},
        {"role":"assistant","content":"","tool_calls":[]},
        {"role":"tool","content":"admitted"},
        {"role":"user","content":"<subagent_notification>{}</subagent_notification>"}
    ]));
    assert!(MatchedScriptedResponse::last_message_role("tool", String::new()).matches(&post_tool));
    assert!(!scripted_is_auxiliary_subagent_notification(&post_tool));

    let continuation = request(serde_json::json!([
        {"role":"user","content":"delegate"},
        {"role":"assistant","content":"complete"},
        {"role":"user","content":"<subagent_notification>{}</subagent_notification>"}
    ]));
    assert!(scripted_is_auxiliary_subagent_notification(&continuation));

    for semantic in [
        "producer progress body",
        "Continue working toward the original goal.",
    ] {
        let semantic_request = request(serde_json::json!([
            {"role":"assistant","content":"complete"},
            {"role":"user","content":semantic}
        ]));
        assert!(!scripted_is_auxiliary_subagent_notification(
            &semantic_request
        ));
        assert!(
            MatchedScriptedResponse::last_message_contains(semantic, String::new())
                .matches(&semantic_request)
        );
    }
}
