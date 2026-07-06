use std::sync::Arc;

use crate::executor::accumulator::ResponseAccumulator;
use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::executor::inference::{call_inference, fetch_response_json};
use crate::executor::request::{ExecutionContext, RequestContext};
use crate::types::request_response::ResponsePayload;
use crate::utils::common::serialize_to_string;

pub(super) async fn fetch_blocking_payload(
    ctx: &RequestContext,
    exec_ctx: &ExecutionContext,
    auth: Option<&str>,
) -> ExecutorResult<ResponsePayload> {
    let url = exec_ctx.responses_url();
    // Non-streaming request: stream=false -> full JSON body -> from_json.
    let upstream_json =
        serialize_to_string(&ctx.enriched_request.to_upstream_request(false)).map_err(ExecutorError::JsonError)?;

    let body = fetch_response_json(upstream_json, &url, &exec_ctx.client, auth).await?;

    let acc = ResponseAccumulator::from_json(&body, ctx.conversation_id.as_deref())?;
    let mut payload = acc.finalize(
        &ctx.enriched_request.model,
        ctx.original_request.previous_response_id.as_deref(),
        ctx.original_request.instructions.as_deref(),
    );
    ctx.inject_ids(&mut payload);

    Ok(payload)
}

pub(super) async fn fetch_stream_payload(
    ctx: &RequestContext,
    exec_ctx: &ExecutionContext,
    auth: Option<&str>,
) -> ExecutorResult<ResponsePayload> {
    let url = exec_ctx.responses_url();
    let upstream_json =
        serialize_to_string(&ctx.enriched_request.to_upstream_request(true)).map_err(ExecutorError::JsonError)?;
    let line_stream = Box::pin(call_inference(
        upstream_json,
        url,
        Arc::clone(&exec_ctx.client),
        auth.map(str::to_owned),
        exec_ctx.streaming_timeout,
    ));
    let acc = ResponseAccumulator::from_stream(line_stream, ctx.conversation_id.as_deref()).await?;
    let mut payload = acc.finalize(
        &ctx.enriched_request.model,
        ctx.original_request.previous_response_id.as_deref(),
        ctx.original_request.instructions.as_deref(),
    );
    ctx.inject_ids(&mut payload);
    Ok(payload)
}
