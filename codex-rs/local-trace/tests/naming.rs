use codex_local_trace::naming::TraceNamer;
use codex_local_trace::naming::internal_label;
use codex_local_trace::naming::sanitize_label;
use codex_local_trace::naming::tool_label;
use codex_local_trace::naming::turn_slug;

#[test]
fn naming_uses_zero_padded_session_local_sequence() {
    let mut namer = TraceNamer::new();

    assert_eq!(
        namer.next_with_timestamp("20260520-154012", "session"),
        "20260520-154012-0001-session"
    );
    assert_eq!(
        namer.next_with_timestamp("20260520-154013", "request"),
        "20260520-154013-0002-request"
    );
}

#[test]
fn naming_turn_slug_uses_first_four_sanitized_prompt_words() {
    assert_eq!(
        turn_slug("Review the spec: personal/codex tracing!"),
        "review-the-spec-personalcodex"
    );
    assert_eq!(turn_slug("!!!"), "turn");
}

#[test]
fn naming_labels_are_sanitized_with_fallbacks() {
    assert_eq!(
        sanitize_label("shell.exec request", "fallback"),
        "shell.exec-request"
    );
    assert_eq!(sanitize_label("////", "fallback"), "fallback");
    assert_eq!(internal_label(Some("Compaction call")), "compaction-call");
    assert_eq!(internal_label(None), "internal");
    assert!(tool_label(&"x".repeat(80)).len() <= 64);
}

#[test]
fn naming_caps_long_user_controlled_labels() {
    assert!(turn_slug(&"word ".repeat(200)).len() <= 96);
    assert!(internal_label(Some(&"x".repeat(200))).len() <= 96);
    assert!(sanitize_label(&"x".repeat(200), "fallback").len() <= 96);
}
