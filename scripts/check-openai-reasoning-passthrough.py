"""Exercise real gateway HTTP ingress -> OpenAI provider -> recording mock.

Run in the development container:
  python3 scripts/check-openai-reasoning-passthrough.py --output /tmp/reasoning.json
The mock verifies transport only; its synthetic reasoning is not model inference.
"""
import argparse
import http.server
import json
from pathlib import Path
import socket
import subprocess
import tempfile
import threading
import time
import urllib.error
import urllib.request


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    repo = Path(__file__).resolve().parents[1]
    received = []

    class MockModel(http.server.BaseHTTPRequestHandler):
        def log_message(self, *args):
            pass

        def do_POST(self):
            body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            received.append({"path": self.path, "body": body})
            delta = {"content": "mock answer"}
            if body.get("reasoning_effort") in ("low", "medium", "high") or body.get(
                "chat_template_kwargs", {}
            ).get("enable_thinking") is True:
                delta["reasoning_content"] = "synthetic mock reasoning"
            if body.get("stream"):
                payload = {
                    "id": "chatcmpl-reasoning-probe", "object": "chat.completion.chunk",
                    "created": 1700000000, "model": "mock-backend-model",
                    "choices": [{"index": 0, "delta": delta, "finish_reason": None}],
                }
                end = {**payload, "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}
                raw = ("data: " + json.dumps(payload) + "\n\ndata: " + json.dumps(end)
                       + "\n\ndata: [DONE]\n\n").encode()
                content_type = "text/event-stream"
            else:
                raw = json.dumps({
                    "id": "chatcmpl-reasoning-probe", "object": "chat.completion",
                    "created": 1700000000, "model": "mock-backend-model",
                    "choices": [{"index": 0, "message": {"role": "assistant", **delta},
                                 "finish_reason": "stop"}],
                    "usage": {"prompt_tokens": 1, "completion_tokens": 2, "total_tokens": 3},
                }).encode()
                content_type = "application/json"
            self.send_response(200)
            self.send_header("Content-Type", content_type)
            self.send_header("Content-Length", str(len(raw)))
            self.end_headers()
            self.wfile.write(raw)

    model = http.server.ThreadingHTTPServer(("127.0.0.1", 0), MockModel)
    threading.Thread(target=model.serve_forever, daemon=True).start()
    # Reserve the gateway port until immediately before launching it.
    reservation = socket.socket()
    reservation.bind(("127.0.0.1", 0))
    port = reservation.getsockname()[1]
    base = f"http://127.0.0.1:{port}"
    process = None
    results = []
    try:
        with tempfile.TemporaryDirectory(prefix="boom-openai-reasoning-") as directory:
            root = Path(directory)
            config = {
                "general_settings": {"master_key": "local-reasoning-probe"},
                "server": {"host": "127.0.0.1", "port": port, "workers": 2},
                "logging": {"enabled": False, "console": True},
                "model_list": [{"model_name": "reasoning-probe", "litellm_params": {
                    "model": "openai/mock-backend-model",
                    "api_base": f"http://127.0.0.1:{model.server_port}/v1",
                    "timeout": 10,
                }}],
            }
            config_path = root / "config.yaml"
            config_path.write_text(json.dumps(config), encoding="utf-8")
            with (root / "gateway.log").open("w") as log:
                reservation.close()
                process = subprocess.Popen(
                    [str(repo / "target/debug/boom-gateway"), "--config", str(config_path)],
                    cwd=root, stdout=log, stderr=subprocess.STDOUT,
                )
                try:
                    deadline = time.monotonic() + 30
                    while True:
                        if process.poll() is not None:
                            raise AssertionError((root / "gateway.log").read_text())
                        try:
                            with urllib.request.urlopen(base + "/health/ready", timeout=1) as response:
                                if response.status == 200:
                                    break
                        except (urllib.error.URLError, TimeoutError):
                            pass
                        if time.monotonic() > deadline:
                            raise AssertionError("isolated gateway startup timed out")
                        time.sleep(0.1)

                    variants = [("omitted", {})] + [
                        (effort, {"reasoning_effort": effort})
                        for effort in ("low", "medium", "high", "none")
                    ] + [("backend_extension", {"chat_template_kwargs": {
                        "enable_thinking": True, "probe_nested": {"preserve": [1, False, "x"]}
                    }})]
                    for stream in (False, True):
                        for name, extra in variants:
                            body = {"model": "reasoning-probe", "messages": [{
                                "role": "user", "content": "local reasoning passthrough probe"
                            }], "stream": stream, **extra}
                            before = len(received)
                            request = urllib.request.Request(
                                base + "/v1/chat/completions", data=json.dumps(body).encode(),
                                headers={"Authorization": "Bearer local-reasoning-probe",
                                         "Content-Type": "application/json"},
                            )
                            with urllib.request.urlopen(request, timeout=15) as response:
                                status = response.status
                                raw = response.read().decode()
                            assert len(received) == before + 1, received[before:]
                            observed = received[-1]
                            upstream = observed["body"]
                            assert observed["path"] == "/v1/chat/completions", observed
                            assert upstream["model"] == "mock-backend-model", upstream
                            assert upstream["messages"] == body["messages"], upstream
                            assert upstream["stream"] == stream, upstream
                            for key in ("reasoning_effort", "chat_template_kwargs"):
                                assert (key in upstream) == (key in body), upstream
                                if key in body:
                                    assert upstream[key] == body[key], upstream
                            assert "extra" not in upstream and "extra_body" not in upstream, upstream
                            if stream:
                                events = [line[5:].strip() for line in raw.splitlines()
                                          if line.startswith("data:")]
                                assert "[DONE]" in events, raw
                                chunks = [json.loads(event) for event in events if event != "[DONE]"]
                                assert not any("error" in chunk for chunk in chunks), raw
                                returned = [choice["delta"] for chunk in chunks
                                            for choice in chunk.get("choices", [])]
                            else:
                                returned = [json.loads(raw)["choices"][0]["message"]]
                            assert "".join(item.get("content") or "" for item in returned) == "mock answer", raw
                            thinking = name in ("low", "medium", "high", "backend_extension")
                            reasoning = "".join(item.get("reasoning_content") or "" for item in returned)
                            assert reasoning == ("synthetic mock reasoning" if thinking else ""), raw
                            result = {"case": name, "stream": stream, "http_status": status,
                                      "passed": True, "client_body": body,
                                      "upstream_request": observed,
                                      "synthetic_reasoning_returned": bool(reasoning)}
                            results.append(result)
                            print(f"PASS {name} stream={stream} HTTP={status}", flush=True)
                finally:
                    process.terminate()
                    try:
                        process.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.wait(timeout=5)
    finally:
        reservation.close()
        model.shutdown()
        model.server_close()
    report = {"scope": "real gateway HTTP passthrough; synthetic backend, no real model inference",
              "passed": len(results), "total": 12, "cases": results}
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(report, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    assert len(results) == 12, report
    print("All 12 OpenAI reasoning passthrough scenarios passed.")


if __name__ == "__main__":
    main()
