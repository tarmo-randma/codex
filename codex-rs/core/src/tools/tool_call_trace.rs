use chrono::Local;
use chrono::NaiveDateTime;
use codex_local_trace::recorder::TraceId;
use codex_local_trace::schema::ToolCallMetadata;
use codex_protocol::models::ResponseInputItem;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;

use crate::client_local_trace::ModelRequestTraceContext;
use crate::function_tool::FunctionCallError;
use crate::sandbox_tags::permission_profile_policy_tag;
use crate::sandbox_tags::permission_profile_sandbox_tag;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::registry::AnyToolResult;

pub(crate) fn record_tool_call_request(
    invocation: &ToolInvocation,
    started_at: NaiveDateTime,
    model_request_trace_context: Option<&ModelRequestTraceContext>,
) -> Option<TraceId> {
    let payload = tool_call_request_payload(&invocation.payload);
    let tool_name = invocation.tool_name.to_string();
    let metadata = tool_call_metadata(
        invocation,
        started_at,
        None,
        None,
        None,
        model_request_trace_context,
        None,
    );
    let recorder = &invocation.session.services.local_trace_recorder;
    if let Some(owner) = model_request_trace_context.and_then(|context| context.owner.as_ref()) {
        recorder.record_tool_call_request_for_owner(owner, tool_name.as_str(), metadata, &payload)
    } else {
        recorder.record_tool_call_request(tool_name.as_str(), metadata, &payload)
    }
}

pub(crate) fn record_aborted_tool_call(
    invocation: &ToolInvocation,
    result: &AnyToolResult,
    started_at: NaiveDateTime,
    model_request_trace_context: Option<&ModelRequestTraceContext>,
) {
    let request = record_tool_call_request(invocation, started_at, model_request_trace_context);
    record_tool_call_success(
        invocation,
        started_at,
        request.as_ref(),
        model_request_trace_context,
        result,
    );
}

pub(crate) fn record_aborted_tool_call_result(
    invocation: &ToolInvocation,
    result: &AnyToolResult,
    started_at: NaiveDateTime,
    request: &TraceId,
    model_request_trace_context: Option<&ModelRequestTraceContext>,
) {
    record_tool_call_success(
        invocation,
        started_at,
        Some(request),
        model_request_trace_context,
        result,
    );
}

pub(crate) fn record_tool_call_success(
    invocation: &ToolInvocation,
    started_at: NaiveDateTime,
    request: Option<&TraceId>,
    model_request_trace_context: Option<&ModelRequestTraceContext>,
    result: &AnyToolResult,
) {
    let success = result.result.success_for_logging();
    let log_preview = result.result.log_preview();
    let status = if success {
        "success"
    } else if log_preview.contains("aborted by user") {
        "aborted"
    } else {
        "failure"
    };
    let payload = ToolCallResultTracePayload {
        model_visible_result: result
            .result
            .to_response_item(&result.call_id, &result.payload),
        log_preview,
    };
    let metadata = tool_call_metadata(
        invocation,
        started_at,
        Some(Local::now().naive_local()),
        Some(status.to_string()),
        None,
        model_request_trace_context,
        request,
    );
    let tool_name = invocation.tool_name.to_string();
    let recorder = &invocation.session.services.local_trace_recorder;
    if let Some(owner) = model_request_trace_context.and_then(|context| context.owner.as_ref()) {
        recorder.record_tool_call_result_for_owner(owner, tool_name.as_str(), metadata, &payload);
    } else {
        recorder.record_tool_call_result(tool_name.as_str(), metadata, &payload);
    }
}

pub(crate) fn record_tool_call_failure(
    invocation: &ToolInvocation,
    started_at: NaiveDateTime,
    request: Option<&TraceId>,
    model_request_trace_context: Option<&ModelRequestTraceContext>,
    err: &FunctionCallError,
) {
    let error = err.to_string();
    let model_visible_result = match err {
        FunctionCallError::Fatal(_) => Value::Null,
        FunctionCallError::RespondToModel(_) => {
            json_value(&failure_response_item(invocation, error.clone()))
        }
    };
    let payload = json!({
        "error": error,
        "model_visible_result": model_visible_result,
    });
    let metadata = tool_call_metadata(
        invocation,
        started_at,
        Some(Local::now().naive_local()),
        Some("failure".to_string()),
        Some(error),
        model_request_trace_context,
        request,
    );
    let tool_name = invocation.tool_name.to_string();
    let recorder = &invocation.session.services.local_trace_recorder;
    if let Some(owner) = model_request_trace_context.and_then(|context| context.owner.as_ref()) {
        recorder.record_tool_call_result_for_owner(owner, tool_name.as_str(), metadata, &payload);
    } else {
        recorder.record_tool_call_result(tool_name.as_str(), metadata, &payload);
    }
}

fn failure_response_item(invocation: &ToolInvocation, message: String) -> ResponseInputItem {
    match &invocation.payload {
        ToolPayload::ToolSearch { .. } => ResponseInputItem::ToolSearchOutput {
            call_id: invocation.call_id.clone(),
            status: "completed".to_string(),
            execution: "client".to_string(),
            tools: Vec::new(),
        },
        ToolPayload::Custom { .. } => ResponseInputItem::CustomToolCallOutput {
            call_id: invocation.call_id.clone(),
            name: None,
            output: codex_protocol::models::FunctionCallOutputPayload {
                body: codex_protocol::models::FunctionCallOutputBody::Text(message),
                success: Some(false),
            },
        },
        ToolPayload::Function { .. } => ResponseInputItem::FunctionCallOutput {
            call_id: invocation.call_id.clone(),
            output: codex_protocol::models::FunctionCallOutputPayload {
                body: codex_protocol::models::FunctionCallOutputBody::Text(message),
                success: Some(false),
            },
        },
    }
}

fn tool_call_metadata(
    invocation: &ToolInvocation,
    started_at: NaiveDateTime,
    ended_at: Option<NaiveDateTime>,
    status: Option<String>,
    error: Option<String>,
    model_request_trace_context: Option<&ModelRequestTraceContext>,
    request: Option<&TraceId>,
) -> ToolCallMetadata {
    ToolCallMetadata {
        call_id: Some(invocation.call_id.clone()),
        source: Some(tool_call_source_value(&invocation.source)),
        approval_policy: Some(invocation.turn.approval_policy.to_string()),
        sandbox: Some(json!({
            "sandbox": permission_profile_sandbox_tag(
                &invocation.turn.permission_profile,
                invocation.turn.windows_sandbox_level,
                invocation.turn.network.is_some(),
            ),
            "sandbox_policy": permission_profile_policy_tag(
                &invocation.turn.permission_profile,
                #[allow(deprecated)]
                invocation.turn.cwd.as_path(),
            ),
            "network_enabled": invocation.turn.network.is_some(),
        })),
        started_at: Some(started_at),
        ended_at,
        status,
        error,
        model_request_id: model_request_trace_context.map(|request| request.id.clone()),
        model_request_path: model_request_trace_context.map(|request| request.path.clone()),
        request_trace_id: request.map(|request| request.id.clone()),
        request_trace_path: request.map(|request| request.path.clone()),
        ..Default::default()
    }
}

fn tool_call_source_value(source: &crate::tools::context::ToolCallSource) -> Value {
    match source {
        crate::tools::context::ToolCallSource::Direct => json!({ "type": "direct" }),
        crate::tools::context::ToolCallSource::CodeMode {
            cell_id,
            runtime_tool_call_id,
        } => json!({
            "type": "code_mode",
            "cell_id": cell_id,
            "runtime_tool_call_id": runtime_tool_call_id,
        }),
    }
}

#[derive(Serialize)]
struct ToolCallResultTracePayload {
    model_visible_result: ResponseInputItem,
    log_preview: String,
}

fn tool_call_request_payload(payload: &ToolPayload) -> Value {
    match payload {
        ToolPayload::Function { arguments } => json!({
            "type": "function",
            "arguments": arguments,
        }),
        ToolPayload::Custom { input } => json!({
            "type": "custom",
            "input": input,
        }),
        ToolPayload::ToolSearch { arguments } => json!({
            "type": "tool_search",
            "arguments": json_value(arguments),
        }),
    }
}

fn json_value(value: &impl Serialize) -> Value {
    serde_json::to_value(value).unwrap_or_else(|err| {
        json!({
            "trace_serialization_error": err.to_string(),
        })
    })
}
