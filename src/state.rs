use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::hash_map::RandomState;
use std::fs;
use std::hash::{BuildHasher, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentConfig {
    pub provider: String, // "ollama" | "anthropic" | "claude-cli"
    pub ollama_url: String,
    pub model: String,
    pub claude_model: String,
    pub num_ctx: usize,
    pub max_tokens: usize,
    pub think: String, // "off" | "low" | "medium" | "high"
    pub mode: String,  // "plan-first" | "auto"
    pub workspace: String,
    pub instructions: String,
    pub auto_compact_ratio: f64,
}

impl AgentConfig {
    pub fn from_env(workspace: PathBuf) -> Self {
        let num_ctx = env_usize("AGENT_CTX", 131_072).clamp(4096, 200_000);
        let max_tokens = clamp_max_tokens(env_usize("AGENT_MAX_TOKENS", 69_632), num_ctx);
        let instructions = load_agents_md(&workspace);
        Self {
            provider: std::env::var("AGENT_PROVIDER").unwrap_or_else(|_| "ollama".into()),
            ollama_url: normalize_ollama_url(
                &std::env::var("OLLAMA_HOST").unwrap_or_else(|_| "http://127.0.0.1:11434".into()),
            ),
            model: std::env::var("AGENT_MODEL").unwrap_or_else(|_| "qwen3.5:latest".into()),
            claude_model: std::env::var("CLAUDE_MODEL")
                .unwrap_or_else(|_| "claude-sonnet-4-6".into()),
            num_ctx,
            max_tokens,
            think: std::env::var("AGENT_THINK").unwrap_or_else(|_| "medium".into()),
            mode: std::env::var("AGENT_MODE").unwrap_or_else(|_| "plan-first".into()),
            workspace: clean_path_string(&workspace),
            instructions,
            auto_compact_ratio: 0.78,
        }
    }

    pub fn normalize(&mut self) {
        self.num_ctx = self.num_ctx.clamp(4096, 200_000);
        self.max_tokens = clamp_max_tokens(self.max_tokens, self.num_ctx);
        self.ollama_url = normalize_ollama_url(&self.ollama_url);
        if !matches!(self.provider.as_str(), "ollama" | "anthropic" | "claude-cli") {
            self.provider = "ollama".into();
        }
        if !matches!(self.think.as_str(), "off" | "low" | "medium" | "high") {
            self.think = "medium".into();
        }
        if !matches!(self.mode.as_str(), "plan-first" | "auto") {
            self.mode = "plan-first".into();
        }
        self.auto_compact_ratio = self.auto_compact_ratio.clamp(0.4, 0.95);
    }
}

pub fn clamp_max_tokens(requested: usize, num_ctx: usize) -> usize {
    let cap = ((num_ctx as f64) * 0.60) as usize;
    requested.max(2048).min(cap.max(2048))
}

pub fn normalize_ollama_url(raw: &str) -> String {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return "http://127.0.0.1:11434".into();
    }
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        trimmed.to_string()
    } else {
        format!("http://{trimmed}")
    }
}

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(default)
}

pub fn clean_path_string(p: &Path) -> String {
    let s = p.to_string_lossy();
    s.strip_prefix(r"\\?\").unwrap_or(&s).to_string()
}

pub fn load_agents_md(workspace: &Path) -> String {
    for name in ["AGENTS.md", "CLAUDE.md"] {
        let p = workspace.join(name);
        if let Ok(text) = fs::read_to_string(&p) {
            return truncate_utf8(&text, 32_768);
        }
    }
    String::new()
}

pub fn truncate_utf8(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n...[truncated]", &s[..end])
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolCallRecord {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String, // "user" | "assistant" | "tool"
    pub content: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub thinking: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCallRecord>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub tool_name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub tool_call_id: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TokenUsage {
    pub prompt_tokens: usize,
    pub output_tokens: usize,
    pub cached_tokens: usize,
    pub estimated_history_tokens: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct ApprovalSnapshot {
    pub id: String,
    pub tool: String,
    pub summary: String,
    pub arguments: Value,
}

pub struct PendingApproval {
    pub snapshot: ApprovalSnapshot,
    pub decision: Option<bool>,
}

pub struct AppState {
    pub token: String,
    pub config: Mutex<AgentConfig>,
    pub messages: Mutex<Vec<ChatMessage>>,
    pub usage: Mutex<TokenUsage>,
    pub running: Mutex<bool>,
    pub cancel: Arc<AtomicBool>,
    pub approval: Mutex<Option<PendingApproval>>,
    pub approval_cv: Condvar,
    pub subscribers: Mutex<Vec<Sender<String>>>,
    pub http: ureq::Agent,
}

impl AppState {
    pub fn new(token: String, config: AgentConfig) -> Arc<Self> {
        let http_cfg = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(std::time::Duration::from_secs(600)))
            .build();
        Arc::new(Self {
            token,
            config: Mutex::new(config),
            messages: Mutex::new(Vec::new()),
            usage: Mutex::new(TokenUsage::default()),
            running: Mutex::new(false),
            cancel: Arc::new(AtomicBool::new(false)),
            approval: Mutex::new(None),
            approval_cv: Condvar::new(),
            subscribers: Mutex::new(Vec::new()),
            http: http_cfg.into(),
        })
    }

    pub fn emit(&self, event: &str, data: Value) {
        let payload = format!("event: {event}\ndata: {}\n\n", data);
        let mut subs = self.subscribers.lock().unwrap();
        subs.retain(|tx| tx.send(payload.clone()).is_ok());
    }

    pub fn estimate_history_tokens(&self) -> usize {
        let cfg = self.config.lock().unwrap();
        let msgs = self.messages.lock().unwrap();
        let instr_chars = cfg.instructions.len();
        let msg_chars: usize = msgs
            .iter()
            .map(|m| m.content.len() + m.thinking.len() + m.tool_name.len() + 64)
            .sum();
        (instr_chars + msg_chars + 1500) / 4
    }
}

pub fn load_or_create_token() -> String {
    if let Ok(tok) = std::env::var("PENGUIN_TOKEN") {
        let t = tok.trim().to_string();
        if !t.is_empty() {
            return t;
        }
    }
    if let Some(home) = home_dir() {
        let token_file = home.join(".penguin-agent-token");
        if let Ok(existing) = fs::read_to_string(&token_file) {
            let t = existing.trim().to_string();
            if t.len() >= 16 {
                return t;
            }
        }
        let generated = random_hex_token();
        let _ = fs::write(&token_file, &generated);
        return generated;
    }
    random_hex_token()
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
}

fn random_hex_token() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut h1 = RandomState::new().build_hasher();
    h1.write_u128(nanos);
    h1.write_u32(std::process::id());
    let mut h2 = RandomState::new().build_hasher();
    h2.write_u64(h1.finish());
    format!("{:016x}{:016x}", h1.finish(), h2.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamps_context_and_output_tokens_safely() {
        assert_eq!(clamp_max_tokens(69_632, 131_072), 69_632);
        assert_eq!(clamp_max_tokens(69_632, 196_608), 69_632);
        assert_eq!(clamp_max_tokens(69_632, 65_536), 39_321);
        assert_eq!(normalize_ollama_url("192.168.10.111:11434/"), "http://192.168.10.111:11434");
    }

    #[test]
    fn utf8_truncation_respects_char_boundaries() {
        let s = "hello 🚀 world";
        let out = truncate_utf8(s, 8);
        assert!(out.starts_with("hello "));
        assert!(out.ends_with("...[truncated]"));
    }
}
