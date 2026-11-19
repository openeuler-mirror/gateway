"""DB-backed integration test of per-deployment defaults and Dashboard hot edits.

Requires a dedicated, pre-created PostgreSQL database named boom_defaults_test_*.
Uses DATABASE_URL from the dev container, replacing only the database name.
Never edits existing development deployments. All model output is synthetic.
"""
import argparse
import concurrent.futures
import http.cookiejar
import http.server
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import threading
import time
import urllib.error
import urllib.parse
import urllib.request


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--database-name", required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    assert args.database_name.startswith("boom_defaults_test_")
    parsed = urllib.parse.urlsplit(os.environ["DATABASE_URL"])
    db_url = urllib.parse.urlunsplit(parsed._replace(path="/" + args.database_name))
    repo = Path(__file__).resolve().parents[1]
    captured = []
    checks = []
    slow_started, release_slow = threading.Event(), threading.Event()
    lock = threading.Lock()

    def passed(name, **evidence):
        checks.append({"name": name, "passed": True, **evidence})
        print("PASS " + name, flush=True)

    class Backend(http.server.BaseHTTPRequestHandler):
        def log_message(self, *args):
            pass

        def do_POST(self):
            body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            with lock:
                captured.append({"path": self.path, "body": body})
            if body["messages"][0]["content"] == "slow":
                slow_started.set()
                assert release_slow.wait(15), "slow request was not released"
            # A strict backend deliberately rejects the unsupported reasoning field.
            if self.path.startswith("/strict/") and "reasoning_effort" in body:
                self.send_response(400)
                self.end_headers()
                self.wfile.write(b'{"error":{"message":"unsupported reasoning_effort"}}')
                return
            if body.get("stream"):
                event = {"id": "probe", "object": "chat.completion.chunk", "created": 0,
                         "model": "backend", "choices": [{"index": 0,
                         "delta": {"content": "ok"}, "finish_reason": None}]}
                end = {**event, "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}
                raw = ("data: " + json.dumps(event) + "\n\ndata: " + json.dumps(end)
                       + "\n\ndata: [DONE]\n\n").encode()
                content_type = "text/event-stream"
            else:
                raw = json.dumps({"id": "probe", "object": "chat.completion", "created": 0,
                    "model": "backend", "choices": [{"index": 0,
                    "message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}],
                    "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}}).encode()
                content_type = "application/json"
            self.send_response(200)
            self.send_header("Content-Type", content_type)
            self.send_header("Content-Length", str(len(raw)))
            self.end_headers()
            self.wfile.write(raw)

    backend = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Backend)
    threading.Thread(target=backend.serve_forever, daemon=True).start()
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        port = sock.getsockname()[1]
    base = f"http://127.0.0.1:{port}"
    cookie_jar = http.cookiejar.CookieJar()
    admin = urllib.request.build_opener(urllib.request.HTTPCookieProcessor(cookie_jar))
    process = None

    def request_http(path, body=None, method=None, admin_request=True):
        req = urllib.request.Request(base + path,
            data=None if body is None else json.dumps(body).encode(),
            method=method, headers={"Content-Type": "application/json",
                                   "Authorization": "Bearer defaults-test-master"})
        opener = admin if admin_request else urllib.request.build_opener()
        try:
            with opener.open(req, timeout=25) as response:
                return response.status, response.read().decode()
        except urllib.error.HTTPError as error:
            return error.code, error.read().decode()

    def admin_json(path, body=None, method=None):
        status, raw = request_http("/dashboard/api" + path, body, method)
        assert status == 200, (status, raw)
        value = json.loads(raw)
        assert "error" not in value, value
        return value

    def chat(model, extra=None, stream=False, content="probe"):
        body = {"model": model, "messages": [{"role": "user", "content": content}],
                "stream": stream, **(extra or {})}
        status, raw = request_http("/v1/chat/completions", body, admin_request=False)
        assert status == 200, (status, raw)
        if stream:
            assert "[DONE]" in raw and '"error"' not in raw, raw
        else:
            assert json.loads(raw)["choices"][0]["message"]["content"] == "ok", raw
        with lock:
            return next(item for item in reversed(captured)
                        if item["body"]["messages"] == body["messages"])

    def edit(row, defaults):
        body = {**row, "request_defaults": defaults, "api_key": None,
                "aws_access_key_id": None, "aws_secret_access_key": None}
        return admin_json("/admin/models/" + row["id"], body, "PUT")

    try:
        with tempfile.TemporaryDirectory(prefix="boom-defaults-live-") as directory:
            root = Path(directory)
            def entry(name, path, defaults):
                return {"model_name": name, "litellm_params": {
                    "model": "openai/backend", "api_base": f"http://127.0.0.1:{backend.server_port}/{path}/v1",
                    "request_defaults": defaults}}
            seed = [entry("standard", "standard", {"reasoning_effort": "medium", "temperature": 0.6}),
                    entry("qwen", "qwen", {"chat_template_kwargs": {"enable_thinking": True, "keep": 7}}),
                    entry("strict", "strict", {}),
                    entry("shared", "shared-standard", {"reasoning_effort": "low"}),
                    entry("shared", "shared-qwen", {"chat_template_kwargs": {"enable_thinking": True}})]
            config = {"general_settings": {"master_key": "defaults-test-master", "database_url": db_url},
                      "server": {"host": "127.0.0.1", "port": port, "workers": 2},
                      "logging": {"enabled": False, "console": True},
                      "router_settings": {"schedule_policy": "round-robin"},
                      "deployment_health_check": {"enabled": False}, "model_list": seed}
            config_path = root / "config.yaml"
            config_path.write_text(json.dumps(config))

            def start():
                nonlocal process
                log = (root / "gateway.log").open("a")
                process = subprocess.Popen([str(repo / "target/debug/boom-gateway"), "--config", str(config_path)],
                                           cwd=root, stdout=log, stderr=subprocess.STDOUT)
                log.close()
                deadline = time.monotonic() + 45
                while time.monotonic() < deadline:
                    if process.poll() is not None:
                        raise AssertionError((root / "gateway.log").read_text())
                    try:
                        if request_http("/health/ready", admin_request=False)[0] == 200:
                            admin_json("/auth/login", {"user_id": "admin", "api_key": "defaults-test-master"})
                            return
                    except urllib.error.URLError:
                        pass
                    time.sleep(0.1)
                raise AssertionError("gateway startup timed out")

            def stop():
                if process and process.poll() is None:
                    process.terminate()
                    try:
                        process.wait(timeout=8)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.wait(timeout=5)

            try:
                start()
                rows = admin_json("/admin/models")["models"]
                by_name = {r["model_name"]: r for r in rows}
                assert len(rows) == 5, rows
                assert by_name["standard"]["request_defaults"]["reasoning_effort"] == "medium"
                passed("YAML seed -> DB -> Dashboard list roundtrip")

                for stream in (False, True):
                    observed = chat("standard", stream=stream)["body"]
                    assert observed["reasoning_effort"] == "medium" and observed["temperature"] == 0.6, observed
                    passed(f"standard missing fields filled stream={stream}")
                    observed = chat("standard", {"reasoning_effort": "high", "temperature": 0}, stream)["body"]
                    assert observed["reasoning_effort"] == "high" and observed["temperature"] == 0, observed
                    passed(f"explicit client values preserved stream={stream}")
                    observed = chat("standard", {"reasoning_effort": "none"}, stream)["body"]
                    assert observed["reasoning_effort"] == "none", observed
                    passed(f"client none preserved stream={stream}")
                    observed = chat("qwen", {"chat_template_kwargs": {"custom": "client"}}, stream)["body"]
                    assert observed["chat_template_kwargs"] == {"custom": "client", "enable_thinking": True, "keep": 7}, observed
                    assert "reasoning_effort" not in observed, observed
                    passed(f"Qwen nested defaults merged stream={stream}")
                    observed = chat("qwen", {"chat_template_kwargs": {"enable_thinking": False}}, stream)["body"]
                    assert observed["chat_template_kwargs"] == {"enable_thinking": False, "keep": 7}, observed
                    passed(f"client Qwen false preserved stream={stream}")
                    observed = chat("qwen", {"reasoning_effort": "none"}, stream)["body"]
                    assert "enable_thinking" not in observed.get("chat_template_kwargs", {}), observed
                    passed(f"none suppresses conflicting Qwen default stream={stream}")
                    observed = chat("standard", {"chat_template_kwargs": {"enable_thinking": False}}, stream)["body"]
                    assert "reasoning_effort" not in observed, observed
                    passed(f"Qwen false suppresses conflicting standard default stream={stream}")
                    observed = chat("strict", stream=stream)["body"]
                    assert "reasoning_effort" not in observed and "chat_template_kwargs" not in observed, observed
                    passed(f"unconfigured strict backend unaffected stream={stream}")

                paths = set()
                for i in range(4):
                    observed = chat("shared", content=f"shared-{i}")
                    paths.add(observed["path"])
                    body = observed["body"]
                    if observed["path"].startswith("/shared-standard/"):
                        assert body["reasoning_effort"] == "low" and "chat_template_kwargs" not in body
                    else:
                        assert body["chat_template_kwargs"]["enable_thinking"] is True and "reasoning_effort" not in body
                assert len(paths) == 2, paths
                passed("same model name uses independent per-deployment defaults")

                original_pid = process.pid
                with concurrent.futures.ThreadPoolExecutor(max_workers=1) as pool:
                    old = pool.submit(chat, "standard", content="slow")
                    assert slow_started.wait(5)
                    edit(by_name["standard"], {"reasoning_effort": "high"})
                    new = chat("standard", content="after-edit")["body"]
                    assert new["reasoning_effort"] == "high", new
                    assert process.pid == original_pid and process.poll() is None
                    release_slow.set()
                    assert old.result()["body"]["reasoning_effort"] == "medium"
                passed("Dashboard PUT changes next request immediately; inflight keeps old defaults; same PID")
                assert chat("standard", stream=True)["body"]["reasoning_effort"] == "high"
                assert chat("qwen")["body"]["chat_template_kwargs"]["enable_thinking"] is True
                passed("live edit applies to streaming and leaves other deployment unchanged")

                edit(by_name["qwen"], {"chat_template_kwargs": {"enable_thinking": False}})
                assert chat("qwen")["body"]["chat_template_kwargs"]["enable_thinking"] is False
                passed("Dashboard live Qwen thinking true -> false")

                edit(by_name["standard"], {})
                assert "reasoning_effort" not in chat("standard")["body"]
                passed("empty object removes defaults immediately")

                body = {**by_name["standard"], "api_key": None, "request_defaults": {"model": "forbidden"}}
                status, raw = request_http("/dashboard/api/admin/models/" + body["id"], body, "PUT")
                assert status == 200 and "request_defaults" in json.loads(raw)["error"], raw
                assert "reasoning_effort" not in chat("standard")["body"]
                passed("reserved default rejected before DB mutation")
                body["request_defaults"] = []
                assert request_http("/dashboard/api/admin/models/" + body["id"], body, "PUT")[0] == 422
                passed("non-object defaults rejected")

                created = admin_json("/admin/models", {"model_name": "created", "litellm_model": "openai/backend",
                    "api_base": f"http://127.0.0.1:{backend.server_port}/created/v1",
                    "request_defaults": {"reasoning_effort": "low"}})
                assert chat("created")["body"]["reasoning_effort"] == "low"
                passed("Dashboard POST activates new deployment defaults immediately")
                bad = admin_json("/admin/models")["models"]
                assert next(r for r in bad if r["id"] == created["id"])["request_defaults"] == {"reasoning_effort": "low"}

                edit(by_name["standard"], {"reasoning_effort": "high"})
                snapshot = admin_json("/admin/config")
                entries = snapshot["model_list"]
                assert next(e for e in entries if e["model_name"] == "standard")["litellm_params"]["request_defaults"] == {"reasoning_effort": "high"}
                passed("config snapshot includes persisted request defaults")
                status, exported = request_http("/dashboard/api/admin/config/export")
                assert status == 200 and "request_defaults:" in exported and "reasoning_effort: high" in exported, exported[:1000]
                passed("YAML export retains request defaults")

                admin_json("/admin/config/reload", {}, "POST")
                assert chat("standard")["body"]["reasoning_effort"] == "high"
                passed("manual reload preserves DB-authoritative defaults over seed YAML")
                stop()
                start()
                assert chat("standard")["body"]["reasoning_effort"] == "high"
                assert chat("qwen")["body"]["chat_template_kwargs"]["enable_thinking"] is False
                assert chat("created")["body"]["reasoning_effort"] == "low"
                passed("restart retains DB edits and created deployment")
                admin_json("/admin/models/" + created["id"], method="DELETE")
                passed("Dashboard delete deployment with defaults")
            except Exception:
                print((root / "gateway.log").read_text()[-10000:], flush=True)
                raise
            finally:
                release_slow.set()
                stop()
    finally:
        release_slow.set()
        backend.shutdown()
        backend.server_close()
    report = {"scope": "real gateway + dedicated PostgreSQL + Dashboard APIs + recording mock backend",
              "passed": len(checks), "checks": checks, "upstream_requests": captured}
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2, ensure_ascii=False) + "\n")
    print(f"All {len(checks)} live default-parameter checks passed.", flush=True)


if __name__ == "__main__":
    main()
