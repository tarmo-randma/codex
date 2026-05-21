use codex_local_trace::TraceConfig;

#[test]
fn config_enables_only_explicit_truthy_values() {
    for value in ["1", "true"] {
        let config = TraceConfig::from_env_map([("CODEX_TRACE", value)]);
        assert!(config.enabled(), "{value} should enable tracing");
        assert!(config.model_enabled());
        assert!(config.tools_enabled());
        assert!(config.usage_enabled());
        assert!(config.subagents_enabled());
        assert!(config.config_enabled());
    }
}

#[test]
fn config_disables_all_other_values() {
    for value in ["", "0", "false", "yes", "on", "TRUE", "random"] {
        let config = TraceConfig::from_env_map([("CODEX_TRACE", value)]);
        assert!(!config.enabled(), "{value:?} should disable tracing");
        assert!(!config.model_enabled());
        assert!(!config.tools_enabled());
        assert!(!config.usage_enabled());
        assert!(!config.subagents_enabled());
        assert!(!config.config_enabled());
    }

    let config = TraceConfig::from_env_map(Vec::<(&str, &str)>::new());
    assert!(!config.enabled());
}

#[test]
fn config_reads_optional_trace_dir_without_mutating_env() {
    let config =
        TraceConfig::from_env_map([("CODEX_TRACE", "1"), ("CODEX_TRACE_DIR", "/tmp/my-traces")]);

    assert_eq!(
        config.trace_dir().unwrap(),
        &std::path::PathBuf::from("/tmp/my-traces")
    );
}
