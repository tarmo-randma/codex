use codex_local_trace::TraceConfig;
use codex_local_trace::TraceRecorder;
use codex_local_trace::schema::OwnerMetadata;
use codex_local_trace::schema::OwnerStatus;
use codex_local_trace::schema::RequestMetadata;
use codex_local_trace::schema::RequestStatus;
use codex_local_trace::schema::RequestUpdate;
use codex_local_trace::schema::SessionMetadata;
use codex_local_trace::schema::ToolCallMetadata;
use serde_json::json;

mod support;

struct FailingSerialize;

impl serde::Serialize for FailingSerialize {
    fn serialize<S>(&self, _serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        Err(serde::ser::Error::custom(
            "intentional serialization failure",
        ))
    }
}

#[test]
fn disabled_recorder_is_noop() {
    let recorder = TraceRecorder::disabled();

    recorder.record_config(&json!({"model": "gpt-test"}));
    assert!(
        recorder
            .start_turn("hello", OwnerMetadata::default())
            .is_none()
    );
    assert!(
        recorder
            .record_model_request(RequestMetadata::default(), None, &json!({"input": []}))
            .is_none()
    );
    assert!(recorder.session_path().is_none());
}

#[test]
fn request_full_context_sidecar_updates_metadata() -> Result<(), Box<dyn std::error::Error>> {
    let temp = support::TempDir::new()?;
    let config = TraceConfig::from_env_map([
        ("CODEX_TRACE", "1"),
        ("CODEX_TRACE_DIR", temp.path().to_str().unwrap()),
    ]);
    let recorder = TraceRecorder::start_session(
        config,
        SessionMetadata {
            workspace_cwd: Some(temp.path().to_path_buf()),
            cwd: Some(temp.path().to_path_buf()),
            ..Default::default()
        },
    );
    let session_path = recorder.session_path().expect("enabled session");
    let request = recorder
        .record_model_request(
            RequestMetadata::default(),
            None,
            &json!({"input": [{"text": "incremental"}]}),
        )
        .expect("request");

    recorder.record_request_full_context(
        &request,
        &json!({"input": [{"text": "full-1"}, {"text": "full-2"}]}),
    );

    let meta = serde_json::from_str::<serde_json::Value>(&std::fs::read_to_string(
        session_path.join(&request.path).with_file_name("meta.json"),
    )?)?;
    let full_context_path = meta["request_full_context_path"]
        .as_str()
        .expect("full context path");
    assert_eq!(
        full_context_path,
        format!(
            "{}/request.full_context.json",
            request.path.trim_end_matches("/request.json")
        )
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&std::fs::read_to_string(
            session_path.join(full_context_path),
        )?)?,
        json!({"input": [{"text": "full-1"}, {"text": "full-2"}]})
    );
    Ok(())
}

#[test]
fn finish_failed_request_updates_grouped_meta_sidecar() -> Result<(), Box<dyn std::error::Error>> {
    let temp = support::TempDir::new()?;
    let config = TraceConfig::from_env_map([
        ("CODEX_TRACE", "1"),
        ("CODEX_TRACE_DIR", temp.path().to_str().unwrap()),
    ]);
    let recorder = TraceRecorder::start_session(
        config,
        SessionMetadata {
            workspace_cwd: Some(temp.path().to_path_buf()),
            cwd: Some(temp.path().to_path_buf()),
            ..Default::default()
        },
    );
    let session_path = recorder.session_path().expect("enabled session");
    let request = recorder
        .record_model_request(
            RequestMetadata::default(),
            None,
            &json!({"input": [{"text": "stream"}]}),
        )
        .expect("request");

    recorder.finish_model_request(
        &request,
        RequestUpdate {
            status: Some(RequestStatus::Failed),
            error: Some("missing response.completed".to_string()),
            ..Default::default()
        },
    );

    let meta = serde_json::from_str::<serde_json::Value>(&std::fs::read_to_string(
        session_path.join(&request.path).with_file_name("meta.json"),
    )?)?;
    assert_eq!(meta["status"], json!("failed"));
    assert_eq!(meta["error"], json!("missing response.completed"));

    Ok(())
}

#[test]
fn explicit_owner_handles_keep_interleaved_requests_and_finish_safe()
-> Result<(), Box<dyn std::error::Error>> {
    let temp = support::TempDir::new()?;
    let config = TraceConfig::from_env_map([
        ("CODEX_TRACE", "1"),
        ("CODEX_TRACE_DIR", temp.path().to_str().unwrap()),
    ]);
    let recorder = TraceRecorder::start_session(
        config,
        SessionMetadata {
            workspace_cwd: Some(temp.path().to_path_buf()),
            cwd: Some(temp.path().to_path_buf()),
            ..Default::default()
        },
    );
    let session_path = recorder.session_path().expect("enabled session");

    let first = recorder
        .start_turn_scope("first prompt", OwnerMetadata::default())
        .expect("first turn");
    let second = recorder
        .start_turn_scope("second prompt", OwnerMetadata::default())
        .expect("second turn");

    let first_request = recorder
        .record_model_request_for_owner(
            &first,
            RequestMetadata::default(),
            None,
            &json!({"input": ["first"]}),
        )
        .expect("first request");
    let second_request = recorder
        .record_model_request_for_owner(
            &second,
            RequestMetadata::default(),
            None,
            &json!({"input": ["second"]}),
        )
        .expect("second request");

    recorder.finish_owner_scope(&second, OwnerStatus::Completed, None);
    recorder.finish_owner_scope(
        &first,
        OwnerStatus::Failed,
        Some("first failed".to_string()),
    );

    assert!(
        first_request
            .path
            .starts_with(&format!("{}/requests/", first.path))
    );
    assert!(
        second_request
            .path
            .starts_with(&format!("{}/requests/", second.path))
    );

    let first_record = serde_json::from_str::<serde_json::Value>(&std::fs::read_to_string(
        session_path.join(&first.path).join("turn.json"),
    )?)?;
    let second_record = serde_json::from_str::<serde_json::Value>(&std::fs::read_to_string(
        session_path.join(&second.path).join("turn.json"),
    )?)?;
    assert_eq!(first_record["status"], "failed");
    assert_eq!(first_record["error"], "first failed");
    assert_eq!(first_record["request_paths"][0], first_request.path);
    assert_eq!(second_record["status"], "completed");
    assert_eq!(second_record["request_paths"][0], second_request.path);

    Ok(())
}

#[test]
fn finishing_scoped_owner_restores_previous_active_owner_for_tools()
-> Result<(), Box<dyn std::error::Error>> {
    let temp = support::TempDir::new()?;
    let config = TraceConfig::from_env_map([
        ("CODEX_TRACE", "1"),
        ("CODEX_TRACE_DIR", temp.path().to_str().unwrap()),
    ]);
    let recorder = TraceRecorder::start_session(
        config,
        SessionMetadata {
            workspace_cwd: Some(temp.path().to_path_buf()),
            cwd: Some(temp.path().to_path_buf()),
            ..Default::default()
        },
    );
    let session_path = recorder.session_path().expect("enabled session");

    let turn = recorder
        .start_turn("active user prompt", OwnerMetadata::default())
        .expect("turn");
    let background = recorder
        .start_background_call_scope(Some("memory-summarize"), OwnerMetadata::default())
        .expect("background");
    recorder
        .record_model_request_for_owner(
            &background,
            RequestMetadata::default(),
            None,
            &json!({"input": ["background"]}),
        )
        .expect("background request");
    recorder.finish_owner_scope(&background, OwnerStatus::Completed, None);

    let tool_call = recorder
        .record_tool_call_request(
            "shell.exec",
            ToolCallMetadata::default(),
            &json!({"cmd": "true"}),
        )
        .expect("tool request");

    assert!(
        tool_call
            .path
            .starts_with(&format!("{}/tool-calls/", turn.path))
    );
    let turn_record = serde_json::from_str::<serde_json::Value>(&std::fs::read_to_string(
        session_path.join(&turn.path).join("turn.json"),
    )?)?;
    assert_eq!(turn_record["tool_call_paths"][0], tool_call.path);
    let background_record = serde_json::from_str::<serde_json::Value>(&std::fs::read_to_string(
        session_path.join(&background.path).join("internal.json"),
    )?)?;
    assert_eq!(
        background_record["tool_call_paths"]
            .as_array()
            .expect("background tool calls")
            .len(),
        0
    );

    Ok(())
}

#[test]
fn scoped_background_request_does_not_steal_active_turn_tool_calls()
-> Result<(), Box<dyn std::error::Error>> {
    let temp = support::TempDir::new()?;
    let config = TraceConfig::from_env_map([
        ("CODEX_TRACE", "1"),
        ("CODEX_TRACE_DIR", temp.path().to_str().unwrap()),
    ]);
    let recorder = TraceRecorder::start_session(
        config,
        SessionMetadata {
            workspace_cwd: Some(temp.path().to_path_buf()),
            cwd: Some(temp.path().to_path_buf()),
            ..Default::default()
        },
    );
    let session_path = recorder.session_path().expect("enabled session");

    let turn = recorder
        .start_turn("active user prompt", OwnerMetadata::default())
        .expect("turn");
    let background = recorder
        .start_background_call_scope(Some("memory-summarize"), OwnerMetadata::default())
        .expect("background");
    recorder
        .record_model_request_for_owner(
            &background,
            RequestMetadata::default(),
            None,
            &json!({"input": ["background"]}),
        )
        .expect("background request");

    let tool_call = recorder
        .record_tool_call_request(
            "shell.exec",
            ToolCallMetadata::default(),
            &json!({"cmd": "true"}),
        )
        .expect("tool request");

    assert!(
        tool_call
            .path
            .starts_with(&format!("{}/tool-calls/", turn.path))
    );
    let turn_record = serde_json::from_str::<serde_json::Value>(&std::fs::read_to_string(
        session_path.join(&turn.path).join("turn.json"),
    )?)?;
    assert_eq!(turn_record["tool_call_paths"][0], tool_call.path);
    let background_record = serde_json::from_str::<serde_json::Value>(&std::fs::read_to_string(
        session_path.join(&background.path).join("internal.json"),
    )?)?;
    assert_eq!(
        background_record["tool_call_paths"]
            .as_array()
            .expect("background tool calls")
            .len(),
        0
    );

    Ok(())
}

#[test]
fn scoped_background_tool_call_can_be_recorded_for_explicit_owner()
-> Result<(), Box<dyn std::error::Error>> {
    let temp = support::TempDir::new()?;
    let config = TraceConfig::from_env_map([
        ("CODEX_TRACE", "1"),
        ("CODEX_TRACE_DIR", temp.path().to_str().unwrap()),
    ]);
    let recorder = TraceRecorder::start_session(
        config,
        SessionMetadata {
            workspace_cwd: Some(temp.path().to_path_buf()),
            cwd: Some(temp.path().to_path_buf()),
            ..Default::default()
        },
    );
    let session_path = recorder.session_path().expect("enabled session");

    let turn = recorder
        .start_turn("active user prompt", OwnerMetadata::default())
        .expect("turn");
    let background = recorder
        .start_background_call_scope(Some("memory-summarize"), OwnerMetadata::default())
        .expect("background");
    let request = recorder
        .record_tool_call_request_for_owner(
            &background,
            "shell.exec",
            ToolCallMetadata::default(),
            &json!({"cmd": "true"}),
        )
        .expect("tool request");
    let result = recorder
        .record_tool_call_result_for_owner(
            &background,
            "shell.exec",
            ToolCallMetadata {
                request_trace_id: Some(request.id.clone()),
                request_trace_path: Some(request.path.clone()),
                ..Default::default()
            },
            &json!({"status": "ok"}),
        )
        .expect("tool result");

    assert!(
        request
            .path
            .starts_with(&format!("{}/tool-calls/", background.path))
    );
    assert_eq!(
        request.path.replace("request.json", "result.json"),
        result.path
    );
    let turn_record = serde_json::from_str::<serde_json::Value>(&std::fs::read_to_string(
        session_path.join(&turn.path).join("turn.json"),
    )?)?;
    assert_eq!(turn_record["tool_call_paths"].as_array().unwrap().len(), 0);
    let background_record = serde_json::from_str::<serde_json::Value>(&std::fs::read_to_string(
        session_path.join(&background.path).join("internal.json"),
    )?)?;
    assert_eq!(
        background_record["tool_call_paths"],
        json!([request.path, result.path])
    );

    Ok(())
}

#[test]
fn enabled_recorder_writes_session_turn_request_and_tool_files()
-> Result<(), Box<dyn std::error::Error>> {
    let temp = support::TempDir::new()?;
    let config = TraceConfig::from_env_map([
        ("CODEX_TRACE", "1"),
        ("CODEX_TRACE_DIR", temp.path().to_str().unwrap()),
    ]);
    let recorder = TraceRecorder::start_session(
        config,
        SessionMetadata {
            workspace_cwd: Some(temp.path().to_path_buf()),
            cwd: Some(temp.path().to_path_buf()),
            ..Default::default()
        },
    );

    let session_path = recorder.session_path().expect("enabled session");
    assert!(session_path.join("manifest.json").exists());
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&std::fs::read_to_string(
            session_path.join("config.json")
        )?)?,
        json!({
            "trace_format_version": 1,
            "config_recorded": false
        })
    );
    recorder.record_config(&json!({"model": "gpt-test"}));
    recorder.record_config(&json!({"model": "ignored"}));
    let turn = recorder
        .start_turn("Review the tracing plan", OwnerMetadata::default())
        .expect("turn id");
    assert!(session_path.join(&turn.path).join("prompt.txt").exists());
    let request = recorder
        .record_model_request(
            RequestMetadata {
                provider: Some("test-provider".to_string()),
                model: Some("gpt-test".to_string()),
                status: Some(RequestStatus::Completed),
                provider_request_id: Some("provider-request-1".to_string()),
                provider_response_id: Some("provider-response-1".to_string()),
                error: Some("transient error from previous attempt".to_string()),
                ..Default::default()
            },
            None,
            &json!({"input": [{"role": "user"}]}),
        )
        .expect("request id");
    recorder.record_response_event(&request, &json!({"delta": "hi"}));
    recorder.record_model_response(&request, &json!({"output": "hi"}));
    recorder.record_usage(
        &request,
        &json!({"input_tokens": 1, "cache_read_input_tokens": 2}),
    );
    recorder
        .record_tool_call_request(
            "shell.exec",
            ToolCallMetadata::default(),
            &json!({"cmd": "true"}),
        )
        .expect("tool request");
    recorder.finish_owner(OwnerStatus::Completed, None);

    let request_path = session_path.join(&request.path);
    assert!(request_path.exists());
    assert_eq!(
        request.path,
        format!("{}/requests/{}/request.json", turn.path, request.id)
    );
    assert!(
        session_path
            .join(format!(
                "{}/requests/{}/response.events.jsonl",
                turn.path, request.id
            ))
            .exists()
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&std::fs::read_to_string(
            session_path.join("config.json")
        )?)?,
        json!({"model": "gpt-test"})
    );
    assert!(session_path.join("requests/index.json").exists());
    let request_meta_path = request_path.with_file_name("meta.json");
    let request_meta =
        serde_json::from_str::<serde_json::Value>(&std::fs::read_to_string(request_meta_path)?)?;
    assert_eq!(request_meta["owner_scope"], "user_turn");
    assert!(
        request_meta["owner_path"]
            .as_str()
            .unwrap()
            .starts_with("turns/")
    );
    assert_eq!(request_meta["status"], "completed");
    assert_eq!(request_meta["provider_request_id"], "provider-request-1");
    assert_eq!(request_meta["provider_response_id"], "provider-response-1");
    assert!(request_meta["started_at"].is_string());
    assert!(request_meta["ended_at"].is_null());
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&std::fs::read_to_string(request_path)?)?,
        json!({"input": [{"role": "user"}]})
    );
    let manifest = serde_json::from_str::<serde_json::Value>(&std::fs::read_to_string(
        session_path.join("manifest.json"),
    )?)?;
    assert_eq!(manifest["turns"].as_array().unwrap().len(), 1);
    let turn_record = serde_json::from_str::<serde_json::Value>(&std::fs::read_to_string(
        session_path
            .join(manifest["turns"][0].as_str().unwrap())
            .join("turn.json"),
    )?)?;
    assert_eq!(turn_record["status"], "completed");
    assert_eq!(turn_record["request_paths"][0], request.path);
    assert_eq!(turn_record["tool_call_paths"].as_array().unwrap().len(), 1);
    let tool_call_path = turn_record["tool_call_paths"][0].as_str().unwrap();
    assert!(tool_call_path.starts_with(&format!("{}/tool-calls/", turn.path)));
    assert!(tool_call_path.ends_with("/request.json"));
    Ok(())
}

#[test]
fn tool_call_result_reuses_request_folder() -> Result<(), Box<dyn std::error::Error>> {
    let temp = support::TempDir::new()?;
    let config = TraceConfig::from_env_map([
        ("CODEX_TRACE", "1"),
        ("CODEX_TRACE_DIR", temp.path().to_str().unwrap()),
    ]);
    let recorder = TraceRecorder::start_session(
        config,
        SessionMetadata {
            workspace_cwd: Some(temp.path().to_path_buf()),
            cwd: Some(temp.path().to_path_buf()),
            ..Default::default()
        },
    );
    let session_path = recorder.session_path().expect("enabled session");
    let turn = recorder
        .start_turn("tool layout", OwnerMetadata::default())
        .expect("turn");
    let request = recorder
        .record_tool_call_request(
            "shell.exec",
            ToolCallMetadata {
                call_id: Some("call-1".to_string()),
                ..Default::default()
            },
            &json!({"cmd": "true"}),
        )
        .expect("tool request");
    let result = recorder
        .record_tool_call_result(
            "shell.exec",
            ToolCallMetadata {
                call_id: Some("call-1".to_string()),
                request_trace_id: Some(request.id.clone()),
                request_trace_path: Some(request.path.clone()),
                ..Default::default()
            },
            &json!({"status": "ok"}),
        )
        .expect("tool result");

    assert_eq!(
        request.path,
        format!("{}/tool-calls/{}/request.json", turn.path, request.id)
    );
    assert_eq!(
        result.path,
        format!("{}/tool-calls/{}/result.json", turn.path, request.id)
    );
    assert!(session_path.join(&request.path).exists());
    assert!(session_path.join(&result.path).exists());

    let turn_record = serde_json::from_str::<serde_json::Value>(&std::fs::read_to_string(
        session_path.join(&turn.path).join("turn.json"),
    )?)?;
    assert_eq!(
        turn_record["tool_call_paths"],
        json!([request.path, result.path])
    );
    Ok(())
}

#[test]
fn tool_call_write_failure_does_not_mutate_owner_metadata() -> Result<(), Box<dyn std::error::Error>>
{
    let temp = support::TempDir::new()?;
    let config = TraceConfig::from_env_map([
        ("CODEX_TRACE", "1"),
        ("CODEX_TRACE_DIR", temp.path().to_str().unwrap()),
    ]);
    let recorder = TraceRecorder::start_session(
        config,
        SessionMetadata {
            workspace_cwd: Some(temp.path().to_path_buf()),
            cwd: Some(temp.path().to_path_buf()),
            ..Default::default()
        },
    );
    let session_path = recorder.session_path().expect("enabled session");
    let turn = recorder
        .start_turn("Review the tracing plan", OwnerMetadata::default())
        .expect("turn id");
    std::fs::write(
        session_path.join(&turn.path).join("tool-calls"),
        b"not a dir",
    )?;

    assert!(
        recorder
            .record_tool_call_request(
                "shell.exec",
                ToolCallMetadata::default(),
                &json!({"cmd": "true"}),
            )
            .is_none()
    );

    let turn_record = serde_json::from_str::<serde_json::Value>(&std::fs::read_to_string(
        session_path.join(&turn.path).join("turn.json"),
    )?)?;
    assert_eq!(turn_record["tool_call_paths"].as_array().unwrap().len(), 0);
    Ok(())
}

#[test]
fn enabled_recorder_reuses_unchanged_tool_snapshots() -> Result<(), Box<dyn std::error::Error>> {
    let temp = support::TempDir::new()?;
    let config = TraceConfig::from_env_map([
        ("CODEX_TRACE", "1"),
        ("CODEX_TRACE_DIR", temp.path().to_str().unwrap()),
    ]);
    let recorder = TraceRecorder::start_session(
        config,
        SessionMetadata {
            workspace_cwd: Some(temp.path().to_path_buf()),
            cwd: Some(temp.path().to_path_buf()),
            ..Default::default()
        },
    );

    let first = recorder
        .record_tool_snapshot("startup", &json!([{"name": "shell"}]))
        .expect("first snapshot");
    let second = recorder
        .record_tool_snapshot("unchanged", &json!([{"name": "shell"}]))
        .expect("reused snapshot");
    let third = recorder
        .record_tool_snapshot(
            "changed",
            &json!([{"name": "shell"}, {"name": "apply_patch"}]),
        )
        .expect("changed snapshot");

    assert_eq!(first, second);
    assert_ne!(first, third);
    Ok(())
}

#[test]
fn enabled_recorder_records_tool_snapshot_on_request() -> Result<(), Box<dyn std::error::Error>> {
    let temp = support::TempDir::new()?;
    let config = TraceConfig::from_env_map([
        ("CODEX_TRACE", "1"),
        ("CODEX_TRACE_DIR", temp.path().to_str().unwrap()),
    ]);
    let recorder = TraceRecorder::start_session(
        config,
        SessionMetadata {
            workspace_cwd: Some(temp.path().to_path_buf()),
            cwd: Some(temp.path().to_path_buf()),
            ..Default::default()
        },
    );
    let session_path = recorder.session_path().expect("enabled session");
    let snapshot = recorder
        .record_tool_snapshot("startup", &json!([{"name": "shell"}]))
        .expect("snapshot");
    let request = recorder
        .record_model_request(
            RequestMetadata::default(),
            Some(&snapshot),
            &json!({"input": []}),
        )
        .expect("request");

    let request_meta_path = session_path.join(&request.path).with_file_name("meta.json");
    let meta =
        serde_json::from_str::<serde_json::Value>(&std::fs::read_to_string(request_meta_path)?)?;
    assert_eq!(meta["tool_snapshot_id"], snapshot.id);
    assert_eq!(meta["tool_snapshot_path"], snapshot.path);
    Ok(())
}

#[test]
fn sessions_started_in_same_root_do_not_collide() -> Result<(), Box<dyn std::error::Error>> {
    let temp = support::TempDir::new()?;
    let config = TraceConfig::from_env_map([
        ("CODEX_TRACE", "1"),
        ("CODEX_TRACE_DIR", temp.path().to_str().unwrap()),
    ]);
    let metadata = SessionMetadata {
        workspace_cwd: Some(temp.path().to_path_buf()),
        cwd: Some(temp.path().to_path_buf()),
        ..Default::default()
    };

    let first = TraceRecorder::start_session(config.clone(), metadata.clone())
        .session_path()
        .expect("first session");
    let second = TraceRecorder::start_session(config, metadata)
        .session_path()
        .expect("second session");

    assert_ne!(first, second);
    assert!(first.join("session.json").exists());
    assert!(second.join("session.json").exists());
    Ok(())
}

#[test]
fn failed_request_artifact_write_returns_no_id_and_skips_index()
-> Result<(), Box<dyn std::error::Error>> {
    let temp = support::TempDir::new()?;
    let config = TraceConfig::from_env_map([
        ("CODEX_TRACE", "1"),
        ("CODEX_TRACE_DIR", temp.path().to_str().unwrap()),
    ]);
    let recorder = TraceRecorder::start_session(
        config,
        SessionMetadata {
            workspace_cwd: Some(temp.path().to_path_buf()),
            cwd: Some(temp.path().to_path_buf()),
            ..Default::default()
        },
    );
    let session_path = recorder.session_path().expect("enabled session");

    assert!(
        recorder
            .record_model_request(RequestMetadata::default(), None, &FailingSerialize)
            .is_none()
    );
    let index = serde_json::from_str::<serde_json::Value>(&std::fs::read_to_string(
        session_path.join("requests/index.json"),
    )?)?;
    assert_eq!(index.as_array().unwrap().len(), 0);
    assert_eq!(std::fs::read_dir(session_path.join("requests"))?.count(), 1);
    Ok(())
}

#[test]
fn failed_tool_snapshot_write_returns_no_id_and_skips_index()
-> Result<(), Box<dyn std::error::Error>> {
    let temp = support::TempDir::new()?;
    let config = TraceConfig::from_env_map([
        ("CODEX_TRACE", "1"),
        ("CODEX_TRACE_DIR", temp.path().to_str().unwrap()),
    ]);
    let recorder = TraceRecorder::start_session(
        config,
        SessionMetadata {
            workspace_cwd: Some(temp.path().to_path_buf()),
            cwd: Some(temp.path().to_path_buf()),
            ..Default::default()
        },
    );
    let session_path = recorder.session_path().expect("enabled session");

    assert!(
        recorder
            .record_tool_snapshot("startup", &FailingSerialize)
            .is_none()
    );
    let index = serde_json::from_str::<serde_json::Value>(&std::fs::read_to_string(
        session_path.join("tools/index.json"),
    )?)?;
    assert_eq!(index.as_array().unwrap().len(), 0);
    Ok(())
}

#[test]
fn request_metadata_tracks_actual_sidecars_and_final_status()
-> Result<(), Box<dyn std::error::Error>> {
    let temp = support::TempDir::new()?;
    let config = TraceConfig::from_env_map([
        ("CODEX_TRACE", "1"),
        ("CODEX_TRACE_DIR", temp.path().to_str().unwrap()),
    ]);
    let recorder = TraceRecorder::start_session(
        config,
        SessionMetadata {
            workspace_cwd: Some(temp.path().to_path_buf()),
            cwd: Some(temp.path().to_path_buf()),
            ..Default::default()
        },
    );
    let session_path = recorder.session_path().expect("enabled session");
    let request = recorder
        .record_model_request(
            RequestMetadata {
                provider_request_id: Some("provider-request-1".to_string()),
                ..Default::default()
            },
            None,
            &json!({"input": []}),
        )
        .expect("request");
    let request_meta_path = session_path.join(&request.path).with_file_name("meta.json");

    let started =
        serde_json::from_str::<serde_json::Value>(&std::fs::read_to_string(&request_meta_path)?)?;
    assert_eq!(started["status"], "started");
    assert!(started["ended_at"].is_null());
    assert!(started["response_events_path"].is_null());
    assert!(started["response_final_path"].is_null());
    assert!(started["usage_path"].is_null());

    recorder.record_response_event(&request, &json!({"event": "delta"}));
    recorder.record_model_response(&request, &json!({"output": "done"}));
    recorder.record_usage(&request, &json!({"input_tokens": 1}));
    recorder.finish_model_request(
        &request,
        RequestUpdate {
            status: Some(RequestStatus::Completed),
            provider_response_id: Some("provider-response-1".to_string()),
            ..Default::default()
        },
    );

    let finished =
        serde_json::from_str::<serde_json::Value>(&std::fs::read_to_string(request_meta_path)?)?;
    assert_eq!(finished["status"], "completed");
    assert!(finished["ended_at"].is_string());
    assert_eq!(finished["provider_request_id"], "provider-request-1");
    assert_eq!(finished["provider_response_id"], "provider-response-1");
    assert!(
        finished["response_events_path"]
            .as_str()
            .unwrap()
            .ends_with("/response.events.jsonl")
    );
    assert!(
        finished["response_final_path"]
            .as_str()
            .unwrap()
            .ends_with("/response.final.json")
    );
    assert!(
        finished["usage_path"]
            .as_str()
            .unwrap()
            .ends_with("/usage.json")
    );

    let index = serde_json::from_str::<serde_json::Value>(&std::fs::read_to_string(
        session_path.join("requests/index.json"),
    )?)?;
    assert_eq!(index[0]["status"], "completed");
    assert_eq!(
        index[0]["response_final_path"],
        finished["response_final_path"]
    );
    Ok(())
}

#[test]
fn failed_sidecar_writes_do_not_claim_metadata_paths() -> Result<(), Box<dyn std::error::Error>> {
    let temp = support::TempDir::new()?;
    let config = TraceConfig::from_env_map([
        ("CODEX_TRACE", "1"),
        ("CODEX_TRACE_DIR", temp.path().to_str().unwrap()),
    ]);
    let recorder = TraceRecorder::start_session(
        config,
        SessionMetadata {
            workspace_cwd: Some(temp.path().to_path_buf()),
            cwd: Some(temp.path().to_path_buf()),
            ..Default::default()
        },
    );
    let session_path = recorder.session_path().expect("enabled session");
    let request = recorder
        .record_model_request(RequestMetadata::default(), None, &json!({"input": []}))
        .expect("request");
    let request_dir = session_path
        .join(&request.path)
        .parent()
        .unwrap()
        .to_path_buf();
    std::fs::create_dir_all(request_dir.join("response.events.jsonl"))?;
    std::fs::create_dir_all(request_dir.join("response.final.json"))?;
    std::fs::create_dir_all(request_dir.join("usage.json"))?;

    recorder.record_response_event(&request, &json!({"event": "delta"}));
    recorder.record_model_response(&request, &json!({"output": "done"}));
    recorder.record_usage(&request, &json!({"input_tokens": 1}));

    let request_meta_path = session_path.join(&request.path).with_file_name("meta.json");
    let meta =
        serde_json::from_str::<serde_json::Value>(&std::fs::read_to_string(request_meta_path)?)?;
    assert!(meta["response_events_path"].is_null());
    assert!(meta["response_final_path"].is_null());
    assert!(meta["usage_path"].is_null());
    let index = serde_json::from_str::<serde_json::Value>(&std::fs::read_to_string(
        session_path.join("requests/index.json"),
    )?)?;
    assert!(index[0]["response_events_path"].is_null());
    assert!(index[0]["response_final_path"].is_null());
    assert!(index[0]["usage_path"].is_null());
    Ok(())
}

#[test]
fn subagent_sessions_are_linked_from_parent_manifest_and_spawn()
-> Result<(), Box<dyn std::error::Error>> {
    let temp = support::TempDir::new()?;
    let config = TraceConfig::from_env_map([
        ("CODEX_TRACE", "1"),
        ("CODEX_TRACE_DIR", temp.path().to_str().unwrap()),
    ]);
    let recorder = TraceRecorder::start_session(
        config.clone(),
        SessionMetadata {
            workspace_cwd: Some(temp.path().to_path_buf()),
            cwd: Some(temp.path().to_path_buf()),
            ..Default::default()
        },
    );
    let spawn = recorder
        .record_subagent_spawn(
            codex_local_trace::schema::SubagentMetadata {
                name: Some("agent-review".to_string()),
                ..Default::default()
            },
            &json!({"prompt": "review"}),
        )
        .expect("spawn");
    let child = recorder.start_subagent_session(
        config,
        SessionMetadata {
            workspace_cwd: Some(temp.path().to_path_buf()),
            cwd: Some(temp.path().to_path_buf()),
            parent_session_path: recorder.session_path(),
            parent_turn_id: Some("turn-parent".to_string()),
            parent_request_id: Some("request-parent".to_string()),
            parent_spawn_id: Some("spawn-parent".to_string()),
            parent_spawn_path: Some(spawn.path.clone()),
            ..Default::default()
        },
    );
    let child_path = child.session_path().expect("child path");
    recorder.record_subagent_nested_trace_path(&spawn, &child_path);

    let parent_path = recorder.session_path().expect("parent path");
    let child_relative_path = child_path.strip_prefix(&parent_path)?;
    let manifest = serde_json::from_str::<serde_json::Value>(&std::fs::read_to_string(
        parent_path.join("manifest.json"),
    )?)?;
    assert_eq!(manifest["subagent_sessions"].as_array().unwrap().len(), 1);
    assert_eq!(
        parent_path.join(manifest["subagent_sessions"][0].as_str().unwrap()),
        child_path
    );
    let spawn_record = serde_json::from_str::<serde_json::Value>(&std::fs::read_to_string(
        parent_path.join(&spawn.path),
    )?)?;
    assert_eq!(
        spawn_record["metadata"]["nested_trace_path"],
        child_relative_path.to_string_lossy().as_ref()
    );
    let child_session = serde_json::from_str::<serde_json::Value>(&std::fs::read_to_string(
        child_path.join("session.json"),
    )?)?;
    assert_eq!(
        child_session["parent_session_path"],
        parent_path.to_string_lossy().as_ref()
    );
    assert_eq!(child_session["parent_turn_id"], "turn-parent");
    assert_eq!(child_session["parent_request_id"], "request-parent");
    assert_eq!(child_session["parent_spawn_id"], "spawn-parent");
    assert_eq!(child_session["parent_spawn_path"], spawn.path.as_str());
    Ok(())
}
