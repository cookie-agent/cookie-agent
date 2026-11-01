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

    // A report's wake carries the engine's announcement of instructions that
    // changed since the previous run, before or after the report.
    let announcement =
        "[cookie: instructions changed]\n<instructions_changed>\n</instructions_changed>";
    for wake in [
        [
            serde_json::json!({"role":"user","content":"<subagent_report handle=\"h\" status=\"completed\">done</subagent_report>"}),
            serde_json::json!({"role":"user","content":announcement}),
        ],
        [
            serde_json::json!({"role":"user","content":announcement}),
            serde_json::json!({"role":"user","content":"<subagent_report handle=\"h\" status=\"completed\">done</subagent_report>"}),
        ],
    ] {
        let wake = request(serde_json::json!([
            {"role":"user","content":"delegate"},
            {"role":"assistant","content":"complete"},
            wake[0],
            wake[1]
        ]));
        assert!(scripted_is_auxiliary_subagent_notification(&wake));
    }
    // An announcement alone, or with a user's own message, is no wake.
    for messages in [
        serde_json::json!([
            {"role":"assistant","content":"complete"},
            {"role":"user","content":announcement}
        ]),
        serde_json::json!([
            {"role":"assistant","content":"complete"},
            {"role":"user","content":announcement},
            {"role":"user","content":"next task"}
        ]),
    ] {
        assert!(!scripted_is_auxiliary_subagent_notification(&request(
            messages
        )));
    }

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
