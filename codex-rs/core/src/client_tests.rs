use super::AuthRequestTelemetryContext;
use super::ModelClient;
use super::PendingUnauthorizedRetry;
use super::UnauthorizedRecoveryExecution;
use super::X_CODEX_INSTALLATION_ID_HEADER;
use super::X_CODEX_PARENT_THREAD_ID_HEADER;
use super::X_CODEX_TURN_METADATA_HEADER;
use super::X_CODEX_WINDOW_ID_HEADER;
use super::X_OPENAI_SUBAGENT_HEADER;
use crate::AttestationContext;
use crate::AttestationProvider;
use crate::GenerateAttestationFuture;
use crate::client_common::Prompt;
use codex_api::ApiError;
use codex_api::ResponseEvent;
use codex_app_server_protocol::AuthMode;
use codex_local_trace::TraceConfig;
use codex_local_trace::TraceRecorder;
use codex_local_trace::schema::OwnerMetadata;
use codex_local_trace::schema::OwnerScopeKind;
use codex_local_trace::schema::OwnerStatus;
use codex_local_trace::schema::RequestMetadata;
use codex_local_trace::schema::RequestRecord;
use codex_local_trace::schema::RequestStatus;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_model_provider::BearerAuthProvider;
use codex_model_provider_info::CHATGPT_CODEX_BASE_URL;
use codex_model_provider_info::ModelProviderInfo;
use codex_model_provider_info::WireApi;
use codex_model_provider_info::create_oss_provider_with_base_url;
use codex_otel::SessionTelemetry;
use codex_protocol::SessionId;
use codex_protocol::ThreadId;
use codex_protocol::models::BaseInstructions;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::protocol::InternalSessionSource;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::TokenUsage;
use codex_rollout_trace::ExecutionStatus;
use codex_rollout_trace::InferenceTraceAttempt;
use codex_rollout_trace::InferenceTraceContext;
use codex_rollout_trace::RawTraceEventPayload;
use codex_rollout_trace::RolloutTrace;
use codex_rollout_trace::TraceWriter;
use codex_rollout_trace::replay_bundle;
use futures::StreamExt;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::fs;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::task::Context;
use std::task::Poll;
use std::time::Duration;
use tempfile::TempDir;
use tokio::sync::Notify;
use tracing::Event;
use tracing::Subscriber;
use tracing::field::Visit;
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context as LayerContext;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt;

fn test_model_client(session_source: SessionSource) -> ModelClient {
    let provider = create_oss_provider_with_base_url("https://example.com/v1", WireApi::Responses);
    test_model_client_with_provider_and_recorder(
        session_source,
        provider,
        TraceRecorder::disabled(),
    )
}

fn test_model_client_with_provider_and_recorder(
    session_source: SessionSource,
    provider: ModelProviderInfo,
    trace_recorder: TraceRecorder,
) -> ModelClient {
    let thread_id = ThreadId::new();
    ModelClient::new(
        /*auth_manager*/ None,
        thread_id.into(),
        thread_id,
        /*installation_id*/ "11111111-1111-4111-8111-111111111111".to_string(),
        provider,
        session_source,
        /*model_verbosity*/ None,
        /*enable_request_compression*/ false,
        /*include_timing_metrics*/ false,
        /*beta_features_header*/ None,
        /*attestation_provider*/ None,
        trace_recorder,
    )
}

fn test_model_info() -> ModelInfo {
    serde_json::from_value(json!({
        "slug": "gpt-test",
        "display_name": "gpt-test",
        "description": "desc",
        "default_reasoning_level": "medium",
        "supported_reasoning_levels": [
            {"effort": "medium", "description": "medium"}
        ],
        "shell_type": "shell_command",
        "visibility": "list",
        "supported_in_api": true,
        "priority": 1,
        "upgrade": null,
        "base_instructions": "base instructions",
        "model_messages": null,
        "supports_reasoning_summaries": false,
        "support_verbosity": false,
        "default_verbosity": null,
        "apply_patch_tool_type": null,
        "truncation_policy": {"mode": "bytes", "limit": 10000},
        "supports_parallel_tool_calls": false,
        "supports_image_detail_original": false,
        "context_window": 272000,
        "auto_compact_token_limit": null,
        "experimental_supported_tools": []
    }))
    .expect("deserialize test model info")
}

fn test_session_telemetry() -> SessionTelemetry {
    SessionTelemetry::new(
        ThreadId::new(),
        "gpt-test",
        "gpt-test",
        /*account_id*/ None,
        /*account_email*/ None,
        /*auth_mode*/ None,
        "test-originator".to_string(),
        /*log_user_prompts*/ false,
        "test-terminal".to_string(),
        SessionSource::Cli,
    )
}

fn local_trace_recorder(temp: &TempDir) -> TraceRecorder {
    TraceRecorder::start_session_at(
        TraceConfig::from_env_map([
            ("CODEX_TRACE".to_string(), "1".to_string()),
            (
                "CODEX_TRACE_DIR".to_string(),
                temp.path().to_string_lossy().to_string(),
            ),
        ]),
        codex_local_trace::schema::SessionMetadata {
            codex_session_id: Some("session-1".to_string()),
            workspace_cwd: Some(temp.path().to_path_buf()),
            cwd: Some(temp.path().to_path_buf()),
            ..Default::default()
        },
    )
}

fn trace_json(path: &Path) -> serde_json::Value {
    serde_json::from_str(&fs::read_to_string(path).expect("read trace JSON"))
        .expect("parse trace JSON")
}

fn trace_jsonl(path: &Path) -> Vec<serde_json::Value> {
    fs::read_to_string(path)
        .expect("read trace JSONL")
        .lines()
        .map(|line| serde_json::from_str(line).expect("parse trace JSONL line"))
        .collect()
}

fn request_index(session_path: &Path) -> Vec<RequestRecord> {
    serde_json::from_str(
        &fs::read_to_string(session_path.join("requests/index.json")).expect("read request index"),
    )
    .expect("parse request index")
}

fn test_prompt() -> Prompt {
    Prompt {
        input: vec![
            core_test_support::responses::user_message_item("first"),
            core_test_support::responses::user_message_item("second"),
        ],
        base_instructions: BaseInstructions {
            text: "test instructions".to_string(),
        },
        ..Default::default()
    }
}

fn test_websocket_request() -> codex_api::ResponsesWsRequest {
    codex_api::ResponsesWsRequest::ResponseCreate(codex_api::ResponseCreateWsRequest {
        model: "gpt-test".to_string(),
        instructions: "test instructions".to_string(),
        previous_response_id: None,
        input: test_prompt().get_formatted_input(),
        tools: Vec::new(),
        tool_choice: "auto".to_string(),
        parallel_tool_calls: false,
        reasoning: None,
        store: false,
        stream: true,
        include: Vec::new(),
        service_tier: None,
        prompt_cache_key: None,
        text: None,
        generate: None,
        client_metadata: None,
    })
}

#[derive(Debug)]
struct SequenceResponder {
    responses: Vec<wiremock::ResponseTemplate>,
    next_response: AtomicUsize,
}

impl SequenceResponder {
    fn new(responses: Vec<wiremock::ResponseTemplate>) -> Self {
        Self {
            responses,
            next_response: AtomicUsize::new(0),
        }
    }
}

impl wiremock::Respond for SequenceResponder {
    fn respond(&self, _request: &wiremock::Request) -> wiremock::ResponseTemplate {
        let response_index = self.next_response.fetch_add(1, Ordering::SeqCst);
        self.responses
            .get(response_index)
            .cloned()
            .unwrap_or_else(|| self.responses.last().expect("response template").clone())
    }
}

#[derive(Default)]
struct TagCollectorVisitor {
    tags: BTreeMap<String, String>,
}

impl Visit for TagCollectorVisitor {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.tags
            .insert(field.name().to_string(), value.to_string());
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.tags
            .insert(field.name().to_string(), format!("{value:?}"));
    }
}

#[derive(Clone)]
struct TagCollectorLayer {
    tags: Arc<Mutex<BTreeMap<String, String>>>,
}

impl<S> Layer<S> for TagCollectorLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &Event<'_>, _ctx: LayerContext<'_, S>) {
        if event.metadata().target() != "feedback_tags" {
            return;
        }
        let mut visitor = TagCollectorVisitor::default();
        event.record(&mut visitor);
        self.tags.lock().unwrap().extend(visitor.tags);
    }
}

fn started_inference_attempt(temp: &TempDir) -> anyhow::Result<InferenceTraceAttempt> {
    let writer = Arc::new(TraceWriter::create(
        temp.path(),
        "trace-1".to_string(),
        "rollout-1".to_string(),
        "thread-root".to_string(),
    )?);
    writer.append(RawTraceEventPayload::ThreadStarted {
        thread_id: "thread-root".to_string(),
        agent_path: "/root".to_string(),
        metadata_payload: None,
    })?;
    writer.append(RawTraceEventPayload::CodexTurnStarted {
        codex_turn_id: "turn-1".to_string(),
        thread_id: "thread-root".to_string(),
    })?;

    let inference_trace = InferenceTraceContext::enabled(
        writer,
        "thread-root".to_string(),
        "turn-1".to_string(),
        "gpt-test".to_string(),
        "test-provider".to_string(),
    );
    let attempt = inference_trace.start_attempt();
    attempt.record_started(&json!({
        "model": "gpt-test",
        "input": [{
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": "hello"}]
        }],
    }));
    Ok(attempt)
}

fn output_message(id: &str, text: &str) -> ResponseItem {
    ResponseItem::Message {
        id: Some(id.to_string()),
        role: "assistant".to_string(),
        content: vec![ContentItem::OutputText {
            text: text.to_string(),
        }],
        phase: None,
    }
}

async fn replay_until_cancelled(temp: &TempDir) -> anyhow::Result<RolloutTrace> {
    let mut rollout = replay_bundle(temp.path())?;
    for _ in 0..50 {
        let inference = rollout
            .inference_calls
            .values()
            .next()
            .expect("inference should be reduced");
        if inference.execution.status == ExecutionStatus::Cancelled {
            return Ok(rollout);
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
        rollout = replay_bundle(temp.path())?;
    }
    Ok(rollout)
}

struct NotifyAfterEventStream {
    events: VecDeque<ResponseEvent>,
    yielded: usize,
    notify_after: usize,
    notify: Arc<Notify>,
}

impl futures::Stream for NotifyAfterEventStream {
    type Item = std::result::Result<ResponseEvent, ApiError>;

    fn poll_next(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let Some(event) = self.events.pop_front() else {
            return Poll::Pending;
        };
        self.yielded += 1;
        if self.yielded == self.notify_after {
            self.notify.notify_one();
        }
        Poll::Ready(Some(Ok(event)))
    }
}

#[test]
fn build_subagent_headers_sets_other_subagent_label() {
    let client = test_model_client(SessionSource::SubAgent(SubAgentSource::Other(
        "memory_consolidation".to_string(),
    )));
    let headers = client.build_subagent_headers();
    let value = headers
        .get(X_OPENAI_SUBAGENT_HEADER)
        .and_then(|value| value.to_str().ok());
    assert_eq!(value, Some("memory_consolidation"));
}

#[test]
fn build_subagent_headers_sets_internal_memory_consolidation_label() {
    let client = test_model_client(SessionSource::Internal(
        InternalSessionSource::MemoryConsolidation,
    ));
    let headers = client.build_subagent_headers();
    let value = headers
        .get(X_OPENAI_SUBAGENT_HEADER)
        .and_then(|value| value.to_str().ok());
    assert_eq!(value, Some("memory_consolidation"));
}

#[test]
fn build_ws_client_metadata_includes_window_lineage_and_turn_metadata() {
    let parent_thread_id = ThreadId::new();
    let client = test_model_client(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id,
        depth: 2,
        agent_path: None,
        agent_nickname: None,
        agent_role: None,
    }));

    client.advance_window_generation();

    let client_metadata = client.build_ws_client_metadata(Some(r#"{"turn_id":"turn-123"}"#));
    let thread_id = client.state.thread_id;
    assert_eq!(
        client_metadata,
        std::collections::HashMap::from([
            (
                X_CODEX_INSTALLATION_ID_HEADER.to_string(),
                "11111111-1111-4111-8111-111111111111".to_string(),
            ),
            (
                X_CODEX_WINDOW_ID_HEADER.to_string(),
                format!("{thread_id}:1"),
            ),
            (
                X_OPENAI_SUBAGENT_HEADER.to_string(),
                "collab_spawn".to_string(),
            ),
            (
                X_CODEX_PARENT_THREAD_ID_HEADER.to_string(),
                parent_thread_id.to_string(),
            ),
            (
                X_CODEX_TURN_METADATA_HEADER.to_string(),
                r#"{"turn_id":"turn-123"}"#.to_string(),
            ),
        ])
    );
}

#[tokio::test]
async fn summarize_memories_returns_empty_for_empty_input() {
    let client = test_model_client(SessionSource::Cli);
    let model_info = test_model_info();
    let session_telemetry = test_session_telemetry();

    let output = client
        .summarize_memories(
            Vec::new(),
            &model_info,
            /*effort*/ None,
            &session_telemetry,
        )
        .await
        .expect("empty summarize request should succeed");
    assert_eq!(output.len(), 0);
}

#[tokio::test]
async fn local_trace_records_compaction_request() -> anyhow::Result<()> {
    let server = core_test_support::responses::start_mock_server().await;
    core_test_support::responses::mount_compact_json_once(
        &server,
        json!({
            "output": [core_test_support::responses::user_message_item("compacted")]
        }),
    )
    .await;
    let temp = TempDir::new()?;
    let recorder = local_trace_recorder(&temp);
    let session_path = recorder.session_path().expect("trace session path");
    let provider = create_oss_provider_with_base_url(&server.uri(), WireApi::Responses);
    let client =
        test_model_client_with_provider_and_recorder(SessionSource::Cli, provider, recorder);

    let output = client
        .compact_conversation_history(
            &test_prompt(),
            &test_model_info(),
            super::CompactConversationRequestSettings {
                effort: None,
                summary: codex_protocol::config_types::ReasoningSummary::None,
                service_tier: None,
            },
            &test_session_telemetry(),
            &codex_rollout_trace::CompactionTraceContext::disabled(),
        )
        .await?;

    assert_eq!(output.len(), 1);
    let records = request_index(&session_path);
    assert_eq!(records.len(), 1);
    let record = &records[0];
    assert_eq!(record.owner_scope, OwnerScopeKind::Compaction);
    assert!(
        record
            .owner_path
            .as_deref()
            .is_some_and(|path| path.starts_with("internal/"))
    );
    assert_eq!(record.endpoint.as_deref(), Some("/responses/compact"));
    assert_eq!(record.status, RequestStatus::Completed);
    assert!(record.response_final_path.is_some());
    assert!(record.tool_snapshot_path.is_some());
    assert_eq!(
        trace_json(&session_path.join(&record.request_path))["input"][0]["content"][0]["text"],
        "first"
    );

    Ok(())
}

#[tokio::test]
async fn local_trace_records_compaction_retry_attempts() -> anyhow::Result<()> {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path_regex(".*/responses/compact$"))
        .respond_with(SequenceResponder::new(vec![
            wiremock::ResponseTemplate::new(500),
            wiremock::ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_json(json!({
                    "output": [core_test_support::responses::user_message_item("compacted")]
                })),
        ]))
        .mount(&server)
        .await;
    let temp = TempDir::new()?;
    let recorder = local_trace_recorder(&temp);
    let session_path = recorder.session_path().expect("trace session path");
    let mut provider = create_oss_provider_with_base_url(&server.uri(), WireApi::Responses);
    provider.request_max_retries = Some(1);
    let client =
        test_model_client_with_provider_and_recorder(SessionSource::Cli, provider, recorder);

    let output = client
        .compact_conversation_history(
            &test_prompt(),
            &test_model_info(),
            super::CompactConversationRequestSettings {
                effort: None,
                summary: codex_protocol::config_types::ReasoningSummary::None,
                service_tier: None,
            },
            &test_session_telemetry(),
            &codex_rollout_trace::CompactionTraceContext::disabled(),
        )
        .await?;

    assert_eq!(output.len(), 1);
    let records = request_index(&session_path);
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].owner_scope, OwnerScopeKind::Compaction);
    assert_eq!(records[0].retry_attempt, Some(1));
    assert_eq!(records[0].status, RequestStatus::Failed);
    assert!(records[0].error.is_some());
    assert_eq!(records[1].owner_scope, OwnerScopeKind::Compaction);
    assert_eq!(records[1].retry_attempt, Some(2));
    assert_eq!(
        records[1].previous_attempt_id,
        Some(records[0].trace_request_id.clone())
    );
    assert_eq!(
        records[1].previous_attempt_path,
        Some(records[0].request_path.clone())
    );
    assert_eq!(records[1].status, RequestStatus::Completed);
    assert_ne!(records[0].request_path, records[1].request_path);

    Ok(())
}

#[tokio::test]
async fn local_trace_records_memory_summarize_request() -> anyhow::Result<()> {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path_regex(
            ".*/memories/trace_summarize$",
        ))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_json(json!({
                    "output": [{
                        "trace_summary": "raw memory",
                        "memory_summary": "summary"
                    }]
                })),
        )
        .mount(&server)
        .await;
    let temp = TempDir::new()?;
    let recorder = local_trace_recorder(&temp);
    let session_path = recorder.session_path().expect("trace session path");
    let provider = create_oss_provider_with_base_url(&server.uri(), WireApi::Responses);
    let client =
        test_model_client_with_provider_and_recorder(SessionSource::Cli, provider, recorder);

    let output = client
        .summarize_memories(
            vec![codex_api::RawMemory {
                id: "memory-1".to_string(),
                metadata: codex_api::RawMemoryMetadata {
                    source_path: "memories.jsonl".to_string(),
                },
                items: vec![json!({"role": "user", "content": "remember this"})],
            }],
            &test_model_info(),
            /*effort*/ None,
            &test_session_telemetry(),
        )
        .await?;

    assert_eq!(output.len(), 1);
    let records = request_index(&session_path);
    assert_eq!(records.len(), 1);
    let record = &records[0];
    assert_eq!(record.owner_scope, OwnerScopeKind::Background);
    assert!(
        record
            .owner_path
            .as_deref()
            .is_some_and(|path| path.starts_with("internal/"))
    );
    assert_eq!(
        record.endpoint.as_deref(),
        Some("/memories/trace_summarize")
    );
    assert_eq!(record.status, RequestStatus::Completed);
    assert!(record.response_final_path.is_some());
    assert_eq!(
        trace_json(&session_path.join(&record.request_path))["traces"][0]["id"],
        "memory-1"
    );

    Ok(())
}

#[tokio::test]
async fn local_trace_records_memory_summarize_retry_attempts() -> anyhow::Result<()> {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path_regex(
            ".*/memories/trace_summarize$",
        ))
        .respond_with(SequenceResponder::new(vec![
            wiremock::ResponseTemplate::new(500),
            wiremock::ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_json(json!({
                    "output": [{
                        "trace_summary": "raw memory",
                        "memory_summary": "summary"
                    }]
                })),
        ]))
        .mount(&server)
        .await;
    let temp = TempDir::new()?;
    let recorder = local_trace_recorder(&temp);
    let session_path = recorder.session_path().expect("trace session path");
    let mut provider = create_oss_provider_with_base_url(&server.uri(), WireApi::Responses);
    provider.request_max_retries = Some(1);
    let client =
        test_model_client_with_provider_and_recorder(SessionSource::Cli, provider, recorder);

    let output = client
        .summarize_memories(
            vec![codex_api::RawMemory {
                id: "memory-1".to_string(),
                metadata: codex_api::RawMemoryMetadata {
                    source_path: "memories.jsonl".to_string(),
                },
                items: vec![json!({"role": "user", "content": "remember this"})],
            }],
            &test_model_info(),
            /*effort*/ None,
            &test_session_telemetry(),
        )
        .await?;

    assert_eq!(output.len(), 1);
    let records = request_index(&session_path);
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].owner_scope, OwnerScopeKind::Background);
    assert_eq!(records[0].retry_attempt, Some(1));
    assert_eq!(records[0].status, RequestStatus::Failed);
    assert!(records[0].error.is_some());
    assert_eq!(records[1].owner_scope, OwnerScopeKind::Background);
    assert_eq!(records[1].retry_attempt, Some(2));
    assert_eq!(
        records[1].previous_attempt_id,
        Some(records[0].trace_request_id.clone())
    );
    assert_eq!(
        records[1].previous_attempt_path,
        Some(records[0].request_path.clone())
    );
    assert_eq!(records[1].status, RequestStatus::Completed);
    assert_ne!(records[0].request_path, records[1].request_path);

    Ok(())
}

#[tokio::test]
async fn local_trace_records_streaming_model_request() -> anyhow::Result<()> {
    let server = core_test_support::responses::start_mock_server().await;
    let response_mock = core_test_support::responses::mount_sse_once(
        &server,
        core_test_support::responses::sse(vec![
            core_test_support::responses::ev_response_created("resp-1"),
            core_test_support::responses::ev_assistant_message("msg-1", "hello"),
            ev_completed_with_usage(
                "resp-1",
                json!({
                    "input_tokens": 17,
                    "input_tokens_details": { "cached_tokens": 5 },
                    "output_tokens": 7,
                    "output_tokens_details": { "reasoning_tokens": 3 },
                    "total_tokens": 24,
                    "provider_specific": { "kept": true },
                }),
            ),
        ]),
    )
    .await;
    let temp = TempDir::new()?;
    let recorder = local_trace_recorder(&temp);
    let session_path = recorder.session_path().expect("trace session path");
    let provider = create_oss_provider_with_base_url(&server.uri(), WireApi::Responses);
    let client =
        test_model_client_with_provider_and_recorder(SessionSource::Cli, provider, recorder);
    let mut client_session = client.new_session();
    let mut stream = client_session
        .stream(
            &test_prompt(),
            &test_model_info(),
            &test_session_telemetry(),
            /*effort*/ None,
            codex_protocol::config_types::ReasoningSummary::None,
            /*service_tier*/ None,
            /*turn_metadata_header*/ None,
            &InferenceTraceContext::disabled(),
        )
        .await?;

    while let Some(event) = stream.next().await {
        event?;
    }

    let provider_request = response_mock.single_request().body_json();
    let records = request_index(&session_path);
    assert_eq!(records.len(), 1);
    let record = &records[0];
    assert_eq!(record.owner_scope, OwnerScopeKind::UserTurn);
    assert!(
        record
            .owner_path
            .as_deref()
            .is_some_and(|path| path.starts_with("turns/"))
    );
    let turn_path = record.owner_path.as_ref().expect("turn owner path");
    assert!(session_path.join(turn_path).join("turn.json").exists());
    assert_eq!(
        fs::read_to_string(session_path.join(turn_path).join("prompt.txt"))?,
        "first\n\nsecond"
    );
    assert!(
        record
            .request_path
            .starts_with(&format!("{turn_path}/requests/"))
    );
    assert_eq!(record.retry_attempt, Some(1));
    assert_eq!(record.provider.as_deref(), Some("gpt-oss"));
    assert_eq!(record.endpoint.as_deref(), Some("/responses"));
    assert_eq!(record.model.as_deref(), Some("gpt-test"));
    assert_eq!(record.status, RequestStatus::Completed);
    assert_eq!(record.provider_response_id.as_deref(), Some("resp-1"));
    assert!(record.ended_at.is_some());
    assert!(record.response_events_path.is_some());
    assert!(record.response_final_path.is_some());
    assert!(record.usage_path.is_some());
    assert!(record.tool_snapshot_path.is_some());

    let traced_request = trace_json(&session_path.join(&record.request_path));
    assert_eq!(traced_request, provider_request);
    let traced_input = traced_request["input"].as_array().expect("input array");
    assert_eq!(traced_input[0]["content"][0]["text"], "first");
    assert_eq!(traced_input[1]["content"][0]["text"], "second");

    let events =
        trace_jsonl(&session_path.join(record.response_events_path.as_ref().expect("events path")));
    assert!(
        events
            .iter()
            .any(|event| event["type"] == "RawProviderEvent")
    );
    assert!(events.iter().any(|event| event["type"] == "Created"));
    assert!(events.iter().any(|event| event["type"] == "OutputItemDone"));
    assert!(events.iter().any(|event| event["type"] == "Completed"));

    let final_response = trace_json(
        &session_path.join(
            record
                .response_final_path
                .as_ref()
                .expect("final response path"),
        ),
    );
    assert_eq!(final_response["response_id"], "resp-1");
    assert!(final_response["items_added"].to_string().contains("hello"));

    let usage = trace_json(&session_path.join(record.usage_path.as_ref().expect("usage path")));
    assert_eq!(
        usage["raw_provider_usage"]["input_tokens_details"]["cached_tokens"],
        5
    );
    assert_eq!(
        usage["raw_provider_usage"]["output_tokens_details"]["reasoning_tokens"],
        3
    );
    assert_eq!(
        usage["raw_provider_usage"]["provider_specific"]["kept"],
        true
    );
    assert_eq!(
        serde_json::from_value::<TokenUsage>(usage["token_usage"].clone())?,
        TokenUsage {
            input_tokens: 17,
            cached_input_tokens: 5,
            output_tokens: 7,
            reasoning_output_tokens: 3,
            total_tokens: 24,
        }
    );

    Ok(())
}

#[tokio::test]
async fn local_trace_records_streaming_compaction_owner() -> anyhow::Result<()> {
    let server = core_test_support::responses::start_mock_server().await;
    core_test_support::responses::mount_sse_once(
        &server,
        core_test_support::responses::sse(vec![
            core_test_support::responses::ev_response_created("resp-compact"),
            core_test_support::responses::ev_assistant_message("msg-compact", "summary"),
            core_test_support::responses::ev_completed("resp-compact"),
        ]),
    )
    .await;
    let temp = TempDir::new()?;
    let recorder = local_trace_recorder(&temp);
    let session_path = recorder.session_path().expect("trace session path");
    let owner = recorder
        .start_compaction_call_scope(Some("compaction"), OwnerMetadata::default())
        .expect("compaction owner");
    let provider = create_oss_provider_with_base_url(&server.uri(), WireApi::Responses);
    let client = test_model_client_with_provider_and_recorder(
        SessionSource::Cli,
        provider,
        recorder.clone(),
    );
    let mut client_session = client.new_session();
    let mut stream = client_session
        .stream_with_local_trace_owner(
            &test_prompt(),
            &test_model_info(),
            &test_session_telemetry(),
            /*effort*/ None,
            codex_protocol::config_types::ReasoningSummary::None,
            /*service_tier*/ None,
            /*turn_metadata_header*/ None,
            &InferenceTraceContext::disabled(),
            owner,
        )
        .await?;

    while let Some(event) = stream.next().await {
        event?;
    }

    let records = request_index(&session_path);
    assert_eq!(records.len(), 1);
    let record = &records[0];
    assert_eq!(record.owner_scope, OwnerScopeKind::Compaction);
    assert!(
        record
            .owner_path
            .as_deref()
            .is_some_and(|path| path.starts_with("internal/"))
    );
    assert_eq!(record.endpoint.as_deref(), Some("/responses"));
    assert_eq!(record.status, RequestStatus::Completed);
    assert!(record.request_path.starts_with(&format!(
        "{}/requests/",
        record.owner_path.as_ref().unwrap()
    )));

    Ok(())
}

#[tokio::test]
async fn websocket_prerequest_failure_finishes_explicit_local_trace_owner() -> anyhow::Result<()> {
    let temp = TempDir::new()?;
    let recorder = local_trace_recorder(&temp);
    let session_path = recorder.session_path().expect("trace session path");
    let owner = recorder
        .start_compaction_call_scope(Some("compaction"), OwnerMetadata::default())
        .expect("compaction owner");
    let mut provider =
        create_oss_provider_with_base_url("http://127.0.0.1:9/v1", WireApi::Responses);
    provider.supports_websockets = true;
    provider.websocket_connect_timeout_ms = Some(50);
    let client =
        test_model_client_with_provider_and_recorder(SessionSource::Cli, provider, recorder);
    let mut client_session = client.new_session();

    let result = client_session
        .stream_with_local_trace_owner(
            &test_prompt(),
            &test_model_info(),
            &test_session_telemetry(),
            /*effort*/ None,
            codex_protocol::config_types::ReasoningSummary::None,
            /*service_tier*/ None,
            /*turn_metadata_header*/ None,
            &InferenceTraceContext::disabled(),
            owner.clone(),
        )
        .await;

    assert!(result.is_err());
    let owner_record = trace_json(&session_path.join(&owner.path).join("internal.json"));
    assert_eq!(owner_record["status"], "failed");
    assert!(
        owner_record["error"]
            .as_str()
            .is_some_and(|error| !error.is_empty())
    );
    assert_eq!(
        owner_record["request_paths"]
            .as_array()
            .expect("request paths")
            .len(),
        0
    );

    Ok(())
}

#[tokio::test]
async fn local_trace_records_retry_attempts() -> anyhow::Result<()> {
    let server = core_test_support::responses::start_mock_server().await;
    core_test_support::responses::mount_response_once(
        &server,
        wiremock::ResponseTemplate::new(500),
    )
    .await;
    core_test_support::responses::mount_sse_once(
        &server,
        core_test_support::responses::sse(vec![
            core_test_support::responses::ev_response_created("resp-2"),
            core_test_support::responses::ev_completed("resp-2"),
        ]),
    )
    .await;
    let temp = TempDir::new()?;
    let recorder = local_trace_recorder(&temp);
    let session_path = recorder.session_path().expect("trace session path");
    let mut provider = create_oss_provider_with_base_url(&server.uri(), WireApi::Responses);
    provider.stream_max_retries = Some(1);
    let client =
        test_model_client_with_provider_and_recorder(SessionSource::Cli, provider, recorder);
    let mut client_session = client.new_session();
    let mut stream = client_session
        .stream(
            &test_prompt(),
            &test_model_info(),
            &test_session_telemetry(),
            /*effort*/ None,
            codex_protocol::config_types::ReasoningSummary::None,
            /*service_tier*/ None,
            /*turn_metadata_header*/ None,
            &InferenceTraceContext::disabled(),
        )
        .await?;

    while let Some(event) = stream.next().await {
        event?;
    }

    let records = request_index(&session_path);
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].retry_attempt, Some(1));
    assert_eq!(records[0].status, RequestStatus::Failed);
    assert!(records[0].error.is_some());
    assert_eq!(records[1].retry_attempt, Some(2));
    assert_eq!(
        records[1].previous_attempt_id,
        Some(records[0].trace_request_id.clone())
    );
    assert_eq!(
        records[1].previous_attempt_path,
        Some(records[0].request_path.clone())
    );
    assert_eq!(records[1].status, RequestStatus::Completed);
    assert_ne!(records[0].request_path, records[1].request_path);

    Ok(())
}

#[tokio::test]
async fn local_trace_groups_multiple_model_requests_in_one_user_turn() -> anyhow::Result<()> {
    let server = core_test_support::responses::start_mock_server().await;
    core_test_support::responses::mount_sse_once(
        &server,
        core_test_support::responses::sse(vec![
            core_test_support::responses::ev_response_created("resp-1"),
            core_test_support::responses::ev_completed("resp-1"),
        ]),
    )
    .await;
    core_test_support::responses::mount_sse_once(
        &server,
        core_test_support::responses::sse(vec![
            core_test_support::responses::ev_response_created("resp-2"),
            core_test_support::responses::ev_completed("resp-2"),
        ]),
    )
    .await;
    let temp = TempDir::new()?;
    let recorder = local_trace_recorder(&temp);
    let session_path = recorder.session_path().expect("trace session path");
    let provider = create_oss_provider_with_base_url(&server.uri(), WireApi::Responses);
    let client =
        test_model_client_with_provider_and_recorder(SessionSource::Cli, provider, recorder);
    let mut client_session = client.new_session();
    client_session.start_local_trace_turn("first user prompt", Some("turn-1".to_string()));

    for _ in 0..2 {
        let mut stream = client_session
            .stream(
                &test_prompt(),
                &test_model_info(),
                &test_session_telemetry(),
                /*effort*/ None,
                codex_protocol::config_types::ReasoningSummary::None,
                /*service_tier*/ None,
                /*turn_metadata_header*/ None,
                &InferenceTraceContext::disabled(),
            )
            .await?;
        while let Some(event) = stream.next().await {
            event?;
        }
    }

    let records = request_index(&session_path);
    assert_eq!(records.len(), 2);
    assert_eq!(
        client_session
            .current_model_request_trace_context()
            .map(|context| context.id),
        Some(records[1].trace_request_id.clone())
    );
    let owner_path = records[0].owner_path.as_deref().expect("owner path");
    assert!(owner_path.starts_with("turns/"));
    assert_eq!(records[1].owner_path.as_deref(), Some(owner_path));
    assert!(records.iter().all(|record| {
        record
            .request_path
            .starts_with(&format!("{owner_path}/requests/"))
    }));
    let turn_record = trace_json(&session_path.join(owner_path).join("turn.json"));
    assert!(turn_record["status"].is_null());

    client_session.finish_local_trace_turn(OwnerStatus::Completed, None);
    let turn_record = trace_json(&session_path.join(owner_path).join("turn.json"));
    assert_eq!(turn_record["status"], "completed");
    assert_eq!(
        turn_record["request_paths"]
            .as_array()
            .expect("requests")
            .len(),
        2
    );

    Ok(())
}

#[test]
fn local_http_trace_links_auth_recovery_attempts_when_attempt_counter_restarts()
-> anyhow::Result<()> {
    let temp = TempDir::new()?;
    let recorder = local_trace_recorder(&temp);
    let session_path = recorder.session_path().expect("trace session path");
    let owner = recorder
        .start_turn_scope("hello", codex_local_trace::schema::OwnerMetadata::default())
        .expect("turn owner");
    let trace = super::LocalHttpRequestTrace {
        recorder: recorder.clone(),
        metadata: RequestMetadata {
            provider: Some("gpt-oss".to_string()),
            endpoint: Some("/responses".to_string()),
            model: Some("gpt-test".to_string()),
            ..Default::default()
        },
        tool_snapshot: None,
        payload: json!({"input": ["hello"]}),
        owner: Some(owner.clone()),
        finish_owner_on_terminal: true,
        current_model_request_trace: Arc::new(Mutex::new(None)),
        state: Arc::new(Mutex::new(super::LocalHttpRequestTraceState::default())),
    };

    trace.record_attempt(0, Some(http::StatusCode::UNAUTHORIZED), /*error*/ None);
    trace.record_attempt(0, Some(http::StatusCode::OK), /*error*/ None);
    trace.finish_owner(OwnerStatus::Completed, None);

    let records = request_index(&session_path);
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].retry_attempt, Some(1));
    assert_eq!(records[0].status, RequestStatus::Failed);
    assert_eq!(records[1].retry_attempt, Some(2));
    assert_eq!(
        records[1].previous_attempt_id,
        Some(records[0].trace_request_id.clone())
    );
    assert_eq!(
        records[1].previous_attempt_path,
        Some(records[0].request_path.clone())
    );
    assert!(records[0].request_path.starts_with(&owner.path));
    assert!(records[1].request_path.starts_with(&owner.path));

    Ok(())
}

#[test]
fn local_websocket_trace_links_failed_request_retry() -> anyhow::Result<()> {
    let temp = TempDir::new()?;
    let recorder = local_trace_recorder(&temp);
    let session_path = recorder.session_path().expect("trace session path");
    let provider = create_oss_provider_with_base_url("https://example.com/v1", WireApi::Responses);
    let client =
        test_model_client_with_provider_and_recorder(SessionSource::Cli, provider, recorder);
    let client_session = client.new_session();
    let prompt = test_prompt();
    let request = test_websocket_request();
    let model_info = test_model_info();

    let first = client_session
        .record_websocket_model_request(
            &prompt,
            &request,
            /*full_context_request*/ None,
            &model_info,
            /*local_trace_owner*/ None,
        )
        .expect("first websocket trace request");
    first.record_failed(
        "synthetic websocket failure",
        /*upstream_request_id*/ None,
    );
    let second = client_session
        .record_websocket_model_request(
            &prompt,
            &request,
            /*full_context_request*/ None,
            &model_info,
            /*local_trace_owner*/ None,
        )
        .expect("second websocket trace request");
    second.record_completed("resp-2", /*upstream_request_id*/ None, None, &[]);
    let third = client_session
        .record_websocket_model_request(
            &prompt,
            &request,
            /*full_context_request*/ None,
            &model_info,
            /*local_trace_owner*/ None,
        )
        .expect("third websocket trace request");
    third.record_completed("resp-3", /*upstream_request_id*/ None, None, &[]);

    let records = request_index(&session_path);
    assert_eq!(records.len(), 3);
    assert_eq!(records[0].retry_attempt, Some(1));
    assert_eq!(records[0].status, RequestStatus::Failed);
    assert_eq!(records[1].retry_attempt, Some(2));
    assert_eq!(
        records[1].previous_attempt_id,
        Some(records[0].trace_request_id.clone())
    );
    assert_eq!(
        records[1].previous_attempt_path,
        Some(records[0].request_path.clone())
    );
    assert_eq!(records[1].status, RequestStatus::Completed);
    assert_eq!(records[2].retry_attempt, Some(1));
    assert_eq!(records[2].previous_attempt_id, None);
    assert_eq!(records[2].previous_attempt_path, None);
    assert_eq!(records[2].status, RequestStatus::Completed);

    Ok(())
}

fn ev_completed_with_usage(id: &str, usage: serde_json::Value) -> serde_json::Value {
    json!({
        "type": "response.completed",
        "response": {
            "id": id,
            "usage": usage,
        }
    })
}

#[tokio::test]
async fn dropped_response_stream_traces_cancelled_partial_output() -> anyhow::Result<()> {
    let temp = TempDir::new()?;
    let attempt = started_inference_attempt(&temp)?;

    // The provider has produced one complete output item, but no terminal
    // response.completed event. The harness has enough information to keep this
    // item in history, so the trace should preserve it when the stream is
    // abandoned.
    let item = output_message("msg-1", "partial answer");
    let api_stream = futures::stream::iter([Ok(ResponseEvent::OutputItemDone(item))])
        .chain(futures::stream::pending());
    let (mut stream, _) = super::map_response_events(
        /*upstream_request_id*/ None,
        api_stream,
        test_session_telemetry(),
        attempt,
        /*local_trace_request*/ None,
    );

    let observed = stream
        .next()
        .await
        .expect("mapped stream should yield output item")?;
    assert!(matches!(observed, ResponseEvent::OutputItemDone(_)));

    // Dropping the consumer is how turn interruption/preemption stops polling
    // the provider stream. The mapper task observes that drop asynchronously
    // and records cancellation using the output items it has already seen.
    drop(stream);

    // Cancellation is recorded by the mapper task after Drop wakes it, so the
    // replay may need a short wait before the terminal event appears on disk.
    let rollout = replay_until_cancelled(&temp).await?;
    let inference = rollout
        .inference_calls
        .values()
        .next()
        .expect("inference should be reduced");

    assert_eq!(inference.execution.status, ExecutionStatus::Cancelled);
    assert_eq!(inference.response_item_ids.len(), 1);
    assert_eq!(rollout.raw_payloads.len(), 2);

    Ok(())
}

#[tokio::test]
async fn response_stream_records_last_model_feedback_ids() {
    let tags = Arc::new(Mutex::new(BTreeMap::new()));
    let _guard = tracing_subscriber::registry()
        .with(TagCollectorLayer { tags: tags.clone() })
        .set_default();

    let api_stream = futures::stream::iter([
        Ok(ResponseEvent::Created),
        Ok(ResponseEvent::Completed {
            response_id: "resp-123".to_string(),
            token_usage: None,
            end_turn: Some(true),
        }),
    ]);
    let (mut stream, _) = super::map_response_events(
        Some("req-123".to_string()),
        api_stream,
        test_session_telemetry(),
        InferenceTraceAttempt::disabled(),
        /*local_trace_request*/ None,
    );

    while stream.next().await.is_some() {}

    let tags = tags.lock().unwrap().clone();
    assert_eq!(
        tags.get("last_model_request_id").map(String::as_str),
        Some("\"req-123\"")
    );
    assert_eq!(
        tags.get("last_model_response_id").map(String::as_str),
        Some("\"resp-123\"")
    );
}

#[tokio::test]
async fn dropped_backpressured_response_stream_traces_cancelled_partial_output()
-> anyhow::Result<()> {
    let temp = TempDir::new()?;
    let attempt = started_inference_attempt(&temp)?;
    let backpressured_item_yielded = Arc::new(Notify::new());
    let mut events = VecDeque::new();
    for _ in 0..super::RESPONSE_STREAM_CHANNEL_CAPACITY {
        events.push_back(ResponseEvent::Created);
    }
    events.push_back(ResponseEvent::OutputItemDone(output_message(
        "msg-1",
        "partial answer",
    )));
    let api_stream = NotifyAfterEventStream {
        events,
        yielded: 0,
        notify_after: super::RESPONSE_STREAM_CHANNEL_CAPACITY + 1,
        notify: Arc::clone(&backpressured_item_yielded),
    };

    let (stream, _) = super::map_response_events(
        /*upstream_request_id*/ None,
        api_stream,
        test_session_telemetry(),
        attempt,
        /*local_trace_request*/ None,
    );

    // Fill the mapper channel with non-terminal events, then yield one output
    // item. The mapper has observed that item and is blocked trying to send it
    // downstream, so dropping the consumer covers the send-failure path rather
    // than the `consumer_dropped` select branch.
    backpressured_item_yielded.notified().await;
    drop(stream);

    let rollout = replay_until_cancelled(&temp).await?;
    let inference = rollout
        .inference_calls
        .values()
        .next()
        .expect("inference should be reduced");

    assert_eq!(inference.execution.status, ExecutionStatus::Cancelled);
    assert_eq!(inference.response_item_ids.len(), 1);
    assert_eq!(rollout.raw_payloads.len(), 2);

    Ok(())
}

#[test]
fn auth_request_telemetry_context_tracks_attached_auth_and_retry_phase() {
    let auth_context = AuthRequestTelemetryContext::new(
        Some(AuthMode::Chatgpt),
        &BearerAuthProvider::for_test(Some("access-token"), Some("workspace-123")),
        PendingUnauthorizedRetry::from_recovery(UnauthorizedRecoveryExecution {
            mode: "managed",
            phase: "refresh_token",
        }),
    );

    assert_eq!(auth_context.auth_mode, Some("Chatgpt"));
    assert!(auth_context.auth_header_attached);
    assert_eq!(auth_context.auth_header_name, Some("authorization"));
    assert!(auth_context.retry_after_unauthorized);
    assert_eq!(auth_context.recovery_mode, Some("managed"));
    assert_eq!(auth_context.recovery_phase, Some("refresh_token"));
}

fn model_client_with_counting_attestation(
    include_attestation: bool,
) -> (ModelClient, Arc<AtomicUsize>) {
    #[derive(Debug)]
    struct CountingAttestationProvider {
        calls: Arc<AtomicUsize>,
    }

    impl AttestationProvider for CountingAttestationProvider {
        fn header_for_request(
            &self,
            _context: AttestationContext,
        ) -> GenerateAttestationFuture<'_> {
            let calls = self.calls.clone();
            Box::pin(async move {
                let call = calls.fetch_add(1, Ordering::Relaxed) + 1;
                Some(http::HeaderValue::from_bytes(format!("v1.header-{call}").as_bytes()).unwrap())
            })
        }
    }

    let attestation_calls = Arc::new(AtomicUsize::new(0));
    let (auth_manager, provider) = if include_attestation {
        (
            Some(AuthManager::from_auth_for_testing(
                CodexAuth::create_dummy_chatgpt_auth_for_testing(),
            )),
            ModelProviderInfo::create_openai_provider(Some(CHATGPT_CODEX_BASE_URL.to_string())),
        )
    } else {
        (
            None,
            create_oss_provider_with_base_url("https://example.com/v1", WireApi::Responses),
        )
    };
    let model_client = ModelClient::new(
        auth_manager,
        SessionId::new(),
        ThreadId::new(),
        /*installation_id*/ "11111111-1111-4111-8111-111111111111".to_string(),
        provider,
        SessionSource::Exec,
        /*model_verbosity*/ None,
        /*enable_request_compression*/ false,
        /*include_timing_metrics*/ false,
        /*beta_features_header*/ None,
        Some(Arc::new(CountingAttestationProvider {
            calls: attestation_calls.clone(),
        })),
        TraceRecorder::disabled(),
    );
    (model_client, attestation_calls)
}

#[tokio::test]
async fn websocket_handshake_includes_attestation_for_chatgpt_codex_responses() {
    let (model_client, attestation_calls) =
        model_client_with_counting_attestation(/*include_attestation*/ true);

    let headers = model_client
        .build_websocket_headers(/*turn_state*/ None, /*turn_metadata_header*/ None)
        .await;

    assert_eq!(
        headers
            .get(crate::attestation::X_OAI_ATTESTATION_HEADER)
            .and_then(|value| value.to_str().ok()),
        Some("v1.header-1"),
    );
    assert_eq!(attestation_calls.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn non_chatgpt_codex_endpoints_omit_attestation_generation() {
    let (model_client, attestation_calls) =
        model_client_with_counting_attestation(/*include_attestation*/ false);
    let mut response_headers = http::HeaderMap::new();

    if let Some(header_value) = model_client.generate_attestation_header_for().await {
        response_headers.insert(crate::attestation::X_OAI_ATTESTATION_HEADER, header_value);
    }
    let mut compaction_headers = http::HeaderMap::new();
    if let Some(header_value) = model_client.generate_attestation_header_for().await {
        compaction_headers.insert(crate::attestation::X_OAI_ATTESTATION_HEADER, header_value);
    }
    let mut realtime_headers = http::HeaderMap::new();
    if let Some(header_value) = model_client.generate_attestation_header_for().await {
        realtime_headers.insert(crate::attestation::X_OAI_ATTESTATION_HEADER, header_value);
    }

    assert_eq!(
        response_headers.get(crate::attestation::X_OAI_ATTESTATION_HEADER),
        None,
    );
    assert_eq!(
        compaction_headers.get(crate::attestation::X_OAI_ATTESTATION_HEADER),
        None,
    );
    assert_eq!(
        realtime_headers.get(crate::attestation::X_OAI_ATTESTATION_HEADER),
        None,
    );
    assert_eq!(attestation_calls.load(Ordering::Relaxed), 0);
}
