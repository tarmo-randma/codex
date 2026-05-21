use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::PoisonError;
use std::time::Duration;

use codex_api::RequestTelemetry;
use codex_api::TransportError;
use codex_local_trace::TraceRecorder;
use codex_local_trace::recorder::TraceId;
use codex_local_trace::recorder::TraceOwner;
use codex_local_trace::schema::OwnerMetadata;
use codex_local_trace::schema::OwnerStatus;
use codex_local_trace::schema::RequestMetadata;
use codex_local_trace::schema::RequestStatus;
use codex_local_trace::schema::RequestUpdate;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use http::StatusCode as HttpStatusCode;

use crate::client_common::Prompt;
use crate::client_common::ResponseEvent;
use codex_response_debug_context::telemetry_transport_error_message;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ModelRequestTraceContext {
    pub(crate) id: String,
    pub(crate) path: String,
    pub(crate) owner: Option<TraceOwner>,
}

pub(crate) struct LocalTraceTurn {
    pub(crate) owner: TraceOwner,
    pub(crate) finished: bool,
}

#[derive(Clone)]
pub(crate) struct LocalTraceRequest {
    pub(crate) recorder: TraceRecorder,
    pub(crate) id: TraceId,
    pub(crate) owner: Option<TraceOwner>,
    pub(crate) finish_owner_on_terminal: bool,
    raw_provider_usage: Arc<StdMutex<Option<serde_json::Value>>>,
    pub(crate) websocket_request_trace_state:
        Option<Arc<StdMutex<LocalWebsocketRequestTraceState>>>,
}

impl LocalTraceRequest {
    pub(crate) fn new(
        recorder: TraceRecorder,
        id: TraceId,
        owner: Option<TraceOwner>,
        finish_owner_on_terminal: bool,
        websocket_request_trace_state: Option<Arc<StdMutex<LocalWebsocketRequestTraceState>>>,
    ) -> Self {
        Self {
            recorder,
            id,
            owner,
            finish_owner_on_terminal,
            raw_provider_usage: Arc::new(StdMutex::new(None)),
            websocket_request_trace_state,
        }
    }

    pub(crate) fn record_event(&self, event: &ResponseEvent) {
        if let ResponseEvent::RawProviderEvent(raw_event) = event
            && let Some(raw_usage) = raw_event
                .get("response")
                .and_then(|response| response.get("usage"))
                .cloned()
        {
            *self
                .raw_provider_usage
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = Some(raw_usage);
        }
        self.recorder
            .record_response_event(&self.id, &response_event_trace_json(event));
    }

    pub(crate) fn record_completed(
        &self,
        response_id: &str,
        upstream_request_id: Option<&str>,
        token_usage: Option<&codex_protocol::protocol::TokenUsage>,
        items_added: &[ResponseItem],
    ) {
        self.recorder.record_model_response(
            &self.id,
            &serde_json::json!({
                "response_id": response_id,
                "items_added": items_added,
            }),
        );
        let raw_provider_usage = self
            .raw_provider_usage
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if token_usage.is_some() || raw_provider_usage.is_some() {
            let mut usage = serde_json::json!({});
            if let Some(token_usage) = token_usage {
                usage["token_usage"] = serde_json::json!(token_usage);
            }
            if let Some(raw_provider_usage) = raw_provider_usage {
                usage["raw_provider_usage"] = raw_provider_usage;
            }
            self.recorder.record_usage(&self.id, &usage);
        }
        self.recorder.finish_model_request(
            &self.id,
            RequestUpdate {
                status: Some(RequestStatus::Completed),
                provider_request_id: upstream_request_id.map(str::to_string),
                provider_response_id: Some(response_id.to_string()),
                ..Default::default()
            },
        );
        if let Some(state) = &self.websocket_request_trace_state {
            *state.lock().unwrap_or_else(PoisonError::into_inner) =
                LocalWebsocketRequestTraceState::default();
        }
        self.finish_owner(OwnerStatus::Completed, None);
    }

    pub(crate) fn record_failed(&self, error: &str, upstream_request_id: Option<&str>) {
        if let Some(state) = &self.websocket_request_trace_state {
            let mut state = state.lock().unwrap_or_else(PoisonError::into_inner);
            state.previous_attempt_id = Some(self.id.id.clone());
            state.previous_attempt_path = Some(self.id.path.clone());
        }
        self.recorder.finish_model_request(
            &self.id,
            RequestUpdate {
                status: Some(RequestStatus::Failed),
                provider_request_id: upstream_request_id.map(str::to_string),
                error: Some(error.to_string()),
                ..Default::default()
            },
        );
        self.finish_owner(OwnerStatus::Failed, Some(error.to_string()));
    }

    pub(crate) fn finish_cancelled(&self, error: &str, upstream_request_id: Option<&str>) {
        if let Some(state) = &self.websocket_request_trace_state {
            *state.lock().unwrap_or_else(PoisonError::into_inner) =
                LocalWebsocketRequestTraceState::default();
        }
        self.recorder.finish_model_request(
            &self.id,
            RequestUpdate {
                status: Some(RequestStatus::Cancelled),
                provider_request_id: upstream_request_id.map(str::to_string),
                error: Some(error.to_string()),
                ..Default::default()
            },
        );
        self.finish_owner(OwnerStatus::Cancelled, Some(error.to_string()));
    }

    fn finish_owner(&self, status: OwnerStatus, error: Option<String>) {
        if self.finish_owner_on_terminal
            && let Some(owner) = &self.owner
        {
            self.recorder.finish_owner_scope(owner, status, error);
        }
    }
}

#[derive(Clone)]
pub(crate) struct LocalHttpRequestTrace {
    pub(crate) recorder: TraceRecorder,
    pub(crate) metadata: RequestMetadata,
    pub(crate) tool_snapshot: Option<TraceId>,
    pub(crate) payload: serde_json::Value,
    pub(crate) owner: Option<TraceOwner>,
    pub(crate) finish_owner_on_terminal: bool,
    pub(crate) current_model_request_trace: Arc<StdMutex<Option<ModelRequestTraceContext>>>,
    pub(crate) state: Arc<StdMutex<LocalHttpRequestTraceState>>,
}

impl LocalHttpRequestTrace {
    pub(crate) fn record_attempt(
        &self,
        attempt: u64,
        status: Option<HttpStatusCode>,
        error: Option<&TransportError>,
    ) {
        let mut metadata = self.metadata.clone();
        let (retry_attempt, previous_attempt_id, previous_attempt_path) = {
            let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            state.next_retry_attempt = state
                .next_retry_attempt
                .max(attempt.saturating_add(1) as u32);
            let retry_attempt = state.next_retry_attempt;
            state.next_retry_attempt = state.next_retry_attempt.saturating_add(1);
            (
                retry_attempt,
                state.previous_attempt_id.clone(),
                state.previous_attempt_path.clone(),
            )
        };
        metadata.retry_attempt = Some(retry_attempt);
        metadata.previous_attempt_id = previous_attempt_id;
        metadata.previous_attempt_path = previous_attempt_path;
        let request = match &self.owner {
            Some(owner) => self.recorder.record_model_request_for_owner(
                owner,
                metadata,
                self.tool_snapshot.as_ref(),
                &self.payload,
            ),
            None => self.recorder.record_model_request(
                metadata,
                self.tool_snapshot.as_ref(),
                &self.payload,
            ),
        };
        let Some(request) = request else {
            return;
        };

        let request_failed = error.is_some() || status.is_some_and(|status| !status.is_success());
        if request_failed {
            self.recorder.finish_model_request(
                &request,
                RequestUpdate {
                    status: Some(RequestStatus::Failed),
                    error: Some(error.map(telemetry_transport_error_message).unwrap_or_else(
                        || {
                            status
                                .map(|status| format!("HTTP {}", status.as_u16()))
                                .unwrap_or_else(|| "request failed".to_string())
                        },
                    )),
                    ..Default::default()
                },
            );
        }

        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.previous_attempt_id = Some(request.id.clone());
        state.previous_attempt_path = Some(request.path.clone());
        if !request_failed {
            *self
                .current_model_request_trace
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = Some(ModelRequestTraceContext {
                id: request.id.clone(),
                path: request.path.clone(),
                owner: self.owner.clone(),
            });
            state.successful_request = Some(LocalTraceRequest::new(
                self.recorder.clone(),
                request,
                self.owner.clone(),
                self.finish_owner_on_terminal,
                None,
            ));
        }
    }

    pub(crate) fn successful_request(&self) -> Option<LocalTraceRequest> {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .successful_request
            .clone()
    }

    pub(crate) fn finish_owner(&self, status: OwnerStatus, error: Option<String>) {
        if self.finish_owner_on_terminal
            && let Some(owner) = &self.owner
        {
            self.recorder.finish_owner_scope(owner, status, error);
        }
    }
}

#[derive(Default)]
pub(crate) struct LocalHttpRequestTraceState {
    previous_attempt_id: Option<String>,
    previous_attempt_path: Option<String>,
    successful_request: Option<LocalTraceRequest>,
    next_retry_attempt: u32,
}

#[derive(Default)]
pub(crate) struct LocalWebsocketRequestTraceState {
    pub(crate) previous_attempt_id: Option<String>,
    pub(crate) previous_attempt_path: Option<String>,
    pub(crate) next_retry_attempt: u32,
}

pub(crate) fn start_turn_owner_for_prompt(
    recorder: &TraceRecorder,
    prompt: &Prompt,
) -> Option<TraceOwner> {
    let prompt_text = prompt_text_for_trace(&prompt.input)?;
    recorder.start_turn_scope(&prompt_text, OwnerMetadata::default())
}

pub(crate) struct LocalTraceRequestTelemetry {
    pub(crate) inner: Arc<dyn RequestTelemetry>,
    pub(crate) local_trace: LocalHttpRequestTrace,
}

impl RequestTelemetry for LocalTraceRequestTelemetry {
    fn on_request(
        &self,
        attempt: u64,
        status: Option<HttpStatusCode>,
        error: Option<&TransportError>,
        duration: Duration,
    ) {
        self.local_trace.record_attempt(attempt, status, error);
        self.inner.on_request(attempt, status, error, duration);
    }
}

fn prompt_text_for_trace(input: &[ResponseItem]) -> Option<String> {
    let text = input
        .iter()
        .filter_map(|item| match item {
            ResponseItem::Message { role, content, .. } if role == "user" => Some(content),
            ResponseItem::Message { .. }
            | ResponseItem::Reasoning { .. }
            | ResponseItem::LocalShellCall { .. }
            | ResponseItem::FunctionCall { .. }
            | ResponseItem::ToolSearchCall { .. }
            | ResponseItem::FunctionCallOutput { .. }
            | ResponseItem::CustomToolCall { .. }
            | ResponseItem::CustomToolCallOutput { .. }
            | ResponseItem::ToolSearchOutput { .. }
            | ResponseItem::WebSearchCall { .. }
            | ResponseItem::ImageGenerationCall { .. }
            | ResponseItem::Compaction { .. }
            | ResponseItem::CompactionTrigger
            | ResponseItem::ContextCompaction { .. }
            | ResponseItem::Other => None,
        })
        .flat_map(|content| {
            content.iter().filter_map(|item| match item {
                ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                    Some(text.as_str())
                }
                ContentItem::InputImage { .. } => None,
            })
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    (!text.is_empty()).then_some(text)
}

fn response_event_trace_json(event: &ResponseEvent) -> serde_json::Value {
    match event {
        ResponseEvent::Created => serde_json::json!({ "type": "Created" }),
        ResponseEvent::RawProviderEvent(event) => {
            serde_json::json!({ "type": "RawProviderEvent", "event": event })
        }
        ResponseEvent::OutputItemDone(item) => {
            serde_json::json!({ "type": "OutputItemDone", "item": item })
        }
        ResponseEvent::OutputItemAdded(item) => {
            serde_json::json!({ "type": "OutputItemAdded", "item": item })
        }
        ResponseEvent::ServerModel(model) => {
            serde_json::json!({ "type": "ServerModel", "model": model })
        }
        ResponseEvent::ModelVerifications(verifications) => {
            serde_json::json!({ "type": "ModelVerifications", "verifications": verifications })
        }
        ResponseEvent::ServerReasoningIncluded(included) => {
            serde_json::json!({ "type": "ServerReasoningIncluded", "included": included })
        }
        ResponseEvent::Completed {
            response_id,
            token_usage,
            end_turn,
        } => serde_json::json!({
            "type": "Completed",
            "response_id": response_id,
            "token_usage": token_usage,
            "end_turn": end_turn,
        }),
        ResponseEvent::OutputTextDelta(delta) => {
            serde_json::json!({ "type": "OutputTextDelta", "delta": delta })
        }
        ResponseEvent::ToolCallInputDelta {
            item_id,
            call_id,
            delta,
        } => serde_json::json!({
            "type": "ToolCallInputDelta",
            "item_id": item_id,
            "call_id": call_id,
            "delta": delta,
        }),
        ResponseEvent::ReasoningSummaryDelta {
            delta,
            summary_index,
        } => serde_json::json!({
            "type": "ReasoningSummaryDelta",
            "delta": delta,
            "summary_index": summary_index,
        }),
        ResponseEvent::ReasoningContentDelta {
            delta,
            content_index,
        } => serde_json::json!({
            "type": "ReasoningContentDelta",
            "delta": delta,
            "content_index": content_index,
        }),
        ResponseEvent::ReasoningSummaryPartAdded { summary_index } => {
            serde_json::json!({
                "type": "ReasoningSummaryPartAdded",
                "summary_index": summary_index,
            })
        }
        ResponseEvent::RateLimits(rate_limits) => {
            serde_json::json!({ "type": "RateLimits", "rate_limits": rate_limits })
        }
        ResponseEvent::ModelsEtag(etag) => {
            serde_json::json!({ "type": "ModelsEtag", "etag": etag })
        }
    }
}
