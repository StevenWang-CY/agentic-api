"""Capture the gateway's forced-to-auto Messages requests to a real vLLM server.

Uses the existing recorder proxy; only the search service is deterministic.
See README.md for the server command and reproduction instructions.
"""

import argparse
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import httpx
import yaml

from record_cassette import _start_proxy, _stop_proxy


HERE = Path(__file__).resolve().parent
SEARCH_RESULT = {
    "results": {"web": [{"url": "https://example.com/proof", "title": "proof", "description": "SEARCH_PROOF_雪"}]}
}


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def wait_for_gateway(client, url, process, log):
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline and process.poll() is None:
        try:
            if client.get(url + "/health", timeout=1).is_success:
                return
        except httpx.TransportError:
            pass
        time.sleep(0.1)
    raise RuntimeError(log.read_text())


def record(binary, upstream, model, output, kind, stream):
    searches = []

    class Search(BaseHTTPRequestHandler):
        def log_message(self, *_args):
            pass

        def do_GET(self):
            assert self.path.startswith("/v1/search?")
            searches.append(self.path)
            body = json.dumps(SEARCH_RESULT, ensure_ascii=False).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

    search = ThreadingHTTPServer(("127.0.0.1", 0), Search)
    thread = threading.Thread(target=search.serve_forever, daemon=True)
    thread.start()
    try:
        with tempfile.TemporaryDirectory(prefix="messages-choice-") as directory:
            proxy_port, gateway_port = free_port(), free_port()
            while gateway_port == proxy_port:
                gateway_port = free_port()
            proxy = _start_proxy(output, upstream, proxy_port)
            try:
                env = dict(os.environ, AGENTIC_API_HOME=directory, YOU_API_KEY="local-test-key",
                           YOU_API_BASE_URL=f"http://127.0.0.1:{search.server_port}", RUST_LOG="error")
                for key in list(env):
                    if key.startswith(("OIDC_", "DATABASE_", "MESSAGES_GATEWAY_")) or key == "OPENAI_API_KEY":
                        env.pop(key)
                log = Path(directory) / "gateway.log"
                with log.open("w") as stdout:
                    process = subprocess.Popen(
                        [str(binary), "--llm-api-base", f"http://127.0.0.1:{proxy_port}",
                         "--gateway-host", "127.0.0.1", "--gateway-port", str(gateway_port),
                         "--skip-llm-ready-check"], env=env, stdout=stdout, stderr=subprocess.STDOUT)
                    try:
                        with httpx.Client(timeout=300) as client:
                            url = f"http://127.0.0.1:{gateway_port}"
                            wait_for_gateway(client, url, process, log)
                            request = {
                                "model": model, "max_tokens": 1024, "stream": stream, "temperature": 0,
                                "chat_template_kwargs": {"enable_thinking": False},
                                "messages": [{"role": "user", "content": (
                                    "Search for the verification token for the Messages tool-choice test. "
                                    "Then reply with only the token returned by web_search. "
                                    "Do not search again after receiving a result.")}],
                                "tools": json.loads((HERE / "messages/tools.json").read_text()),
                                # For `any`, name is an extension and must survive relaxation.
                                "tool_choice": {"type": kind, "name": "web_search",
                                                "disable_parallel_tool_use": kind == "tool"},
                            }
                            response = client.post(url + "/v1/messages", json=request)
                            response.raise_for_status()
                            if stream:
                                events = [json.loads(line[5:]) for line in response.text.splitlines()
                                          if line.startswith("data:")]
                                assert events[-1]["type"] == "message_stop", response.text
                                for event_type in ("message_start", "message_delta", "message_stop"):
                                    assert sum(e["type"] == event_type for e in events) == 1
                                assert any(e.get("delta", {}).get("stop_reason") == "end_turn" for e in events)
                                text = "".join(e.get("delta", {}).get("text", "") for e in events)
                            else:
                                message = response.json()
                                assert message["stop_reason"] == "end_turn", message
                                text = "".join(b.get("text", "") for b in message["content"])
                            assert "SEARCH_PROOF_雪" in text, text
                    finally:
                        process.terminate()
                        try:
                            process.wait(timeout=10)
                        except subprocess.TimeoutExpired:
                            process.kill()
                            process.wait()
                    assert process.returncode == 0, log.read_text()
            finally:
                _stop_proxy(proxy)
        turns = yaml.safe_load(output.read_text())["turns"]
        assert len(turns) == 2 and len(searches) == 1, (len(turns), searches)
        assert all(t["response"]["status_code"] == 200 for t in turns)
        assert turns[0]["request"]["body"] == request
        expected_choice = {"type": "auto", "disable_parallel_tool_use": kind == "tool"}
        if kind == "any":
            expected_choice["name"] = "web_search"
        assert turns[1]["request"]["body"]["tool_choice"] == expected_choice
        print(f"Validated {output.name}: two provider requests, one search, completed answer.")
    finally:
        search.shutdown()
        search.server_close()
        thread.join()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--vllm", required=True)
    parser.add_argument("--model", default="Qwen/Qwen3-4B")
    parser.add_argument("--output-dir", type=Path, default=HERE / "messages/tool-choice")
    args = parser.parse_args()
    for kind in ("any", "tool"):
        for stream in (False, True):
            suffix = "streaming" if stream else "nonstreaming"
            output = args.output_dir / f"messages-{kind}-{args.model.replace('/', '-')}-{suffix}.yaml"
            record(args.binary.resolve(), args.vllm, args.model, output, kind, stream)


if __name__ == "__main__":
    main()
