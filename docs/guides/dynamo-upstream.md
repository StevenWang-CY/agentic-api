# Running Agentic API in front of NVIDIA Dynamo

[NVIDIA Dynamo](https://github.com/ai-dynamo/dynamo) is a distributed inference serving framework. Its frontend
exposes an OpenAI-compatible HTTP API, including `/v1/responses`, and routes requests to backend workers; the vLLM
worker (`python -m dynamo.vllm`) runs the same vLLM engine Agentic API already targets. This guide records how to put
Agentic API in front of a Dynamo deployment serving a model you already run with vLLM, and what changes compared to
pointing the gateway at `vllm serve` directly.

The short version: nothing in Agentic API needs to change. Start the gateway with `--llm-api-base` pointing at the
Dynamo frontend and it works, including stateful `previous_response_id` chaining and client-executed function tools.

## What Dynamo does and does not provide

| Capability | Dynamo frontend | Agentic API adds |
|---|---|---|
| `POST /v1/responses`, `/v1/chat/completions`, `/v1/models`, `/health` | ✅ | — |
| Reasoning / tool-call parsing | ✅ via `--dyn-reasoning-parser` / `--dyn-tool-call-parser` on the worker | — |
| `previous_response_id` | ❌ returns `501 Not Implemented` (`Validation: previous_response_id is not supported.`) | ✅ Stores every response and rehydrates the full item history on each turn, so the upstream call is stateless |
| Gateway-executed built-in tools (web search, MCP), background execution, WebSocket transport | ❌ | ✅ |

Because Dynamo rejects `previous_response_id`, the gateway never forwards it. The second turn of a conversation reaches
Dynamo as one `input` array containing the earlier user message, the stored assistant message, and the new user
message. The replay tests in `crates/agentic-server-core/tests/dynamo_cassette_test.rs` assert exactly that shape.

## 1. Install Dynamo next to your existing vLLM

Dynamo publishes wheels on PyPI. The `[vllm]` extra pins its own vLLM version, so install it into a separate virtual
environment rather than the one running your existing `vllm serve`:

```bash
mkdir -p ~/dev/dynamo && cd ~/dev/dynamo
uv venv --python 3.12 .venv
VIRTUAL_ENV=$PWD/.venv uv pip install --prerelease=allow "ai-dynamo[vllm]"
```

This was verified with `ai-dynamo==1.4.1` (`vllm==0.26.0`, `torch` cu130) on an aarch64 host with a single GB10 GPU.
No etcd or NATS is needed for a single-host setup when the components use file-based discovery.

## 2. Start the Dynamo frontend and a vLLM worker

Run each in its own terminal (or tmux window). The frontend below listens on port 8001 so it can coexist with a
`vllm serve` instance already on 8000.

```bash
# Frontend: OpenAI-compatible HTTP on :8001
.venv/bin/python -m dynamo.frontend --http-port 8001 --discovery-backend file

# Worker: serve a model you already have cached for vLLM
.venv/bin/python -m dynamo.vllm \
  --model openai/gpt-oss-20b \
  --discovery-backend file \
  --kv-events-config '{"enable_kv_cache_events": false}' \
  --dyn-reasoning-parser gpt_oss \
  --dyn-tool-call-parser harmony \
  --enforce-eager --max-model-len 32768 --gpu-memory-utilization 0.15
```

Flags worth knowing:

| Flag | Why |
|---|---|
| `--discovery-backend file` | Lets the frontend and worker find each other via `/tmp/dynamo_store_kv` instead of etcd. Pass it to both. |
| `--kv-events-config '{"enable_kv_cache_events": false}'` | Required for the vLLM worker without NATS. |
| `--dyn-reasoning-parser` / `--dyn-tool-call-parser` | The Dynamo *frontend* parses model output, not vLLM. vLLM's `--reasoning-parser` is ignored and `--tool-call-parser` / `--enable-auto-tool-choice` are rejected as unknown arguments. Without the `--dyn-*` flags, gpt-oss "analysis" text leaks into `content` and tool calls are returned as plain text. Use `gpt_oss` + `harmony` for gpt-oss models and `qwen3` + `qwen3_coder` / `hermes` for Qwen. |
| `--gpu-memory-utilization` | A fraction of *total* device memory that must fit in the memory currently free. On a unified-memory host sharing the GPU with another vLLM, size it to what is actually available or the engine fails at startup. |

Confirm the worker registered and parsing works:

```bash
curl -s localhost:8001/v1/models | jq '.data[].id'
curl -s localhost:8001/v1/chat/completions -H 'Content-Type: application/json' -d '{
  "model": "openai/gpt-oss-20b",
  "messages": [{"role": "user", "content": "Say hello in five words."}],
  "max_tokens": 500
}' | jq '.choices[0].message | {content, reasoning_content}'
```

`content` should hold the answer and `reasoning_content` the chain of thought. If the answer starts with `analysis`, the
worker is missing `--dyn-reasoning-parser`.

## 3. Start Agentic API against the Dynamo frontend

```bash
cargo build -p agentic-server --bins
./target/debug/agentic-server --llm-api-base http://127.0.0.1:8001 --gateway-port 9001
```

The readiness probe uses Dynamo's `/health`, so no `--skip-llm-ready-check` is needed. The harness CLI works the same
way: `./target/debug/agentic run codex --upstream http://127.0.0.1:8001`.

## 4. Verify a stateful conversation and a tool call

```bash
R1=$(curl -s localhost:9001/v1/responses -H 'Content-Type: application/json' -d '{
  "model": "openai/gpt-oss-20b",
  "input": "Remember the word APPLE. Just say: OK",
  "max_output_tokens": 2048
}')
echo "$R1" | jq -r '.output[] | select(.type=="message") | .content[0].text'   # OK

curl -s localhost:9001/v1/responses -H 'Content-Type: application/json' -d "{
  \"model\": \"openai/gpt-oss-20b\",
  \"input\": \"What word did I ask you to remember? Reply with just the word.\",
  \"previous_response_id\": \"$(echo "$R1" | jq -r .id)\",
  \"max_output_tokens\": 2048
}" | jq -r '.output[] | select(.type=="message") | .content[0].text'           # APPLE

curl -s localhost:9001/v1/responses -H 'Content-Type: application/json' -d '{
  "model": "openai/gpt-oss-20b",
  "input": "What is the current NVIDIA stock price? Use the tool.",
  "max_output_tokens": 2048,
  "tools": [{"type": "function", "name": "get_stock_price",
             "description": "Get the latest stock price for a ticker symbol",
             "parameters": {"type": "object", "properties": {"ticker": {"type": "string"}}, "required": ["ticker"]}}]
}' | jq '.output[] | select(.type=="function_call") | {name, arguments}'
```

Expected: `OK`, then `APPLE`, then a `get_stock_price` call with `{"ticker":"NVDA", ...}`.

Sending the second request straight to Dynamo (port 8001) instead of the gateway fails with `501`; that difference is
the value the gateway adds. The third request exercises a client-executed function tool: Dynamo returns the function
call and the application runs it.

## Recorded cassettes and CI

The interactions above are recorded in `crates/agentic-server-core/tests/cassettes/dynamo/` and replayed by
`tests/dynamo_cassette_test.rs` on every `cargo test`, so CI covers the Dynamo upstream without a GPU. To re-record
against a live Dynamo (for example after a Dynamo release changes the response shape):

```bash
cd crates/agentic-server-core
DYNAMO_URL=http://127.0.0.1:8001 MODEL=openai/gpt-oss-20b \
  bash tests/cassettes/record_dynamo_cassettes.sh
```

The script records the second stateful turn from the hydrated item history the gateway would send (built from turn
1's recorded assistant message), because the recorder's own `previous_response_id` chaining cannot be used against a
stateless upstream.

## Troubleshooting

| Symptom | Cause / fix |
|---|---|
| `unrecognized arguments: --tool-call-parser` | Use `--dyn-tool-call-parser` on the worker. |
| Answer text begins with `analysis…assistantfinal…` | Add `--dyn-reasoning-parser gpt_oss` (or the parser for your model). |
| `Free memory on device … is less than desired GPU memory utilization` | Lower `--gpu-memory-utilization`; it is a fraction of total memory. |
| `CUDA error: out of memory` right after restarting a worker | A previous `dynamo.vllm` process is still alive and holding memory; `pkill -f "python -m dynamo.vllm"` before relaunching. Closing its terminal or tmux window does not kill it. |
| `501 Validation: previous_response_id is not supported.` | You are calling Dynamo directly. Send the request to the gateway. |
