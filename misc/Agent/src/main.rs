//! Minimal Responses API test agent for Boom Gateway.
//!
//! Exercises `POST /v1/responses` end to end and asserts the wire contract
//! from Response.md §6.5:
//!
//! 1. non-stream  — Response object shape (resp_ id, message item, usage)
//! 2. stream      — created(seq 0) → … → terminal event, contiguous
//!                  sequence numbers, NO `data: [DONE]` sentinel
//! 3. guard       — `previous_response_id` must fail fast with 400 (P0 has
//!                  no response store; silent-ignore is the anti-pattern)
//! 4. tool loop   — a real agent turn: function_call → local execution →
//!                  function_call_output replay → final message
//!
//! Usage:
//!   agent --base-url http://127.0.0.1:18400 --key smoke-master-key --model test-model
//!
//! Exit code 0 = all checks passed.

use futures_util::StreamExt;
use reqwest::Client;
use serde_json::{json, Value};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

struct Config {
    base_url: String,
    key: String,
    model: String,
}

fn main() {
    let mut cfg = Config {
        base_url: "http://127.0.0.1:18400".to_string(),
        key: "smoke-master-key".to_string(),
        model: "test-model".to_string(),
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = || match args.next() {
            Some(v) => v,
            None => {
                eprintln!("missing value for {arg}");
                std::process::exit(2);
            }
        };
        match arg.as_str() {
            "--base-url" => cfg.base_url = value(),
            "--key" => cfg.key = value(),
            "--model" => cfg.model = value(),
            "-h" | "--help" => {
                println!(
                    "agent --base-url URL --key KEY --model MODEL\n  defaults: {} / {} / {}",
                    cfg.base_url, cfg.key, cfg.model
                );
                std::process::exit(0);
            }
            other => {
                eprintln!("unknown argument: {other}");
                std::process::exit(2);
            }
        }
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let results = rt.block_on(run_all_checks(&cfg));

    println!("\n=== agent summary ===");
    let mut failures = 0usize;
    for (name, result) in &results {
        match result {
            Ok(detail) => println!("PASS  {name:<28} {detail}"),
            Err(err) => {
                println!("FAIL  {name:<28} {err}");
                failures += 1;
            }
        }
    }
    if failures == 0 {
        println!("all {} checks passed", results.len());
    } else {
        println!("{failures}/{} checks failed", results.len());
    }
    std::process::exit(if failures == 0 { 0 } else { 1 });
}

async fn run_all_checks(cfg: &Config) -> Vec<(&'static str, Result<String, String>)> {
    // Generous per-attempt timeout, connect timeout separate: streams can be slow.
    let client = Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .build()
        .expect("reqwest client");
    vec![
        ("non_stream_response", check_non_stream(&client, cfg).await),
        ("stream_event_contract", check_stream(&client, cfg).await),
        ("guard_previous_response_id", check_guard(&client, cfg).await),
        ("agent_tool_loop", check_tool_loop(&client, cfg).await),
        ("store_continuation", check_store_continuation(&client, cfg).await),
        ("custom_tool_loop", check_custom_tool_loop(&client, cfg).await),
        ("passthrough_fidelity", check_passthrough_fidelity(&client, cfg).await),
    ]
}

async fn post_responses(
    client: &Client,
    cfg: &Config,
    body: &Value,
) -> Result<reqwest::Response, String> {
    let url = format!("{}/v1/responses", cfg.base_url);
    client
        .post(&url)
        .bearer_auth(&cfg.key)
        .json(body)
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))
}

// ---------------------------------------------------------------
// Check 1: non-stream Response object shape
// ---------------------------------------------------------------

async fn check_non_stream(client: &Client, cfg: &Config) -> Result<String, String> {
    let body = json!({
        "model": cfg.model,
        "input": "Reply with exactly: OK",
        "max_output_tokens": 512,
    });
    let resp = post_responses(client, cfg, &body).await?;
    let status = resp.status();
    let v: Value = resp.json().await.map_err(|e| format!("bad json: {e}"))?;
    if !status.is_success() {
        return Err(format!("status {status}: {v}"));
    }
    let id = v["id"].as_str().unwrap_or_default();
    if !id.starts_with("resp_") {
        return Err(format!("id should start with resp_, got {id:?}"));
    }
    if v["object"].as_str() != Some("response") {
        return Err(format!("object != response: {}", v["object"]));
    }
    if v["status"].as_str() != Some("completed") {
        return Err(format!("status != completed: {}", v["status"]));
    }
    let item = v["output"]
        .as_array()
        .and_then(|a| {
            // P2 passthrough may lead with a NATIVE reasoning item — the
            // message can sit at any position.
            a.iter().find(|i| i["type"].as_str() == Some("message"))
        })
        .ok_or("output array has no message item")?;
    let text = item["content"][0]["text"].as_str().unwrap_or_default();
    if text.is_empty() {
        return Err(format!("empty output_text: {item}"));
    }
    let input_tokens = v["usage"]["input_tokens"].as_u64().unwrap_or(0);
    if input_tokens == 0 {
        return Err(format!("usage.input_tokens missing: {}", v["usage"]));
    }
    Ok(format!("id={id} text={text:?} tokens={input_tokens}"))
}

// ---------------------------------------------------------------
// Check 2: streaming event contract
// ---------------------------------------------------------------

async fn check_stream(client: &Client, cfg: &Config) -> Result<String, String> {
    let body = json!({
        "model": cfg.model,
        "input": "Say hello",
        "stream": true,
    });
    let resp = post_responses(client, cfg, &body).await?;
    if !resp.status().is_success() {
        let v: Value = resp.json().await.map_err(|e| format!("bad json: {e}"))?;
        return Err(format!("status error: {v}"));
    }

    let events = read_sse(resp).await?;

    // Event 0/1: created(seq 0, in_progress) then in_progress.
    let (name, data) = events.first().ok_or("stream produced no events")?;
    if name != "response.created" {
        return Err(format!("first event should be response.created, got {name}"));
    }
    if data["sequence_number"].as_u64() != Some(0) {
        return Err(format!("created sequence_number != 0: {}", data["sequence_number"]));
    }
    if data["response"]["status"].as_str() != Some("in_progress") {
        return Err("created should carry status in_progress".to_string());
    }
    if events.get(1).map(|(n, _)| n.as_str()) != Some("response.in_progress") {
        return Err("second event should be response.in_progress".to_string());
    }

    // Contiguous sequence numbers starting at 0; `type` matches event name.
    for (i, (name, data)) in events.iter().enumerate() {
        if data["sequence_number"].as_u64() != Some(i as u64) {
            return Err(format!(
                "sequence_number not contiguous at {i} (event {name}): {}",
                data["sequence_number"]
            ));
        }
        if data["type"].as_str() != Some(name.as_str()) {
            return Err(format!("event {name}: type field mismatch"));
        }
    }

    // Deltas concatenate to the done event's text.
    let mut deltas = String::new();
    let mut done_text: Option<&str> = None;
    for (name, data) in &events {
        match name.as_str() {
            "response.output_text.delta" => {
                deltas.push_str(data["delta"].as_str().unwrap_or_default());
            }
            "response.output_text.done" => {
                done_text = data["text"].as_str();
            }
            _ => {}
        }
    }
    let done_text = done_text.ok_or("no response.output_text.done event")?;
    if done_text != deltas {
        return Err(format!("text.done ({done_text:?}) != concatenated deltas ({deltas:?})"));
    }

    // Terminal event: completed/incomplete/failed, response object with usage.
    let (terminal, tdata) = events.last().expect("events checked non-empty above");
    match terminal.as_str() {
        "response.completed" => {
            let tokens = tdata["response"]["usage"]["input_tokens"].as_u64().unwrap_or(0);
            if tokens == 0 {
                return Err("terminal response.usage.input_tokens missing".to_string());
            }
        }
        "response.incomplete" | "response.failed" => {
            return Err(format!("stream ended with {terminal}: {tdata}"));
        }
        other => return Err(format!("stream must end on a terminal event, got {other}")),
    }

    Ok(format!("events={} chars={}", events.len(), deltas.chars().count()))
}

/// Read an SSE body into (event_name, data_json) pairs. Fails the check if a
/// `data: [DONE]` sentinel appears — the Responses contract has none.
async fn read_sse(resp: reqwest::Response) -> Result<Vec<(String, Value)>, String> {
    let mut stream = resp.bytes_stream();
    let mut buffer = String::new();
    let mut events = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("stream read failed: {e}"))?;
        buffer.push_str(&String::from_utf8_lossy(&chunk));
        while let Some(pos) = buffer.find('\n') {
            let line: String = buffer.drain(..=pos).collect();
            let line = line.trim_end_matches(['\r', '\n']);
            if let Some(data) = line.strip_prefix("data: ") {
                if data.trim() == "[DONE]" {
                    return Err("unexpected data: [DONE] sentinel in Responses stream".to_string());
                }
                let value: Value =
                    serde_json::from_str(data).map_err(|e| format!("bad event json: {e}"))?;
                let name = value["type"].as_str().unwrap_or_default().to_string();
                events.push((name, value));
            }
            // `event:` lines are informational here — `type` lives in the data.
        }
    }
    Ok(events)
}

// ---------------------------------------------------------------
// Check 3: previous_response_id guard semantics — either explicit 400 (no
// response store on this gateway) or uniform 404 (store enabled, unknown id).
// ---------------------------------------------------------------

async fn check_guard(client: &Client, cfg: &Config) -> Result<String, String> {
    let body = json!({
        "model": cfg.model,
        "input": "hi",
        "previous_response_id": "resp_does_not_exist",
    });
    let resp = post_responses(client, cfg, &body).await?;
    let status = resp.status();
    let v: Value = resp.json().await.map_err(|e| format!("bad json: {e}"))?;
    match status.as_u16() {
        400 => {
            let message = v["error"]["message"].as_str().unwrap_or_default();
            if !message.contains("previous_response_id") {
                return Err(format!("error should name previous_response_id: {v}"));
            }
            Ok(format!("400 explicit (no store): {message:?}"))
        }
        404 => Ok("404 uniform (store enabled, unknown id)".to_string()),
        other => Err(format!("expected 400 or 404, got {other}: {v}")),
    }
}

// ---------------------------------------------------------------
// Check 4: real agent loop with one tool
// ---------------------------------------------------------------

/// The single tool this agent owns: returns the local time.
fn get_current_time() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("unix_time_{secs}")
}

async fn check_tool_loop(client: &Client, cfg: &Config) -> Result<String, String> {
    let user_text = "What time is it? You must call the get_current_time tool.";
    let mut input: Vec<Value> = vec![json!({ "role": "user", "content": user_text })];

    for turn in 1..=3 {
        let body = json!({
            "model": cfg.model,
            "input": input,
            "tools": [{
                "type": "function",
                "name": "get_current_time",
                "description": "Get the agent host's current time",
                "parameters": { "type": "object", "properties": {} },
            }],
            "tool_choice": "auto",
        });
        let resp = post_responses(client, cfg, &body).await?;
        let status = resp.status();
        let v: Value = resp.json().await.map_err(|e| format!("bad json: {e}"))?;
        if !status.is_success() {
            return Err(format!("turn {turn} status {status}: {v}"));
        }

        let output = v["output"].as_array().cloned().unwrap_or_default();

        // Terminal message → done. The mock upstream echoes the tool result,
        // which proves the call_id round-trip survived the gateway conversion.
        if let Some(message) = output
            .iter()
            .find(|item| item["type"].as_str() == Some("message"))
            .and_then(|item| item["content"][0]["text"].as_str())
        {
            if !message.contains("unix_time_") {
                return Err(format!("final message lacks tool result echo: {message:?}"));
            }
            return Ok(format!("turns={turn} final={message:?}"));
        }

        // function_call → execute locally, replay both items next turn.
        let Some(call) = output.iter().find(|item| item["type"].as_str() == Some("function_call"))
        else {
            return Err(format!("turn {turn}: neither message nor function_call in output: {v}"));
        };
        if call["name"].as_str() != Some("get_current_time") {
            return Err(format!("unexpected tool name: {}", call["name"]));
        }
        let call_id = call["call_id"].as_str().unwrap_or_default().to_string();
        if call_id.is_empty() {
            return Err("function_call missing call_id".to_string());
        }
        let result = get_current_time();
        input.push(json!({
            "type": "function_call",
            "call_id": call_id,
            "name": "get_current_time",
            "arguments": "{}",
        }));
        input.push(json!({
            "type": "function_call_output",
            "call_id": call_id,
            "output": result,
        }));
        println!("      turn {turn}: executed tool → {result}");
    }
    Err("tool loop did not terminate in 3 turns".to_string())
}

// ---------------------------------------------------------------
// Check 5 (P1): stateful continuation + retrieval. Skipped when the
// gateway runs without the response store (previous_response_id → 400).
// ---------------------------------------------------------------

async fn check_store_continuation(client: &Client, cfg: &Config) -> Result<String, String> {
    // Probe availability: 400 = store disabled (skip); 404 = store enabled,
    // unknown id correctly rejected.
    let probe = post_responses(
        client,
        cfg,
        &json!({ "model": cfg.model, "input": "hi", "previous_response_id": "resp_probe" }),
    )
    .await?;
    if probe.status().as_u16() == 400 {
        return Ok("skipped (no response store on this gateway)".to_string());
    }
    if probe.status().as_u16() != 404 {
        return Err(format!("probe expected 404/400, got {}", probe.status()));
    }

    // Turn 1: seed a secret (store defaults to true).
    let resp = post_responses(
        client,
        cfg,
        &json!({ "model": cfg.model, "input": "My secret word is banana" }),
    )
    .await?;
    let v: Value = resp.json().await.map_err(|e| format!("bad json: {e}"))?;
    let id = v["id"].as_str().unwrap_or_default().to_string();
    if !id.starts_with("resp_") {
        return Err(format!("turn 1 returned no resp id: {v}"));
    }

    // Turn 2 sends ONLY the new input. Proof the rehydrated history reached
    // the upstream differs by deployment: the chat mock echoes the FIRST
    // user message ("banana"); a passthrough deployment's native mock echoes
    // the item count (>= 3: user, prior output[, reasoning], user).
    let resp = post_responses(
        client,
        cfg,
        &json!({
            "model": cfg.model,
            "input": "What was my secret word?",
            "previous_response_id": id,
        }),
    )
    .await?;
    let v2: Value = resp.json().await.map_err(|e| format!("bad json: {e}"))?;
    let text = v2["output"]
        .as_array()
        .and_then(|o| o.iter().find(|i| i["type"] == "message"))
        .and_then(|i| i["content"][0]["text"].as_str())
        .unwrap_or_default();
    let history_reached = text.contains("banana")
        || text
            .split("items=")
            .nth(1)
            .and_then(|n| n.trim().parse::<u32>().ok())
            .is_some_and(|n| n >= 3);
    if !history_reached {
        return Err(format!("rehydrated history did not reach upstream, answer: {text:?}"));
    }

    // Retrieval: GET /v1/responses/{id} (persist is async — brief retry).
    let url = format!("{}/v1/responses/{id}", cfg.base_url);
    for attempt in 1..=5 {
        let get = client
            .get(&url)
            .bearer_auth(&cfg.key)
            .send()
            .await
            .map_err(|e| format!("GET failed: {e}"))?;
        if get.status().is_success() {
            let got: Value = get.json().await.map_err(|e| format!("bad json: {e}"))?;
            if got["id"].as_str() != Some(id.as_str()) {
                return Err(format!("GET returned wrong id: {got}"));
            }
            return Ok(format!("turn2_answer={text:?} retrieval_ok"));
        }
        if attempt == 5 {
            return Err(format!("GET status after retries: {}", get.status()));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    unreachable!("loop returns or errors on attempt 5")
}

// ---------------------------------------------------------------
// Check 6 (P0.5): freeform custom tools — the Codex `apply_patch` contract.
// The gateway wraps type:"custom" as a function upstream and MUST restore
// custom_tool_call items on the way out (coercion breaks Codex, vLLM #49724).
// ---------------------------------------------------------------

async fn check_custom_tool_loop(client: &Client, cfg: &Config) -> Result<String, String> {
    let mut input: Vec<Value> =
        vec![json!({ "role": "user", "content": "Apply the demo patch." })];

    for turn in 1..=3 {
        let body = json!({
            "model": cfg.model,
            "input": input,
            "tools": [{
                "type": "custom",
                "name": "apply_patch",
                "description": "Apply a text patch",
            }],
        });
        let resp = post_responses(client, cfg, &body).await?;
        let status = resp.status();
        let v: Value = resp.json().await.map_err(|e| format!("bad json: {e}"))?;
        if !status.is_success() {
            return Err(format!("turn {turn} status {status}: {v}"));
        }
        let output = v["output"].as_array().cloned().unwrap_or_default();

        // Terminal message (mock echoes the tool result) → done.
        if let Some(message) = output
            .iter()
            .find(|item| item["type"].as_str() == Some("message"))
            .and_then(|item| item["content"][0]["text"].as_str())
        {
            if !message.contains("PATCH_RESULT") {
                return Err(format!("final message lacks tool result echo: {message:?}"));
            }
            return Ok(format!("turns={turn} final={message:?}"));
        }

        // The strict contract: a custom_tool_call item — NOT function_call.
        let Some(call) = output
            .iter()
            .find(|item| item["type"].as_str() == Some("custom_tool_call"))
        else {
            let types: Vec<&str> = output
                .iter()
                .filter_map(|i| i["type"].as_str())
                .collect();
            return Err(format!(
                "turn {turn}: expected custom_tool_call, got output types {types:?} (contract coerced?)"
            ));
        };
        if output.iter().any(|i| i["type"] == "function_call") {
            return Err("custom tool leaked as function_call in the same turn".to_string());
        }
        let call_id = call["call_id"].as_str().unwrap_or_default().to_string();
        if call_id.is_empty() || call["name"].as_str() != Some("apply_patch") {
            return Err(format!("bad custom_tool_call identity: {call}"));
        }
        let patch_input = call["input"].as_str().unwrap_or_default().to_string();
        if !patch_input.contains("Begin Patch") {
            return Err(format!("custom input not unwrapped from function args: {patch_input:?}"));
        }

        // Execute: echo the patch back as the freeform output.
        let result = format!("PATCH_RESULT:ok:{patch_input}");
        input.push(json!({
            "type": "custom_tool_call", "call_id": call_id,
            "name": "apply_patch", "input": patch_input,
        }));
        input.push(json!({
            "type": "custom_tool_call_output", "call_id": call_id, "output": result,
        }));
        println!("      turn {turn}: executed custom tool ({} bytes)", patch_input.len());
    }
    Err("custom tool loop did not terminate in 3 turns".to_string())
}

// ---------------------------------------------------------------
// Check 7 (P2): native passthrough fidelity on a `protocol:
// responses` deployment. The mock native endpoint marks its replies;
// non-passthrough deployments get a SKIP.
//
// Fidelity is proven in two steps: turn 1's output carries a NATIVE
// reasoning item (conversion path never emits one), and turn 2 — sending
// only previous_response_id + new input — reaches an upstream that sees the
// full rehydrated history INCLUDING the reasoning item.
// ---------------------------------------------------------------

async fn check_passthrough_fidelity(client: &Client, cfg: &Config) -> Result<String, String> {
    // Probe: passthrough deployments reply with the native marker.
    let probe = post_responses(
        client,
        cfg,
        &json!({ "model": cfg.model, "input": "passthrough probe" }),
    )
    .await?;
    let pv: Value = probe.json().await.map_err(|e| format!("bad json: {e}"))?;
    let probe_text = pv["output"]
        .as_array()
        .and_then(|o| o.iter().find(|i| i["type"] == "message"))
        .and_then(|i| i["content"][0]["text"].as_str())
        .unwrap_or_default();
    if !probe_text.contains("NATIVE_PASSTHROUGH") {
        return Ok("skipped (deployment is not protocol: responses)".to_string());
    }

    // Turn 1: native reasoning item must survive to the client verbatim.
    let resp = post_responses(
        client,
        cfg,
        &json!({ "model": cfg.model, "input": "remember codeword ZULU7" }),
    )
    .await?;
    let v: Value = resp.json().await.map_err(|e| format!("bad json: {e}"))?;
    let reasoning = v["output"]
        .as_array()
        .and_then(|o| o.iter().find(|i| i["type"] == "reasoning"))
        .ok_or_else(|| format!("no native reasoning item in output: {}", v["output"]))?
        .clone();
    if reasoning["id"].as_str().unwrap_or_default().is_empty() {
        return Err(format!("reasoning item missing id: {reasoning}"));
    }
    let id = v["id"].as_str().unwrap_or_default().to_string();
    if !id.starts_with("resp_") {
        return Err(format!("response id is not gateway-minted: {id:?}"));
    }

    // Turn 2 (continuation round-trip) needs the response store — without it
    // previous_response_id is a 400 by design. Outbound fidelity (native
    // reasoning item reaching the client) is already proven above.
    let store_probe = post_responses(
        client,
        cfg,
        &json!({ "model": cfg.model, "input": "hi", "previous_response_id": "resp_probe" }),
    )
    .await?;
    if store_probe.status().as_u16() != 404 {
        return Ok(format!(
            "reasoning_item={} (continuation round-trip needs the response store)",
            reasoning["id"].as_str().unwrap_or("?")
        ));
    }

    // Turn 2: only previous_response_id + new input. Upstream must see the
    // full rehydrated history (user, reasoning, assistant, user = 4 items).
    let resp = post_responses(
        client,
        cfg,
        &json!({
            "model": cfg.model,
            "input": "echo the codeword",
            "previous_response_id": id,
        }),
    )
    .await?;
    let v2: Value = resp.json().await.map_err(|e| format!("bad json: {e}"))?;
    let text = v2["output"]
        .as_array()
        .and_then(|o| o.iter().find(|i| i["type"] == "message"))
        .and_then(|i| i["content"][0]["text"].as_str())
        .unwrap_or_default();
    if !text.contains("items=4") {
        return Err(format!(
            "rehydrated history (incl. reasoning) did not reach upstream verbatim: {text:?}"
        ));
    }
    Ok(format!("reasoning_item={} round2={text:?}", reasoning["id"].as_str().unwrap_or("?")))
}
