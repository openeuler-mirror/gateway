//! Minimal OpenAI-compatible mock upstream for local agent runs (no real
//! inference needed). Serves exactly the behaviors `agent` exercises:
//!
//! - non-stream, no prior tool result → emits a `get_current_time` tool call
//! - non-stream, tool result present   → echoes it as the final answer
//! - stream                            → plain text deltas + usage + [DONE]
//!
//! Usage: mock-upstream [port]   (default 18099)

use serde_json::{json, Value};
use tiny_http::{Header, Method, Response, Server};

fn main() {
    let port: u16 = std::env::args()
        .nth(1)
        .and_then(|p| p.parse().ok())
        .unwrap_or(18099);
    let server = Server::http(("127.0.0.1", port)).expect("bind mock upstream");
    eprintln!("mock upstream listening on 127.0.0.1:{port}");

    for mut request in server.incoming_requests() {
        // P2: native Responses endpoint (vLLM-class passthrough deployments).
        if *request.method() == Method::Post && request.url() == "/v1/responses" {
            let mut body = String::new();
            if request.as_reader().read_to_string(&mut body).is_err() {
                let _ = request.respond(Response::from_string("bad body").with_status_code(400));
                continue;
            }
            let req: Value = match serde_json::from_str(&body) {
                Ok(v) => v,
                Err(_) => {
                    let _ = request.respond(Response::from_string("bad json").with_status_code(400));
                    continue;
                }
            };
            let response = native_responses_endpoint(&req);
            let _ = request.respond(response);
            continue;
        }
        if *request.method() != Method::Post || request.url() != "/v1/chat/completions" {
            let _ = request.respond(Response::from_string("not found").with_status_code(404));
            continue;
        }
        let mut body = String::new();
        if request.as_reader().read_to_string(&mut body).is_err() {
            let _ = request.respond(Response::from_string("bad body").with_status_code(400));
            continue;
        }
        let req: Value = match serde_json::from_str(&body) {
            Ok(v) => v,
            Err(_) => {
                let _ = request.respond(Response::from_string("bad json").with_status_code(400));
                continue;
            }
        };

        let response = if req["stream"].as_bool() == Some(true) {
            sse_response()
        } else if has_tool_result(&req) {
            tool_echo_response(&req)
        } else if let Some(name) = first_tool_name(&req) {
            tool_call_response(&name)
        } else {
            plain_text_response(&req)
        };
        let _ = request.respond(response);
    }
}

/// The first declared tool name — drives which tool call the mock emits.
/// (Gateway wraps `type:"custom"` declarations as functions upstream, so
/// custom tools arrive here as ordinary function tools.)
fn first_tool_name(req: &Value) -> Option<String> {
    req["tools"]
        .as_array()?
        .iter()
        .find_map(|t| t["function"]["name"].as_str().map(String::from))
}

/// Only declare a tool call when the client actually offered tools — a plain
/// prompt (like the agent's non-stream smoke) gets a plain answer.
fn has_tools(req: &Value) -> bool {
    req["tools"].as_array().is_some_and(|t| !t.is_empty())
}

/// Echo the FIRST user message. Gives the agent's continuation check its
/// discriminating power: with previous_response_id honored, turn 2's upstream
/// history starts at turn 1's user message; without it, only turn 2's message
/// is visible.
fn plain_text_response(req: &Value) -> Response<std::io::Cursor<Vec<u8>>> {
    let first_user = req["messages"]
        .as_array()
        .and_then(|msgs| {
            msgs.iter()
                .find(|m| m["role"].as_str() == Some("user"))
                .map(|m| m["content"].clone())
        })
        .and_then(|c| c.as_str().map(String::from))
        .unwrap_or_else(|| "OK".to_string());
    json_response(json!({
        "id": "chatcmpl-mock-0",
        "object": "chat.completion",
        "created": 0,
        "model": "mock-model",
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": format!("You said: {first_user}") },
            "finish_reason": "stop"
        }],
        "usage": { "prompt_tokens": 5, "completion_tokens": 2, "total_tokens": 7 }
    }))
}

/// First turn detector: any `role:"tool"` message in the history means the
/// agent already ran the tool — answer with its result.
fn has_tool_result(req: &Value) -> bool {
    req["messages"]
        .as_array()
        .map(|msgs| {
            msgs.iter()
                .any(|m| m["role"].as_str() == Some("tool"))
        })
        .unwrap_or(false)
}

fn tool_result_text(req: &Value) -> String {
    req["messages"]
        .as_array()
        .and_then(|msgs| {
            msgs.iter()
                .find(|m| m["role"].as_str() == Some("tool"))
                .and_then(|m| m["content"].as_str())
                .map(String::from)
        })
        .unwrap_or_else(|| "<missing>".to_string())
}

fn tool_call_response(name: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    // Custom-wrapped tools carry {"input":"…"}; plain function tools get {}.
    let arguments = if name == "apply_patch" {
        json!({ "input": "*** Begin Patch\n*** Add File: demo.txt\n+hello from custom tool" })
            .to_string()
    } else {
        "{}".to_string()
    };
    json_response(json!({
        "id": "chatcmpl-mock-1",
        "object": "chat.completion",
        "created": 0,
        "model": "mock-model",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call_mock_1",
                    "type": "function",
                    "function": { "name": name, "arguments": arguments }
                }]
            },
            "finish_reason": "tool_calls"
        }],
        "usage": { "prompt_tokens": 11, "completion_tokens": 3, "total_tokens": 14 }
    }))
}

fn tool_echo_response(req: &Value) -> Response<std::io::Cursor<Vec<u8>>> {
    json_response(json!({
        "id": "chatcmpl-mock-2",
        "object": "chat.completion",
        "created": 0,
        "model": "mock-model",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": format!("The current time is {}.", tool_result_text(req))
            },
            "finish_reason": "stop"
        }],
        "usage": { "prompt_tokens": 13, "completion_tokens": 6, "total_tokens": 19 }
    }))
}

fn sse_response() -> Response<std::io::Cursor<Vec<u8>>> {
    let chunk = |c: Value| format!("data: {}\n\n", c);
    let mut body = String::new();
    for text in ["Hel", "lo from mock"] {
        body.push_str(&chunk(json!({
            "id": "chatcmpl-mock-s",
            "object": "chat.completion.chunk",
            "created": 0,
            "model": "mock-model",
            "choices": [{ "index": 0, "delta": { "content": text }, "finish_reason": null }]
        })));
    }
    body.push_str(&chunk(json!({
        "id": "chatcmpl-mock-s",
        "object": "chat.completion.chunk",
        "created": 0,
        "model": "mock-model",
        "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }],
        "usage": { "prompt_tokens": 9, "completion_tokens": 3, "total_tokens": 12 }
    })));
    body.push_str("data: [DONE]\n\n");

    let mut response = Response::from_string(body).with_status_code(200);
    response.add_header(
        Header::from_bytes(&b"Content-Type"[..], &b"text/event-stream"[..]).expect("header"),
    );
    response
}

fn json_response(v: Value) -> Response<std::io::Cursor<Vec<u8>>> {
    let mut response = Response::from_string(v.to_string()).with_status_code(200);
    response.add_header(
        Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).expect("header"),
    );
    response
}

// ─────────────────────────────────────────────────────────────
// P2 native /v1/responses endpoint (vLLM-class passthrough shape)
// ─────────────────────────────────────────────────────────────

/// Native /v1/responses behavior tree (vLLM-class):
/// 1. input carries a tool OUTPUT item → echo it as the final message
/// 2. tools declared (no output yet) → emit a tool call — `type:"custom"`
///    declarations get a custom_tool_call item (native fidelity, no wrapping)
/// 3. input carries a `reasoning` item → NATIVE_ROUND2 items=N (history echo)
/// 4. plain probe → native reasoning item + NATIVE_PASSTHROUGH marker
fn native_responses_endpoint(req: &Value) -> Response<std::io::Cursor<Vec<u8>>> {
    static UPSTREAM_SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = UPSTREAM_SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let upstream_id = format!("resp_upstream_{n}");

    let input_items = req["input"].as_array().cloned().unwrap_or_default();
    let msg_id = format!("msg_native_{n}");

    let message = |text: String| {
        json!({
            "id": msg_id, "type": "message", "role": "assistant", "status": "completed",
            "content": [{ "type": "output_text", "text": text, "annotations": [] }],
        })
    };

    let output: Vec<Value>;
    // 1. Tool result replay → echo.
    if let Some(result) = input_items.iter().rev().find_map(|i| {
        (i["type"] == "function_call_output" || i["type"] == "custom_tool_call_output")
            .then(|| i["output"].clone())
    }) {
        let text = result.as_str().unwrap_or_default().to_string();
        output = vec![message(text)];
    // 2. Tool declaration → tool call.
    } else if let Some(tool) = req["tools"].as_array().and_then(|t| t.first()).cloned() {
        let name = tool["name"].as_str().unwrap_or("tool").to_string();
        output = if tool["type"] == "custom" {
            vec![json!({
                "id": format!("ctc_native_{n}"), "type": "custom_tool_call",
                "status": "completed", "call_id": format!("call_native_{n}"),
                "name": name,
                "input": "*** Begin Patch\n*** Add File: demo.txt\n+hello from native custom tool",
            })]
        } else {
            vec![json!({
                "id": format!("fc_native_{n}"), "type": "function_call",
                "status": "completed", "call_id": format!("call_native_{n}"),
                "name": name, "arguments": "{}",
            })]
        };
    // 3. Reasoning replay → history echo.
    } else if input_items.iter().any(|i| i["type"] == "reasoning") {
        output = vec![message(format!("NATIVE_ROUND2 items={}", input_items.len()))];
    // 4. Plain probe.
    } else {
        output = vec![
            json!({
                "id": format!("rs_native_{n}"), "type": "reasoning",
                "summary": [], "encrypted_content": "gAAAAABopaque-native-reasoning",
            }),
            message("NATIVE_PASSTHROUGH ok".to_string()),
        ];
    }

    let response_obj = json!({
        "id": upstream_id,
        "object": "response",
        "created_at": 0,
        "status": "completed",
        "model": req["model"].clone(),
        "output": output,
        "usage": { "input_tokens": 12, "output_tokens": 4, "total_tokens": 16 },
    });

    if req["stream"].as_bool() == Some(true) {
        sse_native_events(response_obj)
    } else {
        json_response(response_obj)
    }
}

/// Full native event sequence (the wire shape a vLLM-class upstream emits;
/// ids are upstream's — the gateway rewrites the top-level response id,
/// never item ids). Message items get the complete added → part → delta →
/// done lifecycle the stream contract requires.
fn sse_native_events(response_obj: Value) -> Response<std::io::Cursor<Vec<u8>>> {
    use std::fmt::Write as _;
    let mut seq = 0usize;
    let mut body = String::new();
    let mut emit = |body: &mut String, ty: &str, extra: Value| {
        let mut v = json!({ "type": ty, "sequence_number": seq });
        seq += 1;
        if let (Some(dst), Some(src)) = (v.as_object_mut(), extra.as_object()) {
            for (k, val) in src {
                dst.insert(k.clone(), val.clone());
            }
        }
        let _ = writeln!(body, "event: {ty}");
        let _ = writeln!(body, "data: {v}");
        body.push('\n');
    };

    let shell = |status: &str| {
        json!({ "response": {
            "id": response_obj["id"], "object": "response", "status": status,
            "model": response_obj["model"], "output": [], "usage": null,
        }})
    };
    emit(&mut body, "response.created", shell("in_progress"));
    emit(&mut body, "response.in_progress", shell("in_progress"));

    for (index, item) in response_obj["output"].as_array().cloned().unwrap_or_default()
        .into_iter()
        .enumerate()
    {
        let item_id = item["id"].as_str().unwrap_or_default().to_string();
        let index = index as u32;
        emit(&mut body, "response.output_item.added",
            json!({ "output_index": index, "item": item }));
        if item["type"] == "message" {
            let text = item["content"][0]["text"].as_str().unwrap_or_default().to_string();
            emit(&mut body, "response.content_part.added",
                json!({ "item_id": item_id, "output_index": index, "content_index": 0,
                        "part": { "type": "output_text", "text": "", "annotations": [] } }));
            let mid = text.len() / 2;
            for part in [(&text[..mid]), (&text[mid..])] {
                emit(&mut body, "response.output_text.delta",
                    json!({ "item_id": item_id, "output_index": index, "content_index": 0,
                            "delta": part }));
            }
            emit(&mut body, "response.output_text.done",
                json!({ "item_id": item_id, "output_index": index, "content_index": 0,
                        "text": text }));
            emit(&mut body, "response.content_part.done",
                json!({ "item_id": item_id, "output_index": index, "content_index": 0,
                        "part": item["content"][0] }));
        }
        emit(&mut body, "response.output_item.done",
            json!({ "output_index": index, "item": item }));
    }

    emit(&mut body, "response.completed", json!({ "response": response_obj }));

    let mut response = Response::from_string(body).with_status_code(200);
    response.add_header(
        Header::from_bytes(&b"Content-Type"[..], &b"text/event-stream"[..]).expect("header"),
    );
    response
}
