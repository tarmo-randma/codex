use super::*;
use crate::client_local_trace::ModelRequestTraceContext;
use crate::session::tests::make_session_and_context;
use crate::tools::context::ToolCallSource;
use crate::tools::context::ToolPayload;
use crate::tools::parallel::ToolCallRuntime;
use crate::tools::router::ToolCall;
use crate::tools::router::ToolRouter;
use crate::turn_diff_tracker::TurnDiffTracker;
use codex_local_trace::schema::OwnerMetadata;
use codex_local_trace::schema::OwnerScopeKind;
use codex_local_trace::schema::RequestMetadata;
use codex_local_trace::schema::SessionMetadata;
use serde_json::json;
use std::path::Path;
use tempfile::TempDir;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use pretty_assertions::assert_eq;

struct TestHandler {
    tool_name: codex_tools::ToolName,
    supports_parallel_tool_calls: bool,
    matches_kind: bool,
}

#[async_trait::async_trait]
impl ToolExecutor<ToolInvocation> for TestHandler {
    fn tool_name(&self) -> codex_tools::ToolName {
        self.tool_name.clone()
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        self.supports_parallel_tool_calls
    }

    async fn handle(
        &self,
        _invocation: ToolInvocation,
    ) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
        Ok(Box::new(
            crate::tools::context::FunctionToolOutput::from_text("ok".to_string(), Some(true)),
        ))
    }
}

impl CoreToolRuntime for TestHandler {
    fn matches_kind(&self, _payload: &ToolPayload) -> bool {
        self.matches_kind
    }
}

struct BlockingHandler {
    tool_name: codex_tools::ToolName,
    entered: Arc<Notify>,
    release: Arc<Notify>,
}

#[async_trait::async_trait]
impl ToolExecutor<ToolInvocation> for BlockingHandler {
    fn tool_name(&self) -> codex_tools::ToolName {
        self.tool_name.clone()
    }

    async fn handle(
        &self,
        _invocation: ToolInvocation,
    ) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
        self.entered.notify_one();
        self.release.notified().await;
        Ok(Box::new(
            crate::tools::context::FunctionToolOutput::from_text(
                "released".to_string(),
                Some(true),
            ),
        ))
    }
}

impl CoreToolRuntime for BlockingHandler {}

#[test]
fn handler_looks_up_namespaced_aliases_explicitly() {
    let namespace = "mcp__codex_apps__gmail";
    let tool_name = "gmail_get_recent_emails";
    let plain_name = codex_tools::ToolName::plain(tool_name);
    let namespaced_name = codex_tools::ToolName::namespaced(namespace, tool_name);
    let plain_handler = Arc::new(TestHandler {
        tool_name: plain_name.clone(),
        supports_parallel_tool_calls: false,
        matches_kind: true,
    }) as Arc<dyn CoreToolRuntime>;
    let namespaced_handler = Arc::new(TestHandler {
        tool_name: namespaced_name.clone(),
        supports_parallel_tool_calls: false,
        matches_kind: true,
    }) as Arc<dyn CoreToolRuntime>;
    let registry = ToolRegistry::new(HashMap::from([
        (plain_name.clone(), Arc::clone(&plain_handler)),
        (namespaced_name.clone(), Arc::clone(&namespaced_handler)),
    ]));

    let plain = registry.tool(&plain_name);
    let namespaced = registry.tool(&namespaced_name);
    let missing_namespaced = registry.tool(&codex_tools::ToolName::namespaced(
        "mcp__codex_apps__calendar",
        tool_name,
    ));

    assert_eq!(plain.is_some(), true);
    assert_eq!(namespaced.is_some(), true);
    assert_eq!(missing_namespaced.is_none(), true);
    assert!(
        plain
            .as_ref()
            .is_some_and(|handler| Arc::ptr_eq(handler, &plain_handler))
    );
    assert!(
        namespaced
            .as_ref()
            .is_some_and(|handler| Arc::ptr_eq(handler, &namespaced_handler))
    );
}

#[tokio::test]
async fn local_trace_records_tool_call_request_and_result() -> anyhow::Result<()> {
    let temp = TempDir::new()?;
    let (mut session, turn) = make_session_and_context().await;
    let recorder = local_trace_recorder(temp.path());
    let owner = recorder
        .start_turn(
            "trace test",
            OwnerMetadata {
                codex_turn_id: Some(turn.sub_id.clone()),
                label: Some("trace test".to_string()),
            },
        )
        .expect("trace owner");
    session.services.local_trace_recorder = recorder.clone();
    let session = Arc::new(session);
    let turn = Arc::new(turn);
    let registry = ToolRegistry::with_handler_for_test(Arc::new(TestHandler {
        tool_name: codex_tools::ToolName::plain("test_tool"),
        supports_parallel_tool_calls: false,
        matches_kind: true,
    }));

    let result = registry
        .dispatch_any(test_invocation(
            Arc::clone(&session),
            Arc::clone(&turn),
            "call-1",
            "test_tool",
            ToolCallSource::Direct,
            ToolPayload::Function {
                arguments: r#"{"alpha":1}"#.to_string(),
            },
        ))
        .await?;

    assert_eq!(
        result.result.to_response_item("call-1", &result.payload),
        crate::tools::context::FunctionToolOutput::from_text("ok".to_string(), Some(true))
            .to_response_item(
                "call-1",
                &ToolPayload::Function {
                    arguments: r#"{"alpha":1}"#.to_string()
                }
            )
    );

    let session_path = recorder.session_path().expect("trace session path");
    let tool_call_files = trace_json_files(&session_path.join(&owner.path).join("tool-calls"));
    assert_eq!(tool_call_files.len(), 2);
    let request_file = file_with_suffix(&tool_call_files, "request.json");
    let result_file = file_with_suffix(&tool_call_files, "result.json");
    assert_eq!(request_file.parent(), result_file.parent());
    let request = read_trace_json(request_file);
    let result = read_trace_json(result_file);

    assert_eq!(request["tool_name"], json!("test_tool"));
    assert_eq!(request["metadata"]["call_id"], json!("call-1"));
    assert_eq!(request["metadata"]["source"], json!({"type": "direct"}));
    assert_eq!(request["metadata"]["owner_path"], json!(owner.path));
    assert!(request["metadata"]["sandbox"]["sandbox"].is_string());
    assert!(request["metadata"]["approval_policy"].is_string());
    assert!(request["metadata"]["started_at"].is_string());
    assert_eq!(request["payload"]["arguments"], json!(r#"{"alpha":1}"#));

    assert_eq!(result["tool_name"], json!("test_tool"));
    assert_eq!(result["metadata"]["call_id"], json!("call-1"));
    assert_eq!(result["metadata"]["status"], json!("success"));
    assert!(result["metadata"]["ended_at"].is_string());
    assert_eq!(
        result["payload"]["model_visible_result"]["call_id"],
        json!("call-1")
    );
    assert!(
        result["payload"]["model_visible_result"]
            .to_string()
            .contains("ok")
    );

    Ok(())
}

#[tokio::test]
async fn local_trace_records_tool_call_model_request_context() -> anyhow::Result<()> {
    let temp = TempDir::new()?;
    let (mut session, turn) = make_session_and_context().await;
    let recorder = attach_local_trace(&mut session, &turn, temp.path());
    let registry = ToolRegistry::with_handler_for_test(Arc::new(TestHandler {
        tool_name: codex_tools::ToolName::plain("test_tool"),
        supports_parallel_tool_calls: false,
        matches_kind: true,
    }));
    let router = Arc::new(ToolRouter::from_parts(registry, Vec::new()));
    let session = Arc::new(session);
    let turn = Arc::new(turn);
    let runtime = ToolCallRuntime::new(
        router,
        Arc::clone(&session),
        Arc::clone(&turn),
        Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new())),
        model_request_trace_context(),
    );

    let result = runtime
        .handle_tool_call_with_source(
            tool_call("model-linked-call", "{}"),
            ToolCallSource::Direct,
            CancellationToken::new(),
        )
        .await?;
    assert_eq!(result.code_mode_result(), json!("ok"));

    let session_path = recorder.session_path().expect("trace session path");
    let tool_call_files = all_tool_call_files(&session_path);
    let request = tool_trace_with_call_id(&tool_call_files, "request.json", "model-linked-call");
    let result = tool_trace_with_call_id(&tool_call_files, "result.json", "model-linked-call");
    assert_model_request_context(&request);
    assert_model_request_context(&result);

    Ok(())
}

#[tokio::test]
async fn local_trace_records_tool_call_for_explicit_non_user_model_request_owner()
-> anyhow::Result<()> {
    let temp = TempDir::new()?;
    let (mut session, turn) = make_session_and_context().await;
    let recorder = attach_local_trace(&mut session, &turn, temp.path());
    let session_path = recorder.session_path().expect("trace session path");
    let owner = recorder
        .start_internal_call_scope(Some("internal-test"), OwnerMetadata::default())
        .expect("internal owner");
    let model_request = recorder
        .record_model_request_for_owner(
            &owner,
            RequestMetadata::default(),
            None,
            &json!({"input": ["internal"]}),
        )
        .expect("internal model request");
    let registry = ToolRegistry::with_handler_for_test(Arc::new(TestHandler {
        tool_name: codex_tools::ToolName::plain("test_tool"),
        supports_parallel_tool_calls: false,
        matches_kind: true,
    }));
    let router = Arc::new(ToolRouter::from_parts(registry, Vec::new()));
    let session = Arc::new(session);
    let turn = Arc::new(turn);
    let runtime = ToolCallRuntime::new(
        router,
        Arc::clone(&session),
        Arc::clone(&turn),
        Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new())),
        Arc::new(std::sync::Mutex::new(Some(ModelRequestTraceContext {
            id: model_request.id.clone(),
            path: model_request.path.clone(),
            owner: Some(owner.clone()),
        }))),
    );

    let result = runtime
        .handle_tool_call_with_source(
            tool_call("internal-model-linked-call", "{}"),
            ToolCallSource::Direct,
            CancellationToken::new(),
        )
        .await?;
    assert_eq!(result.code_mode_result(), json!("ok"));

    let tool_call_files = trace_json_files(&session_path.join(&owner.path).join("tool-calls"));
    let request = tool_trace_with_call_id(
        &tool_call_files,
        "request.json",
        "internal-model-linked-call",
    );
    let result = tool_trace_with_call_id(
        &tool_call_files,
        "result.json",
        "internal-model-linked-call",
    );
    assert_eq!(request["metadata"]["owner_id"], json!(owner.id));
    assert_eq!(request["metadata"]["owner_path"], json!(owner.path));
    assert_eq!(
        request["metadata"]["model_request_id"],
        json!(model_request.id)
    );
    assert_eq!(
        request["metadata"]["model_request_path"],
        json!(model_request.path)
    );
    assert_eq!(
        result["metadata"]["owner_id"],
        request["metadata"]["owner_id"]
    );
    assert_eq!(
        result["metadata"]["model_request_id"],
        request["metadata"]["model_request_id"]
    );

    let turn_record = read_trace_json(&single_turn_dir(&session_path).join("turn.json"));
    assert_eq!(turn_record["tool_call_paths"].as_array().unwrap().len(), 0);
    let owner_record = read_trace_json(&session_path.join(&owner.path).join("internal.json"));
    assert_eq!(owner_record["owner_scope"], json!(OwnerScopeKind::Internal));
    assert_eq!(owner_record["tool_call_paths"].as_array().unwrap().len(), 2);

    Ok(())
}

#[tokio::test]
async fn local_trace_records_tool_call_unsupported_failure() -> anyhow::Result<()> {
    let temp = TempDir::new()?;
    let (mut session, turn) = make_session_and_context().await;
    let recorder = attach_local_trace(&mut session, &turn, temp.path());
    let registry = ToolRegistry::empty_for_test();
    let session = Arc::new(session);
    let turn = Arc::new(turn);

    let err = match registry
        .dispatch_any(test_invocation(
            session,
            turn,
            "missing-call",
            "missing_tool",
            ToolCallSource::Direct,
            ToolPayload::Function {
                arguments: "{}".to_string(),
            },
        ))
        .await
    {
        Ok(_) => panic!("unsupported tool should preserve normal failure"),
        Err(err) => err,
    };

    assert_eq!(err.to_string(), "unsupported call: missing_tool");
    let result = only_result_trace(&recorder);
    assert_eq!(result["metadata"]["status"], json!("failure"));
    assert_eq!(
        result["metadata"]["error"],
        json!("unsupported call: missing_tool")
    );
    assert_eq!(
        result["payload"]["model_visible_result"]["call_id"],
        json!("missing-call")
    );

    Ok(())
}

#[tokio::test]
async fn local_trace_records_tool_call_incompatible_payload_failure() -> anyhow::Result<()> {
    let temp = TempDir::new()?;
    let (mut session, turn) = make_session_and_context().await;
    let recorder = attach_local_trace(&mut session, &turn, temp.path());
    let registry = ToolRegistry::with_handler_for_test(Arc::new(TestHandler {
        tool_name: codex_tools::ToolName::plain("test_tool"),
        supports_parallel_tool_calls: false,
        matches_kind: false,
    }));
    let session = Arc::new(session);
    let turn = Arc::new(turn);

    let err = match registry
        .dispatch_any(test_invocation(
            session,
            turn,
            "bad-payload",
            "test_tool",
            ToolCallSource::Direct,
            ToolPayload::Custom {
                input: "raw".to_string(),
            },
        ))
        .await
    {
        Ok(_) => panic!("incompatible payload should preserve normal failure"),
        Err(err) => err,
    };

    assert_eq!(
        err.to_string(),
        "Fatal error: tool test_tool invoked with incompatible payload"
    );
    let result = only_result_trace(&recorder);
    assert_eq!(result["metadata"]["status"], json!("failure"));
    assert_eq!(
        result["metadata"]["error"],
        json!("Fatal error: tool test_tool invoked with incompatible payload")
    );
    assert_eq!(result["payload"]["error"], result["metadata"]["error"]);
    assert_eq!(
        result["payload"]["model_visible_result"],
        serde_json::Value::Null
    );

    Ok(())
}

#[tokio::test]
async fn local_trace_records_tool_call_code_mode_parallel_sources() -> anyhow::Result<()> {
    let temp = TempDir::new()?;
    let (mut session, turn) = make_session_and_context().await;
    let recorder = attach_local_trace(&mut session, &turn, temp.path());
    let registry = ToolRegistry::with_handler_for_test(Arc::new(TestHandler {
        tool_name: codex_tools::ToolName::plain("test_tool"),
        supports_parallel_tool_calls: true,
        matches_kind: true,
    }));
    let router = Arc::new(ToolRouter::from_parts(registry, Vec::new()));
    let session = Arc::new(session);
    let turn = Arc::new(turn);
    let runtime = ToolCallRuntime::new(
        router,
        Arc::clone(&session),
        Arc::clone(&turn),
        Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new())),
        model_request_trace_context(),
    );
    let cancellation = CancellationToken::new();

    let first = runtime.clone().handle_tool_call_with_source(
        tool_call("parallel-1", "{}"),
        ToolCallSource::CodeMode {
            cell_id: "cell-1".to_string(),
            runtime_tool_call_id: "runtime-1".to_string(),
        },
        cancellation.clone(),
    );
    let second = runtime.handle_tool_call_with_source(
        tool_call("parallel-2", r#"{"b":2}"#),
        ToolCallSource::CodeMode {
            cell_id: "cell-1".to_string(),
            runtime_tool_call_id: "runtime-2".to_string(),
        },
        cancellation,
    );
    let (first, second) = tokio::join!(first, second);
    assert_eq!(first?.code_mode_result(), json!("ok"));
    assert_eq!(second?.code_mode_result(), json!("ok"));

    let session_path = recorder.session_path().expect("trace session path");
    let tool_call_files = all_tool_call_files(&session_path);
    assert_eq!(tool_call_files.len(), 4);
    let file_names: Vec<_> = tool_call_files
        .iter()
        .map(|path| path.file_name().unwrap().to_string_lossy().to_string())
        .collect();
    assert_eq!(
        file_names
            .iter()
            .filter(|name| name.as_str() == "request.json")
            .count(),
        2
    );
    assert_eq!(
        file_names
            .iter()
            .filter(|name| name.as_str() == "result.json")
            .count(),
        2
    );

    let requests: Vec<_> = tool_call_files
        .iter()
        .filter(|path| path.file_name().unwrap().to_string_lossy() == "request.json")
        .map(|path| read_trace_json(path))
        .collect();
    assert!(requests.iter().any(
        |request| request["metadata"]["source"]["runtime_tool_call_id"] == json!("runtime-1")
    ));
    assert!(requests.iter().any(
        |request| request["metadata"]["source"]["runtime_tool_call_id"] == json!("runtime-2")
    ));
    for request in requests {
        assert_model_request_context(&request);
    }
    let results: Vec<_> = tool_call_files
        .iter()
        .filter(|path| path.file_name().unwrap().to_string_lossy() == "result.json")
        .map(|path| read_trace_json(path))
        .collect();
    for result in results {
        assert_model_request_context(&result);
    }

    Ok(())
}

#[tokio::test]
async fn local_trace_records_tool_call_aborted_before_dispatch() -> anyhow::Result<()> {
    let temp = TempDir::new()?;
    let (mut session, turn) = make_session_and_context().await;
    let recorder = attach_local_trace(&mut session, &turn, temp.path());
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let blocking_tool_name = codex_tools::ToolName::plain("blocking_tool");
    let test_tool_name = codex_tools::ToolName::plain("test_tool");
    let blocking_handler = Arc::new(BlockingHandler {
        tool_name: blocking_tool_name.clone(),
        entered: Arc::clone(&entered),
        release: Arc::clone(&release),
    }) as Arc<dyn CoreToolRuntime>;
    let test_handler = Arc::new(TestHandler {
        tool_name: test_tool_name.clone(),
        supports_parallel_tool_calls: false,
        matches_kind: true,
    }) as Arc<dyn CoreToolRuntime>;
    let registry = ToolRegistry::new(HashMap::from([
        (blocking_tool_name, blocking_handler),
        (test_tool_name, test_handler),
    ]));
    let router = Arc::new(ToolRouter::from_parts(registry, Vec::new()));
    let session = Arc::new(session);
    let turn = Arc::new(turn);
    let runtime = ToolCallRuntime::new(
        router,
        Arc::clone(&session),
        Arc::clone(&turn),
        Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new())),
        model_request_trace_context(),
    );
    let blocking = tokio::spawn(runtime.clone().handle_tool_call_with_source(
        tool_call_with_name("blocking-call", "blocking_tool", "{}"),
        ToolCallSource::Direct,
        CancellationToken::new(),
    ));
    entered.notified().await;

    let cancellation = CancellationToken::new();
    cancellation.cancel();

    let result = runtime
        .handle_tool_call_with_source(
            tool_call("aborted-call", "{}"),
            ToolCallSource::CodeMode {
                cell_id: "cell-1".to_string(),
                runtime_tool_call_id: "runtime-abort".to_string(),
            },
            cancellation,
        )
        .await?;
    assert_eq!(result.result.success_for_logging(), false);
    release.notify_one();
    blocking.await??;

    let session_path = recorder.session_path().expect("trace session path");
    let tool_call_files = all_tool_call_files(&session_path);
    assert_eq!(tool_call_files.len(), 4);
    let request = tool_trace_with_call_id(&tool_call_files, "request.json", "aborted-call");
    let result = tool_trace_with_call_id(&tool_call_files, "result.json", "aborted-call");

    assert_eq!(request["tool_name"], json!("test_tool"));
    assert_eq!(request["metadata"]["call_id"], json!("aborted-call"));
    assert_eq!(
        request["metadata"]["source"]["runtime_tool_call_id"],
        json!("runtime-abort")
    );
    assert_eq!(request["payload"]["arguments"], json!("{}"));
    assert_eq!(result["metadata"]["status"], json!("aborted"));
    assert_model_request_context(&request);
    assert_model_request_context(&result);
    assert_eq!(
        result["payload"]["model_visible_result"]["call_id"],
        json!("aborted-call")
    );
    assert!(
        result["payload"]["model_visible_result"]
            .to_string()
            .contains("aborted by user")
    );

    Ok(())
}

#[tokio::test]
async fn local_trace_records_mid_dispatch_abort_without_duplicate_request() -> anyhow::Result<()> {
    let temp = TempDir::new()?;
    let (mut session, turn) = make_session_and_context().await;
    let recorder = attach_local_trace(&mut session, &turn, temp.path());
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let tool_name = codex_tools::ToolName::plain("test_tool");
    let handler = Arc::new(BlockingHandler {
        tool_name: tool_name.clone(),
        entered: Arc::clone(&entered),
        release: Arc::clone(&release),
    }) as Arc<dyn CoreToolRuntime>;
    let registry = ToolRegistry::new(HashMap::from([(tool_name, handler)]));
    let router = Arc::new(ToolRouter::from_parts(registry, Vec::new()));
    let session = Arc::new(session);
    let turn = Arc::new(turn);
    let runtime = ToolCallRuntime::new(
        router,
        Arc::clone(&session),
        Arc::clone(&turn),
        Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new())),
        Arc::new(std::sync::Mutex::new(None)),
    );
    let cancellation = CancellationToken::new();
    let call = runtime.handle_tool_call_with_source(
        tool_call("mid-dispatch-abort", "{}"),
        ToolCallSource::Direct,
        cancellation.clone(),
    );
    tokio::pin!(call);

    tokio::select! {
        _ = entered.notified() => {},
        _ = &mut call => panic!("tool call completed before cancellation"),
    }

    cancellation.cancel();
    let result = call.await?;
    assert_eq!(result.result.success_for_logging(), false);
    release.notify_one();

    let session_path = recorder.session_path().expect("trace session path");
    let tool_call_files = all_tool_call_files(&session_path);
    assert_eq!(tool_call_files.len(), 2);
    assert_eq!(
        tool_call_files
            .iter()
            .filter(|path| path
                .file_name()
                .is_some_and(|name| name.to_string_lossy() == "request.json"))
            .count(),
        1
    );
    assert_eq!(
        tool_call_files
            .iter()
            .filter(|path| path
                .file_name()
                .is_some_and(|name| name.to_string_lossy() == "result.json"))
            .count(),
        1
    );
    let request_file =
        tool_file_with_call_id(&tool_call_files, "request.json", "mid-dispatch-abort");
    let request_id = request_file
        .parent()
        .and_then(|path| path.file_name())
        .expect("request file name")
        .to_string_lossy()
        .to_string();
    let request_path = request_file
        .strip_prefix(&session_path)
        .expect("request path should be under trace session")
        .to_string_lossy()
        .to_string();
    let result = tool_trace_with_call_id(&tool_call_files, "result.json", "mid-dispatch-abort");
    assert_eq!(result["metadata"]["status"], json!("aborted"));
    assert_eq!(result["metadata"]["request_trace_id"], json!(request_id));
    assert_eq!(
        result["metadata"]["request_trace_path"],
        json!(request_path)
    );

    Ok(())
}

#[tokio::test]
async fn local_trace_result_omits_request_reference_when_request_trace_fails() -> anyhow::Result<()>
{
    let temp = TempDir::new()?;
    let (mut session, turn) = make_session_and_context().await;
    let recorder = attach_local_trace(&mut session, &turn, temp.path());
    let session = Arc::new(session);
    let turn = Arc::new(turn);
    let invocation = test_invocation(
        Arc::clone(&session),
        Arc::clone(&turn),
        "request-failed",
        "test_tool",
        ToolCallSource::Direct,
        ToolPayload::Function {
            arguments: "{}".to_string(),
        },
    );
    let session_path = recorder.session_path().expect("trace session path");
    let turn_dir = single_turn_dir(&session_path);
    std::fs::write(turn_dir.join("tool-calls"), b"not a dir")?;

    assert!(record_tool_call_request(&invocation, Local::now().naive_local(), None).is_none());

    std::fs::remove_file(turn_dir.join("tool-calls"))?;
    let result = AnyToolResult {
        call_id: "request-failed".to_string(),
        payload: ToolPayload::Function {
            arguments: "{}".to_string(),
        },
        result: Box::new(crate::tools::context::FunctionToolOutput::from_text(
            "ok".to_string(),
            Some(true),
        )),
        post_tool_use_payload: None,
    };
    record_tool_call_success(&invocation, Local::now().naive_local(), None, None, &result);

    let tool_call_files = all_tool_call_files(&session_path);
    assert_eq!(tool_call_files.len(), 1);
    let result = tool_trace_with_call_id(&tool_call_files, "result.json", "request-failed");
    assert!(result["metadata"]["request_trace_id"].is_null());
    assert!(result["metadata"]["request_trace_path"].is_null());

    Ok(())
}

fn local_trace_recorder(trace_dir: &Path) -> codex_local_trace::TraceRecorder {
    let config = codex_local_trace::TraceConfig::from_env_map([
        ("CODEX_TRACE".to_string(), "1".to_string()),
        (
            "CODEX_TRACE_DIR".to_string(),
            trace_dir.to_string_lossy().to_string(),
        ),
    ]);
    codex_local_trace::TraceRecorder::start_session(config, SessionMetadata::default())
}

fn model_request_trace_context() -> Arc<std::sync::Mutex<Option<ModelRequestTraceContext>>> {
    Arc::new(std::sync::Mutex::new(Some(ModelRequestTraceContext {
        id: "model-request-trace-id".to_string(),
        path: "requests/model-request-trace-id.json".to_string(),
        owner: None,
    })))
}

fn assert_model_request_context(trace: &serde_json::Value) {
    assert_eq!(
        trace["metadata"]["model_request_id"],
        json!("model-request-trace-id")
    );
    assert_eq!(
        trace["metadata"]["model_request_path"],
        json!("requests/model-request-trace-id.json")
    );
}

fn attach_local_trace(
    session: &mut crate::session::session::Session,
    turn: &crate::session::turn_context::TurnContext,
    trace_dir: &Path,
) -> codex_local_trace::TraceRecorder {
    let recorder = local_trace_recorder(trace_dir);
    recorder
        .start_turn(
            "trace test",
            OwnerMetadata {
                codex_turn_id: Some(turn.sub_id.clone()),
                label: Some("trace test".to_string()),
            },
        )
        .expect("trace owner");
    session.services.local_trace_recorder = recorder.clone();
    recorder
}

fn tool_call(call_id: &str, arguments: &str) -> ToolCall {
    tool_call_with_name(call_id, "test_tool", arguments)
}

fn tool_call_with_name(call_id: &str, tool_name: &str, arguments: &str) -> ToolCall {
    ToolCall {
        tool_name: codex_tools::ToolName::plain(tool_name),
        call_id: call_id.to_string(),
        payload: ToolPayload::Function {
            arguments: arguments.to_string(),
        },
    }
}

fn test_invocation(
    session: Arc<crate::session::session::Session>,
    turn: Arc<crate::session::turn_context::TurnContext>,
    call_id: &str,
    tool_name: &str,
    source: ToolCallSource,
    payload: ToolPayload,
) -> ToolInvocation {
    ToolInvocation {
        session,
        turn,
        cancellation_token: CancellationToken::new(),
        tracker: Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new())),
        call_id: call_id.to_string(),
        tool_name: codex_tools::ToolName::plain(tool_name),
        source,
        payload,
    }
}

fn trace_json_files(root: &Path) -> Vec<std::path::PathBuf> {
    fn collect_json_files(root: &Path, files: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(root).expect("read trace directory") {
            let path = entry.expect("trace directory entry").path();
            if path.is_dir() {
                collect_json_files(&path, files);
            } else if path.is_file() && path.extension().is_some_and(|ext| ext == "json") {
                files.push(path);
            }
        }
    }

    let mut files = Vec::new();
    collect_json_files(root, &mut files);
    files.sort();
    files
}

fn file_with_suffix<'a>(files: &'a [std::path::PathBuf], suffix: &str) -> &'a Path {
    files
        .iter()
        .find(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().ends_with(suffix))
        })
        .map(std::path::PathBuf::as_path)
        .expect("trace file with suffix")
}

fn read_trace_json(path: &Path) -> serde_json::Value {
    serde_json::from_slice(&std::fs::read(path).expect("read trace json"))
        .expect("parse trace json")
}

fn only_result_trace(recorder: &codex_local_trace::TraceRecorder) -> serde_json::Value {
    let session_path = recorder.session_path().expect("trace session path");
    let tool_call_files = all_tool_call_files(&session_path);
    assert_eq!(tool_call_files.len(), 2);
    read_trace_json(file_with_suffix(&tool_call_files, "result.json"))
}

fn all_tool_call_files(session_path: &Path) -> Vec<std::path::PathBuf> {
    trace_json_files(&single_turn_dir(session_path).join("tool-calls"))
}

fn single_turn_dir(session_path: &Path) -> std::path::PathBuf {
    let turn_dirs: Vec<_> = std::fs::read_dir(session_path.join("turns"))
        .expect("read turns")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    assert_eq!(turn_dirs.len(), 1);
    turn_dirs[0].clone()
}

fn tool_trace_with_call_id(
    files: &[std::path::PathBuf],
    suffix: &str,
    call_id: &str,
) -> serde_json::Value {
    read_trace_json(tool_file_with_call_id(files, suffix, call_id))
}

fn tool_file_with_call_id<'a>(
    files: &'a [std::path::PathBuf],
    suffix: &str,
    call_id: &str,
) -> &'a Path {
    files
        .iter()
        .filter(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().ends_with(suffix))
        })
        .find(|path| read_trace_json(path)["metadata"]["call_id"] == json!(call_id))
        .map(std::path::PathBuf::as_path)
        .expect("tool trace with call id")
}
