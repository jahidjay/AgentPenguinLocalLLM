use crate::state::{
    truncate_utf8, AgentConfig, AppState, ApprovalSnapshot, ChatMessage, PendingApproval,
    TokenUsage, ToolCallRecord,
};
use crate::tools::{
    anthropic_tool_schemas, ask_claude_text, execute_tool, is_mutating_tool, ollama_tool_schemas,
    summarize_tool_call,
};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;

struct StepOutput {
    content: String,
    thinking: String,
    tool_calls: Vec<ToolCallRecord>,
    prompt_tokens: usize,
    output_tokens: usize,
    cached_tokens: usize,
}

pub fn fetch_ollama_models(ollama_url: &str, http: &ureq::Agent) -> Vec<String> {
    let url = format!("{}/api/tags", ollama_url.trim_end_matches('/'));
    let Ok(mut resp) = http.get(&url).call() else {
        return Vec::new();
    };
    if resp.status().as_u16() >= 400 {
        return Vec::new();
    }
    let Ok(v) = resp.body_mut().read_json::<Value>() else {
        return Vec::new();
    };
    v["models"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|m| m["name"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

pub fn run_agent_turn(state: Arc<AppState>, user_prompt: String) {
    state.cancel.store(false, Ordering::SeqCst);
    {
        let mut msgs = state.messages.lock().unwrap();
        msgs.push(ChatMessage {
            role: "user".into(),
            content: user_prompt.clone(),
            thinking: String::new(),
            tool_calls: Vec::new(),
            tool_name: String::new(),
            tool_call_id: String::new(),
        });
    }
    let est = state.estimate_history_tokens();
    {
        state.usage.lock().unwrap().estimated_history_tokens = est;
    }
    state.emit("message", json!({ "role": "user", "content": user_prompt }));
    emit_usage(&state);

    for step in 0..24 {
        if state.cancel.load(Ordering::SeqCst) {
            state.emit("status", json!({ "state": "cancelled" }));
            break;
        }
        maybe_auto_compact(&state);

        let cfg = state.config.lock().unwrap().clone();
        let history = state.messages.lock().unwrap().clone();
        let sys = build_system_prompt(&cfg);

        state.emit("status", json!({ "state": "thinking", "step": step + 1, "provider": cfg.provider, "model": cfg.model }));

        let res = match cfg.provider.as_str() {
            "anthropic" => call_anthropic_messages(&state, &cfg, &sys, &history),
            "claude-cli" => call_claude_cli(&cfg, &history),
            _ => call_ollama_stream(&state, &cfg, &sys, &history),
        };

        let out = match res {
            Ok(o) => o,
            Err(err) => {
                state.emit("error", json!({ "message": err }));
                break;
            }
        };

        if out.prompt_tokens > 0 || out.output_tokens > 0 {
            let mut u = state.usage.lock().unwrap();
            if out.prompt_tokens > 0 {
                u.prompt_tokens = out.prompt_tokens;
            }
            u.output_tokens = out.output_tokens;
            u.cached_tokens = out.cached_tokens;
        }

        let msg = ChatMessage {
            role: "assistant".into(),
            content: out.content.clone(),
            thinking: out.thinking.clone(),
            tool_calls: out.tool_calls.clone(),
            tool_name: String::new(),
            tool_call_id: String::new(),
        };
        state.messages.lock().unwrap().push(msg);
        state.usage.lock().unwrap().estimated_history_tokens = state.estimate_history_tokens();
        emit_usage(&state);
        state.emit(
            "assistant_done",
            json!({
                "content": out.content,
                "thinking": out.thinking,
                "tool_calls": out.tool_calls
            }),
        );

        if out.tool_calls.is_empty() {
            break;
        }

        for tc in out.tool_calls {
            if state.cancel.load(Ordering::SeqCst) {
                break;
            }
            if cfg.mode == "plan-first" && is_mutating_tool(&tc.name) {
                let approved = wait_for_approval(&state, &tc);
                if !approved {
                    let result = "Tool execution rejected or cancelled by user in plan-first mode.".to_string();
                    push_tool_result(&state, &tc, &result);
                    continue;
                }
            }
            state.emit(
                "tool_start",
                json!({ "id": tc.id, "name": tc.name, "arguments": tc.arguments }),
            );
            let result = execute_tool(&tc.name, &tc.arguments, &cfg, &state.http);
            push_tool_result(&state, &tc, &result);
        }
    }

    *state.running.lock().unwrap() = false;
    state.emit("status", json!({ "state": "idle" }));
}

fn push_tool_result(state: &Arc<AppState>, tc: &ToolCallRecord, result: &str) {
    state.messages.lock().unwrap().push(ChatMessage {
        role: "tool".into(),
        content: result.to_string(),
        thinking: String::new(),
        tool_calls: Vec::new(),
        tool_name: tc.name.clone(),
        tool_call_id: tc.id.clone(),
    });
    state.usage.lock().unwrap().estimated_history_tokens = state.estimate_history_tokens();
    emit_usage(state);
    state.emit(
        "tool_result",
        json!({ "id": tc.id, "name": tc.name, "result": result }),
    );
}

fn wait_for_approval(state: &Arc<AppState>, tc: &ToolCallRecord) -> bool {
    let snap = ApprovalSnapshot {
        id: tc.id.clone(),
        tool: tc.name.clone(),
        summary: summarize_tool_call(&tc.name, &tc.arguments),
        arguments: tc.arguments.clone(),
    };
    {
        let mut guard = state.approval.lock().unwrap();
        *guard = Some(PendingApproval {
            snapshot: snap.clone(),
            decision: None,
        });
    }
    state.emit("approval_needed", serde_json::to_value(&snap).unwrap_or(json!({})));

    let mut guard = state.approval.lock().unwrap();
    loop {
        if state.cancel.load(Ordering::SeqCst) {
            *guard = None;
            return false;
        }
        if let Some(pending) = guard.as_ref() {
            if let Some(dec) = pending.decision {
                *guard = None;
                return dec;
            }
        } else {
            return false;
        }
        let (g, _) = state
            .approval_cv
            .wait_timeout(guard, std::time::Duration::from_millis(500))
            .unwrap();
        guard = g;
    }
}

fn build_system_prompt(cfg: &AgentConfig) -> String {
    let mode_rule = if cfg.mode == "plan-first" {
        "MODE: PLAN-FIRST. Explore read-only with list_files/read_file/grep_files first, reason carefully, and explain your concise plan before calling write_file, edit_file, or run_command (mutating calls require user approval)."
    } else {
        "MODE: AUTO-CODING. Explore first, state a concise plan, then execute edits and verify with real builds/tests (C++/Python/Rust). Keep terminal output bounded."
    };
    let mut s = format!(
        "You are PenguinAgent, a local coding agent running in workspace: `{}`.\n{}\nTools available: list_files, read_file, grep_files, write_file, edit_file, run_command, ask_claude.\nAlways inspect before editing and verify edits by compiling or running tests.",
        cfg.workspace, mode_rule
    );
    if !cfg.instructions.trim().is_empty() {
        s.push_str("\n\n## Workspace Instructions (AGENTS.md)\n");
        s.push_str(cfg.instructions.trim());
    }
    s
}

fn call_ollama_stream(
    state: &Arc<AppState>,
    cfg: &AgentConfig,
    sys: &str,
    history: &[ChatMessage],
) -> Result<StepOutput, String> {
    let mut api_msgs = vec![json!({ "role": "system", "content": sys })];
    for m in history {
        match m.role.as_str() {
            "user" => api_msgs.push(json!({ "role": "user", "content": m.content })),
            "assistant" => {
                let mut obj = json!({ "role": "assistant", "content": m.content });
                if !m.tool_calls.is_empty() {
                    let calls: Vec<Value> = m
                        .tool_calls
                        .iter()
                        .enumerate()
                        .map(|(idx, tc)| {
                            json!({
                                "function": {
                                    "index": idx,
                                    "name": tc.name,
                                    "arguments": tc.arguments
                                }
                            })
                        })
                        .collect();
                    obj.as_object_mut()
                        .unwrap()
                        .insert("tool_calls".into(), Value::Array(calls));
                }
                api_msgs.push(obj);
            }
            "tool" => {
                let mut obj = json!({ "role": "tool", "content": m.content });
                if !m.tool_name.is_empty() {
                    obj.as_object_mut()
                        .unwrap()
                        .insert("tool_name".into(), json!(m.tool_name));
                }
                api_msgs.push(obj);
            }
            _ => {}
        }
    }

    let think_val = if cfg.think == "off" {
        json!(false)
    } else {
        json!(cfg.think)
    };
    let payload = json!({
        "model": cfg.model,
        "stream": true,
        "think": think_val,
        "options": {
            "num_ctx": cfg.num_ctx,
            "num_predict": cfg.max_tokens
        },
        "messages": api_msgs,
        "tools": ollama_tool_schemas()
    });

    let url = format!("{}/api/chat", cfg.ollama_url.trim_end_matches('/'));
    let resp = state
        .http
        .post(&url)
        .send_json(&payload)
        .map_err(|e| format!("Ollama request failed ({url}): {e}"))?;

    let status = resp.status().as_u16();
    if status >= 400 {
        let mut body = resp.into_body();
        let text = body.read_to_string().unwrap_or_default();
        return Err(format!("Ollama HTTP {status}: {text}"));
    }

    let reader = BufReader::new(resp.into_body().into_reader());
    let mut content = String::new();
    let mut thinking = String::new();
    let mut tool_calls = Vec::new();
    let mut prompt_tokens = 0;
    let mut output_tokens = 0;
    let mut cached_tokens = 0;

    for line_res in reader.lines() {
        if state.cancel.load(Ordering::SeqCst) {
            break;
        }
        let line = line_res.map_err(|e| format!("Stream read error: {e}"))?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let chunk: Value = serde_json::from_str(trimmed)
            .map_err(|e| format!("Invalid Ollama stream JSON: {e}"))?;
        if let Some(err) = chunk["error"].as_str() {
            return Err(format!("Ollama error: {err}"));
        }
        if let Some(t) = chunk["message"]["thinking"].as_str()
            && !t.is_empty()
        {
            thinking.push_str(t);
            state.emit("thinking_delta", json!({ "delta": t }));
        }
        if let Some(c) = chunk["message"]["content"].as_str()
            && !c.is_empty()
        {
            content.push_str(c);
            state.emit("text_delta", json!({ "delta": c }));
        }
        if let Some(arr) = chunk["message"]["tool_calls"].as_array() {
            for (i, tc) in arr.iter().enumerate() {
                let name = tc["function"]["name"].as_str().unwrap_or("").to_string();
                if name.is_empty() {
                    continue;
                }
                let id = tc["id"]
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("call_{}", tool_calls.len() + i));
                let arguments = match &tc["function"]["arguments"] {
                    Value::String(s) => serde_json::from_str(s).unwrap_or_else(|_| json!({})),
                    other => other.clone(),
                };
                tool_calls.push(ToolCallRecord {
                    id,
                    name,
                    arguments,
                });
            }
        }
        if chunk["done"].as_bool() == Some(true) {
            prompt_tokens = chunk["prompt_eval_count"].as_u64().unwrap_or(0) as usize;
            output_tokens = chunk["eval_count"].as_u64().unwrap_or(0) as usize;
            cached_tokens = chunk["prompt_eval_cached_count"].as_u64().unwrap_or(0) as usize;
        }
    }

    Ok(StepOutput {
        content,
        thinking,
        tool_calls,
        prompt_tokens,
        output_tokens,
        cached_tokens,
    })
}

fn call_anthropic_messages(
    state: &Arc<AppState>,
    cfg: &AgentConfig,
    sys: &str,
    history: &[ChatMessage],
) -> Result<StepOutput, String> {
    let key = std::env::var("ANTHROPIC_API_KEY")
        .map_err(|_| "ANTHROPIC_API_KEY is not set; switch provider to `ollama` or `claude-cli`.".to_string())?;
    let key = key.trim();
    if key.is_empty() {
        return Err("ANTHROPIC_API_KEY is empty.".into());
    }

    let mut api_msgs: Vec<Value> = Vec::new();
    for m in history {
        match m.role.as_str() {
            "user" => api_msgs.push(json!({ "role": "user", "content": m.content })),
            "assistant" => {
                let mut blocks = Vec::new();
                if !m.content.is_empty() {
                    blocks.push(json!({ "type": "text", "text": m.content }));
                }
                for tc in &m.tool_calls {
                    blocks.push(json!({
                        "type": "tool_use",
                        "id": tc.id,
                        "name": tc.name,
                        "input": tc.arguments
                    }));
                }
                if blocks.is_empty() {
                    blocks.push(json!({ "type": "text", "text": "" }));
                }
                api_msgs.push(json!({ "role": "assistant", "content": blocks }));
            }
            "tool" => {
                let block = json!({
                    "type": "tool_result",
                    "tool_use_id": m.tool_call_id,
                    "content": m.content
                });
                // Merge consecutive tool_result blocks into one user message for Anthropic API
                if let Some(last) = api_msgs.last_mut()
                    && last["role"] == "user"
                    && last["content"].is_array()
                {
                    last["content"].as_array_mut().unwrap().push(block);
                    continue;
                }
                api_msgs.push(json!({ "role": "user", "content": [block] }));
            }
            _ => {}
        }
    }

    let max_out = cfg.max_tokens.min(64_000);
    let mut body = json!({
        "model": cfg.claude_model,
        "max_tokens": max_out,
        "system": sys,
        "messages": api_msgs,
        "tools": anthropic_tool_schemas()
    });
    if cfg.think != "off" {
        let budget = match cfg.think.as_str() {
            "low" => 2048,
            "high" => 12_000,
            _ => 5000,
        };
        if max_out > budget + 1024 {
            body.as_object_mut().unwrap().insert(
                "thinking".into(),
                json!({ "type": "enabled", "budget_tokens": budget }),
            );
        }
    }

    let mut resp = state
        .http
        .post("https://api.anthropic.com/v1/messages")
        .header("x-api-key", key)
        .header("anthropic-version", "2023-06-01")
        .send_json(&body)
        .map_err(|e| format!("Anthropic request failed: {e}"))?;

    let status = resp.status().as_u16();
    let text = resp.body_mut().read_to_string().unwrap_or_default();
    if status >= 400 {
        return Err(format!("Anthropic HTTP {status}: {text}"));
    }
    let v: Value = serde_json::from_str(&text).map_err(|e| format!("Invalid Anthropic JSON: {e}"))?;
    let mut content = String::new();
    let mut thinking = String::new();
    let mut tool_calls = Vec::new();
    if let Some(blocks) = v["content"].as_array() {
        for b in blocks {
            match b["type"].as_str().unwrap_or("") {
                "thinking" => {
                    if let Some(t) = b["thinking"].as_str() {
                        thinking.push_str(t);
                        state.emit("thinking_delta", json!({ "delta": t }));
                    }
                }
                "text" => {
                    if let Some(t) = b["text"].as_str() {
                        content.push_str(t);
                        state.emit("text_delta", json!({ "delta": t }));
                    }
                }
                "tool_use" => {
                    tool_calls.push(ToolCallRecord {
                        id: b["id"].as_str().unwrap_or("call_claude").into(),
                        name: b["name"].as_str().unwrap_or("").into(),
                        arguments: b["input"].clone(),
                    });
                }
                _ => {}
            }
        }
    }
    Ok(StepOutput {
        content,
        thinking,
        tool_calls,
        prompt_tokens: v["usage"]["input_tokens"].as_u64().unwrap_or(0) as usize,
        output_tokens: v["usage"]["output_tokens"].as_u64().unwrap_or(0) as usize,
        cached_tokens: v["usage"]["cache_read_input_tokens"].as_u64().unwrap_or(0) as usize,
    })
}

fn call_claude_cli(cfg: &AgentConfig, history: &[ChatMessage]) -> Result<StepOutput, String> {
    let last_user = history
        .iter()
        .rev()
        .find(|m| m.role == "user")
        .map(|m| m.content.as_str())
        .unwrap_or("");
    let recent: String = history
        .iter()
        .rev()
        .take(6)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .map(|m| format!("[{}]: {}", m.role, truncate_utf8(&m.content, 1200)))
        .collect::<Vec<_>>()
        .join("\n\n");
    let prompt = format!(
        "Workspace: {}\nMode: {}\nRecent context:\n{}\n\nLatest request:\n{}",
        cfg.workspace, cfg.mode, recent, last_user
    );
    let reply = ask_claude_text(&PathBuf::from(&cfg.workspace), &prompt, cfg, &ureq::Agent::new_with_defaults());
    Ok(StepOutput {
        content: reply,
        thinking: String::new(),
        tool_calls: Vec::new(),
        prompt_tokens: 0,
        output_tokens: 0,
        cached_tokens: 0,
    })
}

fn maybe_auto_compact(state: &Arc<AppState>) {
    let (num_ctx, max_tokens, ratio) = {
        let cfg = state.config.lock().unwrap();
        (cfg.num_ctx, cfg.max_tokens, cfg.auto_compact_ratio)
    };
    let (prompt_tokens, est_tokens) = {
        let u = state.usage.lock().unwrap();
        (u.prompt_tokens, u.estimated_history_tokens)
    };
    let active_used = prompt_tokens.max(est_tokens);
    let safe_budget = num_ctx.saturating_sub(max_tokens.min(num_ctx / 2));
    let threshold = ((num_ctx as f64) * ratio) as usize;
    let trigger = threshold.min(safe_budget.saturating_sub(4096)).max(4096);
    if active_used >= trigger && state.messages.lock().unwrap().len() > 6 {
        let _ = compact_history(state);
    }
}

pub fn compact_history(state: &Arc<AppState>) -> String {
    let (older, kept) = {
        let msgs = state.messages.lock().unwrap();
        if msgs.len() <= 4 {
            return "History is already compact (<= 4 messages).".into();
        }
        let split = msgs.len() - 4;
        (msgs[..split].to_vec(), msgs[split..].to_vec())
    };

    let transcript: String = older
        .iter()
        .map(|m| {
            format!(
                "[{}]: {}",
                m.role,
                truncate_utf8(&m.content, 1200)
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    let cfg = state.config.lock().unwrap().clone();
    let summary_prompt = format!(
        "Summarize this coding session concisely for continuing work. Preserve: goal, modified file paths, exact errors/findings, and next step.\n\n{}",
        truncate_utf8(&transcript, 24_000)
    );

    let summary = summarize_with_ollama(&state.http, &cfg, &summary_prompt)
        .unwrap_or_else(|_| truncate_utf8(&transcript, 2500));

    let mut next_msgs = vec![ChatMessage {
        role: "user".into(),
        content: format!("[Compacted Earlier Context]\n{summary}"),
        thinking: String::new(),
        tool_calls: Vec::new(),
        tool_name: String::new(),
        tool_call_id: String::new(),
    }];
    next_msgs.extend(kept);

    let count = next_msgs.len();
    *state.messages.lock().unwrap() = next_msgs;
    {
        let mut u = state.usage.lock().unwrap();
        u.prompt_tokens = 0;
        u.estimated_history_tokens = state.estimate_history_tokens();
    }
    emit_usage(state);
    state.emit("compacted", json!({ "summary": summary, "kept": count }));
    summary
}

fn summarize_with_ollama(
    http: &ureq::Agent,
    cfg: &AgentConfig,
    prompt: &str,
) -> Result<String, String> {
    let url = format!("{}/api/chat", cfg.ollama_url.trim_end_matches('/'));
    let payload = json!({
        "model": cfg.model,
        "stream": false,
        "think": false,
        "options": { "num_ctx": cfg.num_ctx.min(32_768), "num_predict": 1024 },
        "messages": [{ "role": "user", "content": prompt }]
    });
    let mut resp = http
        .post(&url)
        .send_json(&payload)
        .map_err(|e| e.to_string())?;
    let v: Value = resp.body_mut().read_json().map_err(|e| e.to_string())?;
    let text = v["message"]["content"].as_str().unwrap_or("").trim().to_string();
    if text.is_empty() {
        Err("Empty summary".into())
    } else {
        Ok(text)
    }
}

pub fn escalate_to_claude(state: &Arc<AppState>, note: &str) -> String {
    let cfg = state.config.lock().unwrap().clone();
    let recent: String = {
        let msgs = state.messages.lock().unwrap();
        msgs.iter()
            .rev()
            .take(6)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .map(|m| format!("[{}]: {}", m.role, truncate_utf8(&m.content, 1500)))
            .collect::<Vec<_>>()
            .join("\n\n")
    };
    let prompt = format!(
        "Workspace: {}\nNote from user: {}\n\nRecent agent transcript:\n{}",
        cfg.workspace,
        if note.trim().is_empty() { "Diagnose the blocker and propose exact minimal fixes." } else { note },
        recent
    );
    let advice = ask_claude_text(&PathBuf::from(&cfg.workspace), &prompt, &cfg, &state.http);
    {
        state.messages.lock().unwrap().push(ChatMessage {
            role: "user".into(),
            content: format!("[Claude Brain Guidance]\n{advice}"),
            thinking: String::new(),
            tool_calls: Vec::new(),
            tool_name: String::new(),
            tool_call_id: String::new(),
        });
        state.usage.lock().unwrap().estimated_history_tokens = state.estimate_history_tokens();
    }
    emit_usage(state);
    state.emit("claude_brain", json!({ "advice": advice }));
    advice
}

fn emit_usage(state: &Arc<AppState>) {
    let u: TokenUsage = state.usage.lock().unwrap().clone();
    let cfg = state.config.lock().unwrap();
    state.emit(
        "usage",
        json!({
            "prompt_tokens": u.prompt_tokens,
            "output_tokens": u.output_tokens,
            "cached_tokens": u.cached_tokens,
            "estimated_history_tokens": u.estimated_history_tokens,
            "num_ctx": cfg.num_ctx,
            "max_tokens": cfg.max_tokens
        }),
    );
}
