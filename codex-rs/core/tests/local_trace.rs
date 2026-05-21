use std::ffi::OsStr;
use std::ffi::OsString;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use codex_features::Feature;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::user_input::UserInput;
use core_test_support::responses::ResponseMock;
use core_test_support::responses::ResponsesRequest;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_response_once;
use core_test_support::responses::mount_sse_once;
use core_test_support::responses::mount_sse_once_match;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use serial_test::serial;
use tempfile::TempDir;
use wiremock::ResponseTemplate;

const TURN_TIMEOUT: Duration = Duration::from_secs(/*secs*/ 60);
struct EnvVarGuard {
    key: &'static str,
    original: Option<OsString>,
}

impl EnvVarGuard {
    fn set(key: &'static str, value: &OsStr) -> Self {
        let original = std::env::var_os(key);
        unsafe {
            std::env::set_var(key, value);
        }
        Self { key, original }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        unsafe {
            match &self.original {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }
}

struct TraceEnv {
    _enabled: EnvVarGuard,
    _dir: EnvVarGuard,
}

impl TraceEnv {
    fn enabled(trace_root: &Path) -> Self {
        Self {
            _enabled: EnvVarGuard::set("CODEX_TRACE", OsStr::new("1")),
            _dir: EnvVarGuard::set("CODEX_TRACE_DIR", trace_root.as_os_str()),
        }
    }

    fn disabled(trace_root: &Path) -> Self {
        Self {
            _enabled: EnvVarGuard::set("CODEX_TRACE", OsStr::new("0")),
            _dir: EnvVarGuard::set("CODEX_TRACE_DIR", trace_root.as_os_str()),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial(local_trace_env)]
async fn local_trace_disabled_mode_creates_no_trace_files() -> Result<()> {
    let trace_root = TempDir::new()?;
    let _trace_env = TraceEnv::disabled(trace_root.path());
    let server = start_mock_server().await;
    mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp-1"), ev_completed("resp-1")]),
    )
    .await;

    let test = test_codex().build(&server).await?;
    submit_user_input_and_wait(&test, "trace disabled prompt").await?;

    assert_eq!(
        fs::read_dir(trace_root.path())?.count(),
        0,
        "disabled tracing should not create files under CODEX_TRACE_DIR"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial(local_trace_env)]
async fn local_trace_enabled_user_turn_records_session_and_model_context_order() -> Result<()> {
    let trace_root = TempDir::new()?;
    let _trace_env = TraceEnv::enabled(trace_root.path());
    let server = start_mock_server().await;
    let response = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp-1"), ev_completed("resp-1")]),
    )
    .await;

    let mut builder = test_codex().with_config(|config| {
        config
            .features
            .disable(Feature::EnableRequestCompression)
            .expect("test config should allow feature update");
        config.user_instructions = Some("trace test developer guidance".to_string());
    });
    let test = builder.build(&server).await?;
    submit_user_input_and_wait(&test, "Inspect trace layout now").await?;

    let session_path = only_trace_session(trace_root.path())?;
    assert!(session_path.join("manifest.json").is_file());
    assert!(session_path.join("config.json").is_file());
    assert!(session_path.join("session.json").is_file());
    assert!(session_path.join("requests/index.json").is_file());

    let manifest = trace_json(&session_path.join("manifest.json"))?;
    assert_eq!(manifest["trace_format_version"], json!(1));
    assert_eq!(manifest["session"], json!("session.json"));
    assert_eq!(manifest["config"], json!("config.json"));
    assert_eq!(manifest["requests"], json!("requests/index.json"));
    assert_eq!(
        manifest["turns"]
            .as_array()
            .context("manifest turns")?
            .len(),
        1
    );

    let turn_path = manifest["turns"][0].as_str().context("turn path")?;
    let prompt = fs::read_to_string(session_path.join(turn_path).join("prompt.txt"))?;
    assert!(
        prompt.contains("Inspect trace layout now"),
        "turn prompt should preserve the submitted user text: {prompt}"
    );

    let records = request_index(&session_path)?;
    assert_eq!(records.len(), 1);
    let record = &records[0];
    assert_eq!(record["owner_scope"], json!("user_turn"));
    assert_eq!(record["owner_path"], json!(turn_path));
    assert!(
        record["request_path"]
            .as_str()
            .context("request path")?
            .starts_with(&format!("{turn_path}/requests/"))
    );
    assert_eq!(record["provider_response_id"], json!("resp-1"));
    assert_eq!(record["status"], json!("completed"));

    let traced_request = trace_json(&session_path.join(record["request_path"].as_str().unwrap()))?;
    assert_eq!(traced_request, response.single_request().body_json());
    let input_texts = request_input_texts(&traced_request);
    assert!(
        input_texts
            .first()
            .is_some_and(|text| text.starts_with("<permissions instructions>")),
        "system/developer context should be model-visible before user text: {input_texts:?}"
    );
    assert!(
        input_texts
            .iter()
            .position(|text| text.starts_with("<environment_context>"))
            .context("environment context in request")?
            < input_texts
                .iter()
                .position(|text| text == "Inspect trace layout now")
                .context("user prompt in request")?,
        "environment context should precede the user prompt: {input_texts:?}"
    );
    assert!(
        input_texts
            .iter()
            .position(|text| text.contains("trace test developer guidance"))
            .context("developer guidance in request")?
            < input_texts
                .iter()
                .position(|text| text == "Inspect trace layout now")
                .context("user prompt in request")?,
        "developer guidance should precede the user prompt: {input_texts:?}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial(local_trace_env)]
async fn local_trace_usage_preserves_cache_reasoning_and_raw_provider_usage() -> Result<()> {
    let trace_root = TempDir::new()?;
    let _trace_env = TraceEnv::enabled(trace_root.path());
    let server = start_mock_server().await;
    mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-usage"),
            ev_completed_with_usage(
                "resp-usage",
                json!({
                    "input_tokens": 11,
                    "input_tokens_details": { "cached_tokens": 7 },
                    "output_tokens": 13,
                    "output_tokens_details": { "reasoning_tokens": 5 },
                    "total_tokens": 24
                }),
            ),
        ]),
    )
    .await;

    let test = test_codex().build(&server).await?;
    submit_user_input_and_wait(&test, "usage trace please").await?;

    let session_path = only_trace_session(trace_root.path())?;
    let records = request_index(&session_path)?;
    let usage_path = records[0]["usage_path"].as_str().context("usage path")?;
    let usage = trace_json(&session_path.join(usage_path))?;
    assert_eq!(usage["token_usage"]["input_tokens"], json!(11));
    assert_eq!(usage["token_usage"]["cached_input_tokens"], json!(7));
    assert_eq!(usage["token_usage"]["output_tokens"], json!(13));
    assert_eq!(usage["token_usage"]["reasoning_output_tokens"], json!(5));
    assert_eq!(usage["token_usage"]["total_tokens"], json!(24));
    assert_eq!(usage["raw_provider_usage"]["input_tokens"], json!(11));
    assert_eq!(
        usage["raw_provider_usage"]["input_tokens_details"]["cached_tokens"],
        json!(7)
    );
    assert_eq!(
        usage["raw_provider_usage"]["output_tokens_details"]["reasoning_tokens"],
        json!(5)
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial(local_trace_env)]
async fn local_trace_retry_attempts_and_tool_snapshot_are_indexed() -> Result<()> {
    let trace_root = TempDir::new()?;
    let _trace_env = TraceEnv::enabled(trace_root.path());
    let server = start_mock_server().await;
    mount_response_once(&server, ResponseTemplate::new(500)).await;
    mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp-2"), ev_completed("resp-2")]),
    )
    .await;

    let mut builder = test_codex().with_config(|config| {
        config.model_provider.stream_max_retries = Some(1);
    });
    let test = builder.build(&server).await?;
    submit_user_input_and_wait(&test, "retry and tools").await?;

    let session_path = only_trace_session(trace_root.path())?;
    let records = request_index(&session_path)?;
    assert_eq!(
        records.len(),
        2,
        "user turn and compaction should both be present in requests/index.json"
    );
    assert_eq!(records[0]["retry_attempt"], json!(1));
    assert_eq!(records[0]["status"], json!("failed"));
    assert_eq!(records[1]["retry_attempt"], json!(2));
    assert_eq!(
        records[1]["previous_attempt_id"],
        records[0]["trace_request_id"]
    );
    assert_eq!(
        records[1]["previous_attempt_path"],
        records[0]["request_path"]
    );
    assert_eq!(records[1]["status"], json!("completed"));

    for record in &records {
        let tool_snapshot_path = record["tool_snapshot_path"]
            .as_str()
            .context("tool snapshot path")?;
        let tools = trace_json(&session_path.join(tool_snapshot_path))?;
        let tool_names = tool_names(&tools);
        assert!(
            tool_names.iter().any(|name| name == "spawn_agent"),
            "provider-visible tool schema should include spawn_agent: {tool_names:?}"
        );
        assert!(
            tool_names
                .iter()
                .any(|name| name == "exec_command" || name == "shell_command"),
            "provider-visible tool schema should include shell execution: {tool_names:?}"
        );
    }

    let tool_index = trace_json(&session_path.join("tools/index.json"))?;
    assert_eq!(
        tool_index.as_array().context("tool index")?.len(),
        1,
        "retry attempts with identical tools should share one snapshot"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial(local_trace_env)]
async fn local_trace_compaction_request_lives_under_internal_compaction_and_index() -> Result<()> {
    let trace_root = TempDir::new()?;
    let _trace_env = TraceEnv::enabled(trace_root.path());
    let server = start_mock_server().await;
    mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-1"),
            ev_assistant_message("msg-1", "before compact reply"),
            ev_completed("resp-1"),
        ]),
    )
    .await;

    let mut builder = test_codex().with_config(|config| {
        config.compact_prompt = Some("Summarize the trace test conversation.".to_string());
    });
    let test = builder.build(&server).await?;
    submit_user_input_and_wait(&test, "compact me later").await?;
    test.codex.submit(Op::Compact).await?;
    wait_for_event(&test.codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    let session_path = only_trace_session(trace_root.path())?;
    let records = request_index(&session_path)?;
    assert_eq!(records.len(), 2);
    assert_eq!(records[1]["owner_scope"], json!("compaction"));
    let owner_path = records[1]["owner_path"]
        .as_str()
        .context("compaction owner path")?;
    assert!(
        owner_path.starts_with("internal/") && owner_path.ends_with("-compaction"),
        "compaction owner path should live under internal/<prefix>-compaction: {owner_path}"
    );
    assert!(
        records[1]["request_path"]
            .as_str()
            .context("compaction request path")?
            .starts_with(&format!("{owner_path}/requests/"))
    );
    assert!(
        session_path
            .join(records[1]["request_path"].as_str().unwrap())
            .is_file()
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial(local_trace_env)]
async fn local_trace_subagent_records_parent_spawn_child_session_and_cross_reference() -> Result<()>
{
    let trace_root = TempDir::new()?;
    let _trace_env = TraceEnv::enabled(trace_root.path());
    let server = start_mock_server().await;
    let spawn_args = serde_json::to_string(&json!({ "message": "child trace task" }))?;
    let parent_mock = mount_sse_once_match(
        &server,
        |req: &wiremock::Request| request_body_contains(req, "spawn trace child"),
        sse(vec![
            ev_response_created("resp-parent-1"),
            ev_function_call("spawn-call-1", "spawn_agent", &spawn_args),
            ev_completed("resp-parent-1"),
        ]),
    )
    .await;
    let child_mock = mount_sse_once_match(
        &server,
        |req: &wiremock::Request| request_body_contains(req, "child trace task"),
        sse(vec![
            ev_response_created("resp-child-1"),
            ev_assistant_message("msg-child-1", "child done"),
            ev_completed("resp-child-1"),
        ]),
    )
    .await;
    mount_sse_once_match(
        &server,
        |req: &wiremock::Request| request_body_contains(req, "spawn-call-1"),
        sse(vec![
            ev_response_created("resp-parent-2"),
            ev_assistant_message("msg-parent-2", "parent done"),
            ev_completed("resp-parent-2"),
        ]),
    )
    .await;

    let mut builder = test_codex().with_config(|config| {
        config
            .features
            .disable(Feature::EnableRequestCompression)
            .expect("test config should allow feature update");
    });
    let test = builder.build(&server).await?;
    submit_user_input_and_wait(&test, "spawn trace child").await?;
    wait_for_matching_request(&parent_mock, "parent spawn request", |request| {
        request.body_contains_text("spawn trace child")
    })
    .await?;
    wait_for_matching_request(&child_mock, "child request", |request| {
        request.body_contains_text("child trace task")
    })
    .await?;

    let session_path = only_top_level_trace_session(trace_root.path())?;
    let parent_records = request_index(&session_path)?;
    assert_eq!(parent_records.len(), 2);
    let parent_owner_path = parent_records[0]["owner_path"]
        .as_str()
        .context("parent turn owner path")?;
    assert!(parent_owner_path.starts_with("turns/"));
    assert!(
        parent_records.iter().all(|record| {
            record["owner_scope"] == json!("user_turn")
                && record["owner_path"] == json!(parent_owner_path)
                && record["request_path"]
                    .as_str()
                    .is_some_and(|path| path.starts_with(&format!("{parent_owner_path}/requests/")))
        }),
        "all parent model requests in one production user turn should share one owner"
    );
    let parent_turn = trace_json(&session_path.join(parent_owner_path).join("turn.json"))?;
    assert_eq!(parent_turn["status"], json!("completed"));
    assert_eq!(
        parent_turn["request_paths"]
            .as_array()
            .context("parent turn request paths")?
            .len(),
        2
    );

    let manifest = trace_json(&session_path.join("manifest.json"))?;
    let subagent_sessions = manifest["subagent_sessions"]
        .as_array()
        .context("subagent sessions")?;
    assert_eq!(subagent_sessions.len(), 1);
    let child_relative = subagent_sessions[0]
        .as_str()
        .context("child trace relative path")?;
    let child_session_path = session_path.join(child_relative);
    assert!(child_session_path.join("manifest.json").is_file());
    assert!(child_session_path.join("requests/index.json").is_file());

    let spawn_files = trace_files_with_suffix(&session_path.join("subagents"), ".json")?;
    assert_eq!(spawn_files.len(), 1);
    let spawn = trace_json(&spawn_files[0])?;
    assert_eq!(spawn["metadata"]["name"], json!("subagent"));
    assert_eq!(
        spawn["metadata"]["nested_trace_path"],
        json!(child_relative)
    );
    assert_eq!(spawn["input"]["thread_source"], json!("Subagent"));

    let child_session = trace_json(&child_session_path.join("session.json"))?;
    assert_eq!(
        child_session["parent_session_path"],
        json!(session_path.to_string_lossy().to_string())
    );
    assert_eq!(
        child_session["parent_spawn_path"],
        json!(
            spawn_files[0]
                .strip_prefix(&session_path)?
                .to_string_lossy()
                .replace('\\', "/")
        )
    );
    let child_records = request_index(&child_session_path)?;
    assert_eq!(child_records.len(), 1);
    assert_eq!(child_records[0]["retry_attempt"], json!(1));
    assert!(
        child_records[0]["request_path"]
            .as_str()
            .context("child request path")?
            .starts_with("turns/")
    );

    let tool_call_files = trace_files_with_suffix(&session_path, ".json")?
        .into_iter()
        .filter(|path| path.to_string_lossy().contains("/tool-calls/"))
        .collect::<Vec<_>>();
    let tool_request_path = tool_call_files
        .iter()
        .find(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy() == "request.json")
        })
        .context("spawn_agent tool request trace should exist")?;
    let tool_result_path = tool_call_files
        .iter()
        .find(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy() == "result.json")
        })
        .context("spawn_agent tool result trace should exist")?;
    assert_eq!(tool_request_path.parent(), tool_result_path.parent());
    for path in [tool_request_path, tool_result_path] {
        let trace = trace_json(path)?;
        assert_eq!(
            trace["metadata"]["model_request_id"],
            parent_records[0]["trace_request_id"]
        );
        assert_eq!(
            trace["metadata"]["model_request_path"],
            parent_records[0]["request_path"]
        );
    }

    Ok(())
}

fn ev_completed_with_usage(id: &str, usage: Value) -> Value {
    json!({
        "type": "response.completed",
        "response": {
            "id": id,
            "usage": usage,
        }
    })
}

async fn submit_user_input_and_wait(test: &TestCodex, prompt: &str) -> Result<()> {
    test.codex
        .submit(Op::UserInput {
            environments: None,
            items: vec![UserInput::Text {
                text: prompt.to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
        })
        .await?;
    tokio::time::timeout(TURN_TIMEOUT, async {
        wait_for_event(&test.codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;
    })
    .await
    .context("timed out waiting for turn complete")?;
    Ok(())
}

async fn wait_for_matching_request<F>(
    mock: &ResponseMock,
    label: &str,
    mut predicate: F,
) -> Result<ResponsesRequest>
where
    F: FnMut(&ResponsesRequest) -> bool,
{
    tokio::time::timeout(TURN_TIMEOUT, async {
        loop {
            if let Some(request) = mock
                .requests()
                .into_iter()
                .find(|request| predicate(request))
            {
                return Ok(request);
            }
            tokio::time::sleep(Duration::from_millis(/*millis*/ 20)).await;
        }
    })
    .await
    .with_context(|| format!("timed out waiting for {label}"))?
}

fn only_trace_session(trace_root: &Path) -> Result<PathBuf> {
    let sessions = fs::read_dir(trace_root)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<std::io::Result<Vec<_>>>()?
        .into_iter()
        .filter(|path| path.is_dir())
        .collect::<Vec<_>>();
    assert_eq!(
        sessions.len(),
        1,
        "expected exactly one trace session under {}",
        trace_root.display()
    );
    Ok(sessions.into_iter().next().expect("one session"))
}

fn only_top_level_trace_session(trace_root: &Path) -> Result<PathBuf> {
    let sessions = fs::read_dir(trace_root)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<std::io::Result<Vec<_>>>()?
        .into_iter()
        .filter(|path| path.is_dir())
        .filter(|path| {
            trace_json(&path.join("session.json"))
                .ok()
                .and_then(|session| session.get("parent_session_path").cloned())
                .is_none_or(|parent| parent.is_null())
        })
        .collect::<Vec<_>>();
    assert_eq!(
        sessions.len(),
        1,
        "expected exactly one top-level trace session under {}",
        trace_root.display()
    );
    Ok(sessions.into_iter().next().expect("one top-level session"))
}

fn trace_json(path: &Path) -> Result<Value> {
    let contents = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_str(&contents).with_context(|| format!("parse {}", path.display()))
}

fn request_index(session_path: &Path) -> Result<Vec<Value>> {
    serde_json::from_value(trace_json(&session_path.join("requests/index.json"))?)
        .context("parse request index")
}

fn request_input_texts(request: &Value) -> Vec<String> {
    request["input"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|item| {
            item["content"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|content| content["text"].as_str().map(str::to_string))
        })
        .collect()
}

fn tool_names(tools: &Value) -> Vec<String> {
    tools
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|tool| {
            tool.get("name")
                .and_then(Value::as_str)
                .or_else(|| tool.get("type").and_then(Value::as_str))
                .map(str::to_string)
        })
        .collect()
}

fn trace_files_with_suffix(root: &Path, suffix: &str) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    collect_trace_files_with_suffix(root, suffix, &mut files)?;
    files.sort();
    Ok(files)
}

fn collect_trace_files_with_suffix(
    root: &Path,
    suffix: &str,
    files: &mut Vec<PathBuf>,
) -> Result<()> {
    if !root.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(root).with_context(|| format!("read dir {}", root.display()))? {
        let path = entry?.path();
        if path.is_dir() {
            collect_trace_files_with_suffix(&path, suffix, files)?;
        } else if path
            .file_name()
            .is_some_and(|name| name.to_string_lossy().ends_with(suffix))
        {
            files.push(path);
        }
    }
    Ok(())
}

fn request_body_contains(req: &wiremock::Request, needle: &str) -> bool {
    req.body_json::<Value>()
        .ok()
        .is_some_and(|body| body.to_string().contains(needle))
}
