use agentic_core::executor::ExecuteRequest;
use agentic_core::types::io::OutputItem;
use agentic_core::types::request_response::{RequestPayload, ResponsePayload};
use serde_json::{Value, json};

mod support;

const CASSETTE_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/cassettes/reasoning/responses");
const GATEWAY_MODEL: &str = "gpt-5.6";
const GATEWAY_MODEL_SLUG: &str = "gpt-5.6";
const OPENAI_MODEL: &str = "gpt-5.6";
const OPENAI_MODEL_SLUG: &str = "gpt-5.6";
const PROMPT: &str = "Determine whether 47 is the unique two-digit positive integer whose digits sum to 11 and whose reversal is 27 larger. Analyze the constraints, then reply with exactly one word: VALID or INVALID.";

fn expected_reasoning() -> Value {
    json!({"effort": "high", "summary": "detailed"})
}

fn load_recorded_pair(streaming: bool) -> (support::Cassette, support::Cassette) {
    let mode = if streaming { "streaming" } else { "nonstreaming" };
    let openai = support::load_cassette(&format!(
        "{CASSETTE_DIR}/reasoning-openai-reference-{OPENAI_MODEL_SLUG}-{mode}.yaml"
    ));
    let gateway = support::load_cassette(&format!(
        "{CASSETTE_DIR}/reasoning-gateway-{GATEWAY_MODEL_SLUG}-{mode}.yaml"
    ));
    (openai, gateway)
}

fn terminal_response(turn: &support::Turn) -> Value {
    if let Some(body) = &turn.response.body {
        return body.clone();
    }

    support::recorded_named_sse_events(turn)
        .into_iter()
        .rev()
        .find_map(|event| {
            (event["type"] == "response.completed")
                .then(|| event.get("response").cloned())
                .flatten()
        })
        .expect("streaming cassette should contain response.completed")
}

fn assert_request_contract(openai: &support::Cassette, gateway: &support::Cassette, streaming: bool) {
    assert_eq!(openai.turns.len(), 1);
    assert_eq!(gateway.turns.len(), 1);
    let openai = &openai.turns[0].request;
    let gateway = &gateway.turns[0].request;

    assert_eq!(openai.path, "/v1/responses");
    assert_eq!(gateway.path, openai.path);
    assert_eq!(openai.body.model.as_deref(), Some(OPENAI_MODEL));
    assert_eq!(gateway.body.model.as_deref(), Some(GATEWAY_MODEL));
    assert_eq!(openai.body.input, PROMPT);
    assert_eq!(gateway.body.input, openai.body.input);
    assert!(openai.body.store);
    assert_eq!(gateway.body.store, openai.body.store);
    assert_eq!(openai.body.stream, streaming);
    assert_eq!(gateway.body.stream, openai.body.stream);
    assert_eq!(openai.body.max_output_tokens, Some(2048));
    assert_eq!(gateway.body.max_output_tokens, openai.body.max_output_tokens);
    assert_eq!(openai.body.reasoning, Some(expected_reasoning()));
    assert_eq!(gateway.body.reasoning, openai.body.reasoning);
}

fn assert_terminal_contract(turn: &support::Turn) {
    let response = terminal_response(turn);
    assert_eq!(response["status"], "completed");
    let output = response["output"]
        .as_array()
        .expect("completed response should contain output");
    let reasoning = output
        .iter()
        .find(|item| item["type"] == "reasoning")
        .expect("explicit reasoning request should produce a reasoning item");
    let has_reasoning_text = ["content", "summary"].into_iter().any(|field| {
        reasoning[field].as_array().is_some_and(|parts| {
            parts
                .iter()
                .any(|part| part["text"].as_str().is_some_and(|text| !text.is_empty()))
        })
    });
    let has_encrypted_reasoning = reasoning["encrypted_content"]
        .as_str()
        .is_some_and(|content| !content.is_empty());
    assert!(
        has_reasoning_text || has_encrypted_reasoning,
        "reasoning item should contain recorded reasoning content"
    );
    let message = output
        .iter()
        .find(|item| item["type"] == "message")
        .expect("completed response should contain a message");
    assert_eq!(message["status"], "completed");
    let text = message["content"]
        .as_array()
        .expect("message should contain content")
        .iter()
        .filter_map(|part| part["text"].as_str())
        .collect::<String>();
    assert_eq!(text.trim(), "VALID");
}

async fn replay_through_gateway(turn: &support::Turn) -> ResponsePayload {
    let fixture = support::TestFixture::new(&[turn]).await;
    let payload: RequestPayload = serde_json::from_value(json!({
        "model": turn.request.body.model,
        "input": turn.request.body.input,
        "store": turn.request.body.store,
        "stream": turn.request.body.stream,
        "max_output_tokens": turn.request.body.max_output_tokens,
        "reasoning": turn.request.body.reasoning,
    }))
    .expect("recorded request should satisfy the gateway request schema");
    let streaming = payload.stream;

    let result = ExecuteRequest::new(payload, fixture.exec_ctx.clone())
        .run()
        .await
        .expect("recorded response should replay through the gateway");
    let response = if streaming {
        support::collect_stream(result).await
    } else {
        support::unwrap_blocking(result)
    };

    let requests = fixture.request_bodies().await;
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0]["reasoning"], expected_reasoning());
    assert_eq!(requests[0]["stream"], streaming);
    response
}

fn assert_replayed_output(response: &ResponsePayload) {
    assert_eq!(response.status, "completed");
    assert!(
        response
            .output
            .iter()
            .any(|item| matches!(item, OutputItem::Reasoning(_))),
        "gateway replay should retain the reasoning item"
    );
    assert_eq!(support::output_text(response).trim(), "VALID");
}

#[tokio::test]
async fn recorded_nonstreaming_reasoning_matches_openai_contract() {
    let (openai, gateway) = load_recorded_pair(false);
    assert_request_contract(&openai, &gateway, false);
    assert_terminal_contract(&openai.turns[0]);
    assert_terminal_contract(&gateway.turns[0]);

    assert_replayed_output(&replay_through_gateway(&openai.turns[0]).await);
    assert_replayed_output(&replay_through_gateway(&gateway.turns[0]).await);
}

#[tokio::test]
async fn recorded_streaming_reasoning_matches_openai_contract() {
    let (openai, gateway) = load_recorded_pair(true);
    assert_request_contract(&openai, &gateway, true);
    assert_terminal_contract(&openai.turns[0]);
    assert_terminal_contract(&gateway.turns[0]);

    assert_replayed_output(&replay_through_gateway(&openai.turns[0]).await);
    assert_replayed_output(&replay_through_gateway(&gateway.turns[0]).await);
}
