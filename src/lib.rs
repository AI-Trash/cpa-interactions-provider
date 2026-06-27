//! cpa-interactions-provider
//!
//! Native CLIProxyAPI plugin that wraps the Gemini Interactions API behind an
//! OpenAI Chat Completions surface. The host loads this cdylib, calls
//! `cliproxy_plugin_init` once, and then dispatches JSON envelopes through
//! `plugin_call`. We use `host.http.do` to perform the upstream call so the
//! host's transport policy, request logging, and proxy configuration still
//! apply.
//!
//! MVP scope: text-only, non-stream, blocking agent calls (e.g.
//! `antigravity-preview-05-2026`), no client-side tool/function result mapping.

#![allow(clippy::missing_safety_doc)]

use std::collections::HashMap;
use std::ffi::CStr;
use std::os::raw::{c_char, c_int, c_void};
use std::ptr;
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use serde_json::{json, Value};

// ============================================================
// FFI ABI surface
// ============================================================

const ABI_VERSION: u32 = 1;
const SCHEMA_VERSION: u32 = 1;
#[allow(dead_code)]
const PLUGIN_ID: &str = "cpa-interactions-provider";

#[repr(C)]
#[derive(Clone, Copy)]
pub struct CliproxyBuffer {
    ptr: *mut u8,
    len: usize,
}

type HostCallFn =
    unsafe extern "C" fn(*mut c_void, *const c_char, *const u8, usize, *mut CliproxyBuffer) -> c_int;
type HostFreeFn = unsafe extern "C" fn(*mut c_void, usize);
type PluginCallFn =
    unsafe extern "C" fn(*const c_char, *const u8, usize, *mut CliproxyBuffer) -> c_int;
type PluginFreeFn = unsafe extern "C" fn(*mut c_void, usize);
type PluginShutdownFn = unsafe extern "C" fn();

#[repr(C)]
#[derive(Clone, Copy)]
pub struct CliproxyHostApi {
    abi_version: u32,
    host_ctx: *mut c_void,
    call: Option<HostCallFn>,
    free_buffer: Option<HostFreeFn>,
}

#[repr(C)]
pub struct CliproxyPluginApi {
    abi_version: u32,
    call: Option<PluginCallFn>,
    free_buffer: Option<PluginFreeFn>,
    shutdown: Option<PluginShutdownFn>,
}

// AtomicPtr is Send+Sync unconditionally, so the static host handle is safe
// to share across the Go host's goroutines.
static HOST: AtomicPtr<CliproxyHostApi> = AtomicPtr::new(ptr::null_mut());

// ============================================================
// Plugin config
// ============================================================

#[derive(Clone, Default)]
struct PluginConfig {
    enabled: bool,
    endpoint: String,
    store: bool,
    agents: Vec<String>,
    default_environment: String,
    route_all_models: bool,
}

impl PluginConfig {
    fn defaults() -> Self {
        PluginConfig {
            enabled: true,
            endpoint: "https://generativelanguage.googleapis.com/v1beta/interactions".to_string(),
            store: true,
            agents: vec![
                "antigravity-preview-05-2026".to_string(),
                "deep-research-preview-04-2026".to_string(),
                "deep-research-max-preview-04-2026".to_string(),
            ],
            default_environment: "remote".to_string(),
            route_all_models: true,
        }
    }
}

static CONFIG: OnceLock<Mutex<PluginConfig>> = OnceLock::new();

fn config() -> &'static Mutex<PluginConfig> {
    CONFIG.get_or_init(|| Mutex::new(PluginConfig::defaults()))
}

// ============================================================
// Session map
// ============================================================

#[derive(Clone)]
struct SessionState {
    interaction_id: String,
    environment_id: Option<String>,
}

static SESSIONS: OnceLock<Mutex<HashMap<[u8; 32], SessionState>>> = OnceLock::new();

fn sessions() -> &'static Mutex<HashMap<[u8; 32], SessionState>> {
    SESSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

// ============================================================
// Plugin entry
// ============================================================

#[no_mangle]
pub extern "C" fn cliproxy_plugin_init(host: *const CliproxyHostApi, plugin: *mut CliproxyPluginApi) -> c_int {
    if plugin.is_null() {
        return 1;
    }
    if !host.is_null() {
        HOST.store(host as *mut CliproxyHostApi, Ordering::SeqCst);
    }
    unsafe {
        (*plugin).abi_version = ABI_VERSION;
        (*plugin).call = Some(plugin_call);
        (*plugin).free_buffer = Some(plugin_free);
        (*plugin).shutdown = Some(plugin_shutdown);
    }
    0
}

unsafe extern "C" fn plugin_call(
    method: *const c_char,
    request: *const u8,
    request_len: usize,
    response: *mut CliproxyBuffer,
) -> c_int {
    if !response.is_null() {
        (*response).ptr = ptr::null_mut();
        (*response).len = 0;
    }
    if method.is_null() {
        let msg = error_envelope("invalid_method", "method is required");
        write_response(response, msg.as_bytes());
        return 1;
    }
    let method_str = match CStr::from_ptr(method).to_str() {
        Ok(v) => v,
        Err(_) => {
            let msg = error_envelope("invalid_method", "method is not utf-8");
            write_response(response, msg.as_bytes());
            return 1;
        }
    };
    let req_bytes: &[u8] = if request.is_null() || request_len == 0 {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(request, request_len) }
    };
    let outcome = handle_method(method_str, req_bytes);
    let (rc, body) = match outcome {
        Ok(bytes) => (0, bytes),
        Err(msg) => (
            1,
            error_envelope("plugin_error", &msg).into_bytes(),
        ),
    };
    write_response(response, &body);
    rc
}

unsafe extern "C" fn plugin_free(ptr: *mut c_void, len: usize) {
    if !ptr.is_null() && len > 0 {
        let _ = Vec::from_raw_parts(ptr as *mut u8, len, len);
    }
}

unsafe extern "C" fn plugin_shutdown() {}

// ============================================================
// Method dispatch
// ============================================================

fn handle_method(method: &str, request: &[u8]) -> Result<Vec<u8>, String> {
    match method {
        "plugin.register" | "plugin.reconfigure" => {
            apply_config(request)?;
            Ok(ok_envelope_bytes(&registration_result()))
        }
        "executor.identifier" => Ok(ok_envelope_bytes(r#"{"identifier":"cpa-interactions-provider"}"#)),
        "model.route" => route_model(request),
        "executor.execute" => execute(request, false),
        "executor.execute_stream" => execute(request, true),
        "executor.count_tokens" => Ok(ok_envelope_bytes(
            r#"{"Payload":"eyJ0b3RhbF90b2tlbnMiOjB9"}"#,
        )),
        "executor.http_request" => Ok(ok_envelope_bytes(
            r#"{"StatusCode":200,"Headers":{"content-type":["application/json"]}}"#,
        )),
        _ => Ok(error_envelope("unknown_method", &format!("unknown method: {method}")).into_bytes()),
    }
}

fn apply_config(raw: &[u8]) -> Result<(), String> {
    if raw.is_empty() {
        return Ok(());
    }
    let req: Value = serde_json::from_slice(raw).map_err(|e| format!("decode lifecycle request: {e}"))?;
    let config_yaml: &[u8] = req
        .get("config_yaml")
        .and_then(|v| v.as_str())
        .map(|s| s.as_bytes())
        .unwrap_or(&[]);
    // Parse YAML manually for the few fields we care about. Avoids a yaml
    // dependency. We tolerate comments and trailing whitespace by skipping
    // lines starting with '#'.
    let mut cfg = PluginConfig::defaults();
    for line in std::str::from_utf8(config_yaml)
        .map_err(|e| format!("config_yaml is not utf-8: {e}"))?
        .lines()
    {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some((key, val)) = trimmed.split_once(':') else { continue };
        let key = key.trim();
        let val = val.trim();
        // Strip inline comments after value.
        let val = match val.split_once('#') {
            Some((v, _)) => v.trim(),
            None => val,
        };
        match key {
            "enabled" => cfg.enabled = parse_bool(val),
            "endpoint" => cfg.endpoint = strip_quotes(val).to_string(),
            "store" => cfg.store = parse_bool(val),
            "default_environment" => cfg.default_environment = strip_quotes(val).to_string(),
            "route_all_models" => cfg.route_all_models = parse_bool(val),
            "agents" => {
                // Multi-line array; tolerate simple inline form, e.g.
                // `agents: [a, b]` or skip subsequent `- item` lines handled
                // in a second pass.
                let trimmed_v = strip_quotes(val);
                if !trimmed_v.is_empty() {
                    let inner = trimmed_v
                        .strip_prefix('[')
                        .and_then(|s| s.strip_suffix(']'))
                        .unwrap_or(trimmed_v);
                    cfg.agents = inner
                        .split(',')
                        .map(|s| strip_quotes(s.trim()).to_string())
                        .filter(|s| !s.is_empty())
                        .collect();
                }
            }
            _ => {
                // Ignore unknown fields (host-managed like priority).
                if val.is_empty() && key == "agents" {
                    // Begin array below; handled in array_item branch.
                }
            }
        }
    }
    // Second pass for block-style arrays under `agents:`.
    let mut collecting_agents = false;
    for line in std::str::from_utf8(config_yaml)
        .map_err(|e| format!("config_yaml is not utf-8: {e}"))?
        .lines()
    {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if collecting_agents {
            if let Some(item) = trimmed.strip_prefix("- ") {
                let item = strip_quotes(item.trim());
                if !item.is_empty() {
                    cfg.agents.push(item.to_string());
                }
                continue;
            }
            // Stop collecting once a non-dash line appears.
            collecting_agents = false;
        }
        if let Some((key, val)) = trimmed.split_once(':') {
            if key.trim() == "agents" && val.trim().is_empty() {
                collecting_agents = true;
            }
        }
    }
    *config().lock().map_err(|e| format!("config lock: {e}"))? = cfg;
    Ok(())
}

fn parse_bool(s: &str) -> bool {
    matches!(s.trim().to_ascii_lowercase().as_str(), "true" | "yes" | "on" | "1")
}

fn strip_quotes(s: &str) -> &str {
    let s = s.trim();
    if (s.starts_with('"') && s.ends_with('"') && s.len() >= 2)
        || (s.starts_with('\'') && s.ends_with('\'') && s.len() >= 2)
    {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

fn registration_result() -> String {
    let caps = json!({
        "model_router": true,
        "executor": true,
        "executor_model_scope": "both",
        "executor_input_formats": ["chat-completions"],
        "executor_output_formats": ["chat-completions"],
    });
    let config_fields = json!([
        {"Name":"enabled","Type":"boolean","Description":"Master switch."},
        {"Name":"endpoint","Type":"string","Description":"Interactions API endpoint URL."},
        {"Name":"store","Type":"boolean","Description":"Store interactions server-side for chaining."},
        {"Name":"agents","Type":"array","Description":"Model names treated as agents (use `agent` field)."},
        {"Name":"default_environment","Type":"string","Description":"Default environment string for agents that require one."},
        {"Name":"route_all_models","Type":"boolean","Description":"Claim Gemini chat-model requests too; when false only agent-named models route here."}
    ]);
    let result = json!({
        "schema_version": SCHEMA_VERSION,
        "metadata": {
            "Name": "cpa-interactions-provider",
            "Version": env!("CARGO_PKG_VERSION"),
            "Author": "AI-Trash",
            "GitHubRepository": "https://github.com/AI-Trash/cpa-interactions-provider",
            "ConfigFields": config_fields,
        },
        "capabilities": caps,
    });
    serde_json::to_string(&result).expect("serialize registration")
}

// ============================================================
// Model router
// ============================================================

fn route_model(raw: &[u8]) -> Result<Vec<u8>, String> {
    let cfg = config().lock().map_err(|e| format!("config lock: {e}"))?.clone();
    if !cfg.enabled {
        return Ok(ok_envelope_bytes(r#"{"Handled":false}"#));
    }
    let req: Value =
        serde_json::from_slice(raw).map_err(|e| format!("decode route request: {e}"))?;
    let model = req.get("RequestedModel").and_then(|v| v.as_str()).unwrap_or("");
    if model.is_empty() {
        return Ok(ok_envelope_bytes(r#"{"Handled":false}"#));
    }
    if cfg.agents.iter().any(|a| a == model) || (cfg.route_all_models && is_gemini_chat_model(model))
    {
        let result = json!({
            "Handled": true,
            "TargetKind": "self",
            "Reason": "interactions-provider-claim",
        });
        let envelope = format!("{{\"ok\":true,\"result\":{}}}", result);
        return Ok(envelope.into_bytes());
    }
    Ok(ok_envelope_bytes(r#"{"Handled":false}"#))
}

fn is_gemini_chat_model(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.starts_with("gemini-") || lower.starts_with("gemma-")
}

// ============================================================
// Executor
// ============================================================

fn execute(raw: &[u8], stream: bool) -> Result<Vec<u8>, String> {
    let req: Value =
        serde_json::from_slice(raw).map_err(|e| format!("decode executor request: {e}"))?;

    let model = req.get("Model").and_then(|v| v.as_str()).unwrap_or("").to_string();
    if model.is_empty() {
        return Err("executor request missing model".to_string());
    }
    let cfg = config().lock().map_err(|e| format!("config lock: {e}"))?.clone();

    // OriginalRequest carries the raw client payload (OpenAI chat-completions
    // JSON). The host passes it straight through because we declared
    // executor_input_formats=["chat-completions"].
    let original_b64 = req
        .get("OriginalRequest")
        .or_else(|| req.get("Payload"))
        .and_then(|v| v.as_str())
        .ok_or("executor request missing OriginalRequest")?;
    let original_bytes = B64
        .decode(original_b64)
        .map_err(|e| format!("decode original request base64: {e}"))?;
    let openai_req: Value =
        serde_json::from_slice(&original_bytes).map_err(|e| format!("parse openai request: {e}"))?;

    let messages = openai_req
        .get("messages")
        .and_then(|v| v.as_array())
        .ok_or("openai request missing messages array")?;
    if messages.is_empty() {
        return Err("openai request has empty messages".to_string());
    }

    // Established prefix = everything except the last message. The last
    // message becomes this turn's new `user_input` step.
    let last_user_text = extract_text_content(messages.last().unwrap());
    if last_user_text.is_empty() {
        return Err("last message has no text content".to_string());
    }

    // Compute session hash key from the established prefix + model + system
    // + tools so we can identify a chained conversation.
    let established = &messages[..messages.len() - 1];
    let lookup_key = compute_session_key(&model, established, &openai_req);
    let prev_state = sessions()
        .lock()
        .map_err(|e| format!("sessions lock: {e}"))?
        .get(&lookup_key)
        .cloned();

    let is_agent = cfg.agents.iter().any(|a| a == &model);

    // Build Interactions API request body.
    let mut interactions_req = serde_json::Map::new();
    if is_agent {
        interactions_req.insert("agent".into(), Value::String(model.clone()));
        let cached_env_id = prev_state
            .as_ref()
            .and_then(|s| s.environment_id.as_ref())
            .cloned();
        if let Some(env_id) = cached_env_id {
            interactions_req.insert("environment".into(), Value::String(env_id));
        } else if !cfg.default_environment.is_empty() {
            interactions_req.insert(
                "environment".into(),
                Value::String(cfg.default_environment.clone()),
            );
        }
    } else {
        interactions_req.insert("model".into(), Value::String(model.clone()));
    }
    if let Some(state) = prev_state.as_ref() {
        if !state.interaction_id.is_empty() {
            interactions_req.insert(
                "previous_interaction_id".into(),
                Value::String(state.interaction_id.clone()),
            );
        }
    }
    interactions_req.insert("store".into(), Value::Bool(cfg.store));
    interactions_req.insert(
        "input".into(),
        json!([{
            "type": "user_input",
            "content": [{"type":"text","text": last_user_text}],
        }]),
    );
    let body_json = serde_json::to_string(&Value::Object(interactions_req))
        .map_err(|e| format!("serialize interactions request: {e}"))?;
    let body_b64 = B64.encode(body_json.as_bytes());

    // Read the API key from the auth attributes populated by the host's
    // auth selection step. We inject it manually because host.http.do does
    // not run the auth layer for us (its auth style is provider-specific).
    let auth_attrs = req.get("AuthAttributes").and_then(|v| v.as_object());
    let api_key = auth_attrs
        .and_then(|m| m.get("api_key"))
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let host_callback_id = req
        .get("HostCallbackID")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    // Build host.http.do RPC payload.
    let host_http_req = json!({
        "method": "POST",
        "url": cfg.endpoint,
        "headers": {
            "x-goog-api-key": [api_key],
            "content-type": ["application/json"],
        },
        "body": body_b64,
        "host_callback_id": host_callback_id,
    });
    let host_payload =
        serde_json::to_vec(&host_http_req).map_err(|e| format!("serialize host.http.do: {e}"))?;

    let (rc, resp_envelope) = call_host("host.http.do", &host_payload)?;
    if rc != 0 {
        return Err(format!("host.http.do returned rc={rc}"));
    }
    let env: Value = serde_json::from_slice(&resp_envelope)
        .map_err(|e| format!("parse host.http.do envelope: {e}"))?;
    if !env.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
        let err = env
            .get("error")
            .and_then(|v| v.as_object())
            .and_then(|m| m.get("message"))
            .and_then(|v| v.as_str())
            .unwrap_or("host.http.do failed");
        return Err(err.to_string());
    }
    let result = env.get("result").ok_or("missing host.http.do result")?;
    let status_code = result
        .get("status_code")
        .and_then(|v| v.as_i64())
        .unwrap_or(0) as i32;
    let resp_body_b64 = result
        .get("body")
        .and_then(|v| v.as_str())
        .ok_or("missing host.http.do body")?;
    let resp_bytes = B64
        .decode(resp_body_b64)
        .map_err(|e| format!("decode host.http.do body base64: {e}"))?;
    if status_code != 200 {
        return Err(format!(
            "upstream returned status {status_code}: {}",
            String::from_utf8_lossy(&resp_bytes)
        ));
    }
    let interaction: Value = serde_json::from_slice(&resp_bytes)
        .map_err(|e| format!("parse interaction response: {e}"))?;

    let new_id = interaction
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or("missing interaction.id")?
        .to_string();
    let new_env_id = interaction
        .get("environment_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let status = interaction
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if status == "in_progress" || status == "requires_action" {
        return Err(format!(
            "agent returned non-terminal status {status}; background polling is not supported in v0.1"
        ));
    }

    // Store new session keyed by (established + last user + new assistant) hash.
    let assistant_text = extract_model_output_text(&interaction);
    let next_messages = build_next_messages(messages, &assistant_text);
    let next_key = compute_session_key(&model, &next_messages, &openai_req);
    sessions()
        .lock()
        .map_err(|e| format!("sessions lock on insert: {e}"))?
        .insert(
            next_key,
            SessionState {
                interaction_id: new_id.clone(),
                environment_id: new_env_id.clone(),
            },
        );

    let usage = interaction.get("usage");
    let prompt_tokens = usage
        .and_then(|u| u.get("total_input_tokens"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let completion_tokens = usage
        .and_then(|u| u.get("total_output_tokens"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let total_tokens = usage
        .and_then(|u| u.get("total_tokens"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let created = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    if stream {
        return build_stream_executor_response(&new_id, &model, &assistant_text, created);
    }

    let chat_completion = json!({
        "id": new_id,
        "object": "chat.completion",
        "created": created,
        "model": model,
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": assistant_text,
            },
            "finish_reason": "stop",
        }],
        "usage": {
            "prompt_tokens": prompt_tokens,
            "completion_tokens": completion_tokens,
            "total_tokens": total_tokens,
        },
    });
    let payload_bytes = serde_json::to_vec(&chat_completion)
        .map_err(|e| format!("serialize chat completion: {e}"))?;
    let payload_b64 = B64.encode(&payload_bytes);
    let result = format!(
        r#"{{"Payload":"{}","Headers":{{"content-type":["application/json"]}}}}"#,
        payload_b64
    );
    Ok(format!("{{\"ok\":true,\"result\":{}}}", result).into_bytes())
}

fn build_stream_executor_response(
    new_id: &str,
    model: &str,
    assistant_text: &str,
    created: i64,
) -> Result<Vec<u8>, String> {
    // MVP: emit a single chunk plus [DONE]. The host stream_forwarder punts
    // these bytes straight into the SSE response. Real streaming means
    // calling host.http.do_stream and translating Interactions SSE events,
    // which is a v0.2 task.
    let chunk = json!({
        "id": new_id,
        "object": "chat.completion.chunk",
        "created": created,
        "model": model,
        "choices": [{
            "index": 0,
            "delta": {
                "role": "assistant",
                "content": assistant_text,
            },
            "finish_reason": "stop",
        }],
    });
    let chunk_json = serde_json::to_string(&chunk).map_err(|e| format!("serialize chunk: {e}"))?;
    let sse_payload = format!("data: {chunk_json}\n\ndata: [DONE]\n\n");
    let payload_b64 = B64.encode(sse_payload.as_bytes());
    let result = format!(
        r#"{{"headers":{{"content-type":["text/event-stream"]}},"chunks":[{{"Payload":"{}"}}]}}"#,
        payload_b64
    );
    Ok(format!("{{\"ok\":true,\"result\":{}}}", result).into_bytes())
}

// ============================================================
// Helpers
// ============================================================

fn extract_text_content(msg: &Value) -> String {
    // Accept both string content and array content. Multimodal parts are
    // joined by space (text-only for MVP).
    let content = msg.get("content");
    let Some(content) = content else { return String::new() };
    if let Some(s) = content.as_str() {
        return s.to_string();
    }
    if let Some(arr) = content.as_array() {
        return arr
            .iter()
            .filter_map(|p| {
                if p.get("type").and_then(|t| t.as_str()) == Some("text") {
                    p.get("text").and_then(|t| t.as_str()).map(|s| s.to_string())
                } else {
                    None
                }
            })
            .collect::<Vec<String>>()
            .join("");
    }
    String::new()
}

fn extract_model_output_text(interaction: &Value) -> String {
    let Some(steps) = interaction.get("steps").and_then(|v| v.as_array()) else {
        return String::new();
    };
    let mut buf = String::new();
    for step in steps {
        if step.get("type").and_then(|t| t.as_str()) != Some("model_output") {
            continue;
        }
        if let Some(content) = step.get("content").and_then(|c| c.as_array()) {
            for part in content {
                if let Some(t) = part.get("text").and_then(|v| v.as_str()) {
                    buf.push_str(t);
                }
            }
        }
    }
    buf
}

fn build_next_messages(request_messages: &[Value], assistant_text: &str) -> Vec<Value> {
    // The next request will append the assistant reply we just produced, so
    // the established prefix on the next turn is (request_messages
    // including the just-sent user message) + this assistant message.
    let mut next = request_messages.to_vec();
    next.push(json!({"role": "assistant", "content": assistant_text}));
    next
}

fn compute_session_key(model: &str, established: &[Value], openai_req: &Value) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"model:");
    hasher.update(model.as_bytes());
    hasher.update(b"\n");
    // Stable serialization: rely on serde_json canonical ordering on small
    // arrays. We hash each message's `role` and serialized JSON for
    // determinism.
    hasher.update(b"messages:\n");
    for msg in established {
        if let Some(role) = msg.get("role").and_then(|r| r.as_str()) {
            hasher.update(role.as_bytes());
            hasher.update(b"\n");
            if let Ok(text) = serde_json::to_string(msg) {
                hasher.update(text.as_bytes());
                hasher.update(b"\n");
            }
        }
    }
    hasher.update(b"system:\n");
    if let Some(system) = openai_req.get("messages").and_then(|v| v.as_array()) {
        for msg in system {
            if msg.get("role").and_then(|r| r.as_str()) == Some("system") {
                if let Ok(text) = serde_json::to_string(msg) {
                    hasher.update(text.as_bytes());
                    hasher.update(b"\n");
                }
            }
        }
    }
    hasher.update(b"tools:\n");
    if let Some(tools) = openai_req.get("tools").and_then(|v| v.as_array()) {
        for tool in tools {
            if let Ok(text) = serde_json::to_string(tool) {
                hasher.update(text.as_bytes());
                hasher.update(b"\n");
            }
        }
    }
    hasher.update(b"generation_config:\n");
    if let Some(gc) = openai_req.get("generation_config") {
        if let Ok(text) = serde_json::to_string(gc) {
            hasher.update(text.as_bytes());
            hasher.update(b"\n");
        }
    }
    let digest = hasher.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(digest.as_bytes());
    out
}

// Synchronous host call. Returns (rc, response_bytes).
fn call_host(method: &str, payload: &[u8]) -> Result<(i32, Vec<u8>), String> {
    let host_ptr = HOST.load(Ordering::SeqCst);
    if host_ptr.is_null() {
        return Err("host api not initialized".to_string());
    }
    let host = unsafe { &*host_ptr };
    let call = host.call.ok_or("host call function missing")?;
    let mut method_bytes = method.as_bytes().to_vec();
    method_bytes.push(0);
    let mut response = CliproxyBuffer { ptr: ptr::null_mut(), len: 0 };
    let rc = unsafe {
        call(
            host.host_ctx,
            method_bytes.as_ptr() as *const c_char,
            payload.as_ptr(),
            payload.len(),
            &mut response,
        )
    };
    let mut out = Vec::new();
    if !response.ptr.is_null() && response.len > 0 {
        unsafe {
            out.extend_from_slice(std::slice::from_raw_parts(response.ptr, response.len));
        }
        if let Some(free_buffer) = host.free_buffer {
            unsafe { free_buffer(response.ptr as *mut c_void, response.len) };
        }
    }
    Ok((rc, out))
}

fn ok_envelope_bytes(result_json: &str) -> Vec<u8> {
    format!("{{\"ok\":true,\"result\":{}}}", result_json).into_bytes()
}

fn error_envelope(code: &str, msg: &str) -> String {
    format!(
        r#"{{"ok":false,"error":{{"code":"{}","message":"{}"}}}}"#,
        escape_json_string(code),
        escape_json_string(msg),
    )
}

fn escape_json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch if ch.is_control() => out.push(' '),
            ch => out.push(ch),
        }
    }
    out
}

fn write_response(response: *mut CliproxyBuffer, body: &[u8]) {
    if response.is_null() || body.is_empty() {
        return;
    }
    let mut bytes = body.to_vec();
    let len = bytes.len();
    let ptr = bytes.as_mut_ptr();
    std::mem::forget(bytes);
    unsafe {
        (*response).ptr = ptr;
        (*response).len = len;
    }
}