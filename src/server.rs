use crate::llm::{compact_history, escalate_to_claude, fetch_ollama_models, run_agent_turn};
use crate::state::{clean_path_string, load_agents_md, AppState};
use crate::tools::run_quick_check;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Duration;

const UI_HTML: &str = include_str!("ui.html");

pub fn serve_http(listener: TcpListener, state: Arc<AppState>) {
    for stream in listener.incoming().flatten() {
        let st = Arc::clone(&state);
        thread::spawn(move || {
            let _ = handle_connection(stream, st);
        });
    }
}

struct HttpRequest {
    method: String,
    path: String,
    query: HashMap<String, String>,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

fn handle_connection(mut stream: TcpStream, state: Arc<AppState>) -> std::io::Result<()> {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
    let req = match read_request(&mut stream) {
        Ok(Some(r)) => r,
        _ => return Ok(()),
    };
    let _ = stream.set_read_timeout(None);

    if req.method == "OPTIONS" {
        return write_response(&mut stream, "204 No Content", "text/plain", b"");
    }

    if req.path == "/" || req.path == "/index.html" {
        return write_response(&mut stream, "200 OK", "text/html; charset=utf-8", UI_HTML.as_bytes());
    }

    if !is_authorized(&req, &state.token) {
        return write_json(&mut stream, "401 Unauthorized", &json!({ "error": "Unauthorized: invalid or missing token" }));
    }

    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/api/events") => handle_sse(stream, state),
        ("GET", "/api/state") => {
            let cfg = state.config.lock().unwrap().clone();
            let msgs = state.messages.lock().unwrap().clone();
            let mut usage = state.usage.lock().unwrap().clone();
            usage.estimated_history_tokens = state.estimate_history_tokens();
            let running = *state.running.lock().unwrap();
            let approval = state
                .approval
                .lock()
                .unwrap()
                .as_ref()
                .map(|p| p.snapshot.clone());
            let ollama_models = fetch_ollama_models(&cfg.ollama_url, &state.http);
            let has_anthropic_key = std::env::var("ANTHROPIC_API_KEY")
                .map(|v| !v.trim().is_empty())
                .unwrap_or(false);
            write_json(
                &mut stream,
                "200 OK",
                &json!({
                    "config": cfg,
                    "messages": msgs,
                    "usage": usage,
                    "running": running,
                    "approval": approval,
                    "ollama_models": ollama_models,
                    "has_anthropic_key": has_anthropic_key,
                }),
            )
        }
        ("POST", "/api/config") => {
            let patch: Value = serde_json::from_slice(&req.body).unwrap_or(json!({}));
            let result = apply_config_patch(&state, &patch);
            match result {
                Ok(cfg) => {
                    state.emit("config_updated", serde_json::to_value(&cfg).unwrap_or(json!({})));
                    write_json(&mut stream, "200 OK", &json!({ "ok": true, "config": cfg }))
                }
                Err(e) => write_json(&mut stream, "400 Bad Request", &json!({ "error": e })),
            }
        }
        ("POST", "/api/chat") => {
            let body: Value = serde_json::from_slice(&req.body).unwrap_or(json!({}));
            let prompt = body["prompt"].as_str().unwrap_or("").trim().to_string();
            if prompt.is_empty() {
                return write_json(&mut stream, "400 Bad Request", &json!({ "error": "Empty prompt" }));
            }
            {
                let mut running = state.running.lock().unwrap();
                if *running {
                    return write_json(&mut stream, "409 Conflict", &json!({ "error": "Agent is already running; stop it first." }));
                }
                *running = true;
            }
            let st = Arc::clone(&state);
            thread::spawn(move || run_agent_turn(st, prompt));
            write_json(&mut stream, "200 OK", &json!({ "ok": true }))
        }
        ("POST", "/api/approve") => {
            let body: Value = serde_json::from_slice(&req.body).unwrap_or(json!({}));
            let allow = body["allow"].as_bool().unwrap_or(false);
            let mut guard = state.approval.lock().unwrap();
            if let Some(pending) = guard.as_mut() {
                pending.decision = Some(allow);
                state.approval_cv.notify_all();
                state.emit("approval_resolved", json!({ "allow": allow }));
                write_json(&mut stream, "200 OK", &json!({ "ok": true }))
            } else {
                write_json(&mut stream, "404 Not Found", &json!({ "error": "No pending approval" }))
            }
        }
        ("POST", "/api/stop") => {
            state.cancel.store(true, Ordering::SeqCst);
            {
                let mut guard = state.approval.lock().unwrap();
                if let Some(pending) = guard.as_mut() {
                    pending.decision = Some(false);
                }
                state.approval_cv.notify_all();
            }
            write_json(&mut stream, "200 OK", &json!({ "ok": true }))
        }
        ("POST", "/api/compact") => {
            let summary = compact_history(&state);
            write_json(&mut stream, "200 OK", &json!({ "ok": true, "summary": summary }))
        }
        ("POST", "/api/clear") => {
            state.messages.lock().unwrap().clear();
            {
                let mut u = state.usage.lock().unwrap();
                u.prompt_tokens = 0;
                u.output_tokens = 0;
                u.cached_tokens = 0;
                u.estimated_history_tokens = state.estimate_history_tokens();
            }
            state.emit("cleared", json!({}));
            write_json(&mut stream, "200 OK", &json!({ "ok": true }))
        }
        ("POST", "/api/quick-check") => {
            let body: Value = serde_json::from_slice(&req.body).unwrap_or(json!({}));
            let lang = body["lang"].as_str().unwrap_or("rust");
            let ws = PathBuf::from(&state.config.lock().unwrap().workspace);
            let output = run_quick_check(&ws, lang);
            state.emit("quick_check", json!({ "lang": lang, "output": output }));
            write_json(&mut stream, "200 OK", &json!({ "ok": true, "output": output }))
        }
        ("POST", "/api/escalate") => {
            let body: Value = serde_json::from_slice(&req.body).unwrap_or(json!({}));
            let note = body["note"].as_str().unwrap_or("").to_string();
            let advice = escalate_to_claude(&state, &note);
            write_json(&mut stream, "200 OK", &json!({ "ok": true, "advice": advice }))
        }
        _ => write_json(&mut stream, "404 Not Found", &json!({ "error": "Not found" })),
    }
}

fn apply_config_patch(
    state: &Arc<AppState>,
    patch: &Value,
) -> Result<crate::state::AgentConfig, String> {
    let mut cfg = state.config.lock().unwrap();
    if let Some(v) = patch["provider"].as_str() {
        cfg.provider = v.trim().to_string();
    }
    if let Some(v) = patch["ollama_url"].as_str() {
        cfg.ollama_url = v.trim().to_string();
    }
    if let Some(v) = patch["model"].as_str() {
        let m = v.trim();
        if !m.is_empty() {
            cfg.model = m.to_string();
        }
    }
    if let Some(v) = patch["claude_model"].as_str() {
        let m = v.trim();
        if !m.is_empty() {
            cfg.claude_model = m.to_string();
        }
    }
    if let Some(v) = patch["num_ctx"].as_u64() {
        cfg.num_ctx = v as usize;
    }
    if let Some(v) = patch["max_tokens"].as_u64() {
        cfg.max_tokens = v as usize;
    }
    if let Some(v) = patch["think"].as_str() {
        cfg.think = v.trim().to_string();
    }
    if let Some(v) = patch["mode"].as_str() {
        cfg.mode = v.trim().to_string();
    }
    if let Some(v) = patch["auto_compact_ratio"].as_f64() {
        cfg.auto_compact_ratio = v;
    }
    let mut workspace_changed = false;
    if let Some(v) = patch["workspace"].as_str() {
        let p = PathBuf::from(v.trim());
        if !p.is_dir() {
            return Err(format!("Workspace directory does not exist: {}", p.display()));
        }
        let canon = p.canonicalize().unwrap_or(p);
        let next_ws = clean_path_string(&canon);
        if next_ws != cfg.workspace {
            cfg.workspace = next_ws;
            workspace_changed = true;
        }
    }
    if let Some(v) = patch["instructions"].as_str() {
        cfg.instructions = v.to_string();
        if patch["save_agents_md"].as_bool() == Some(true) {
            let p = PathBuf::from(&cfg.workspace).join("AGENTS.md");
            fs::write(&p, v).map_err(|e| format!("Failed to save AGENTS.md: {e}"))?;
        }
    } else if workspace_changed {
        cfg.instructions = load_agents_md(&PathBuf::from(&cfg.workspace));
    }

    cfg.normalize();
    Ok(cfg.clone())
}

fn handle_sse(mut stream: TcpStream, state: Arc<AppState>) -> std::io::Result<()> {
    let header = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\nAccess-Control-Allow-Origin: *\r\n\r\n: connected\n\n";
    stream.write_all(header.as_bytes())?;
    stream.flush()?;

    let (tx, rx) = mpsc::channel::<String>();
    state.subscribers.lock().unwrap().push(tx);

    loop {
        match rx.recv_timeout(Duration::from_secs(15)) {
            Ok(msg) => {
                if stream.write_all(msg.as_bytes()).is_err() || stream.flush().is_err() {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if stream.write_all(b": keepalive\n\n").is_err() || stream.flush().is_err() {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    Ok(())
}

fn is_authorized(req: &HttpRequest, expected: &str) -> bool {
    if let Some(q) = req.query.get("token")
        && q == expected
    {
        return true;
    }
    if let Some(h) = req.headers.get("x-penguin-token")
        && h == expected
    {
        return true;
    }
    if let Some(auth) = req.headers.get("authorization")
        && let Some(bearer) = auth.strip_prefix("Bearer ").or_else(|| auth.strip_prefix("bearer "))
        && bearer.trim() == expected
    {
        return true;
    }
    false
}

fn read_request(stream: &mut TcpStream) -> std::io::Result<Option<HttpRequest>> {
    let mut buf = Vec::with_capacity(4096);
    let mut tmp = [0u8; 2048];
    let header_end;
    loop {
        let n = stream.read(&mut tmp)?;
        if n == 0 {
            return Ok(None);
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = find_bytes(&buf, b"\r\n\r\n") {
            header_end = pos + 4;
            break;
        }
        if buf.len() > 65_536 {
            return Ok(None);
        }
    }

    let head_str = String::from_utf8_lossy(&buf[..header_end]);
    let mut lines = head_str.split("\r\n");
    let first = lines.next().unwrap_or("");
    let mut parts = first.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let raw_target = parts.next().unwrap_or("/");
    if method.is_empty() {
        return Ok(None);
    }

    let (path, query) = parse_target(raw_target);
    let mut headers = HashMap::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }

    let content_len = headers
        .get("content-length")
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(0)
        .min(8 * 1024 * 1024);

    let mut body = buf[header_end..].to_vec();
    while body.len() < content_len {
        let mut chunk = vec![0u8; (content_len - body.len()).min(8192)];
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(content_len);

    Ok(Some(HttpRequest {
        method,
        path,
        query,
        headers,
        body,
    }))
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn parse_target(raw: &str) -> (String, HashMap<String, String>) {
    let mut query = HashMap::new();
    let (path, qs) = raw.split_once('?').unwrap_or((raw, ""));
    for pair in qs.split('&').filter(|s| !s.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        query.insert(k.to_string(), v.to_string());
    }
    (path.to_string(), query)
}

fn write_json(stream: &mut TcpStream, status: &str, val: &Value) -> std::io::Result<()> {
    let body = serde_json::to_vec(val).unwrap_or_else(|_| b"{}".to_vec());
    write_response(stream, status, "application/json; charset=utf-8", &body)
}

fn write_response(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    body: &[u8],
) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Headers: Authorization, Content-Type, X-Penguin-Token\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}
