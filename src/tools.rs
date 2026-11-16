use crate::state::{truncate_utf8, AgentConfig};
use serde_json::{json, Value};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

const SKIP_DIRS: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    "__pycache__",
    ".pytest_cache",
    "build",
    "dist",
    "out",
];

pub fn ollama_tool_schemas() -> Value {
    json!([
        tool_schema("list_files", "List workspace files (ignores .git, target, node_modules). Optional subdir and max_results.", json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Relative directory path (default '.')" },
                "max_results": { "type": "integer", "description": "Max entries (default 120)" }
            }
        }), &[]),
        tool_schema("read_file", "Read a bounded line range of a workspace file (max 200 lines per call).", json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Relative path inside workspace" },
                "start_line": { "type": "integer", "description": "1-based start line (default 1)" },
                "end_line": { "type": "integer", "description": "1-based inclusive end line (default start_line + 149)" }
            }
        }), &["path"]),
        tool_schema("grep_files", "Search workspace text files for a case-sensitive substring (max 60 matches).", json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Exact substring to find" },
                "path": { "type": "string", "description": "Optional relative subpath (default '.')" }
            }
        }), &["query"]),
        tool_schema("write_file", "Create or overwrite a file inside the workspace.", json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Relative file path inside workspace" },
                "content": { "type": "string", "description": "Full UTF-8 file contents" }
            }
        }), &["path", "content"]),
        tool_schema("edit_file", "Replace an exact unique string target inside an existing workspace file.", json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Relative file path inside workspace" },
                "target": { "type": "string", "description": "Exact existing text to replace" },
                "replacement": { "type": "string", "description": "Replacement text" }
            }
        }), &["path", "target", "replacement"]),
        tool_schema("run_command", "Execute a shell command in the workspace (PowerShell on Windows, sh on macOS/Linux). Output is bounded to protect context.", json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "Command to execute" }
            }
        }), &["command"]),
        tool_schema("ask_claude", "Escalate a hard bug, UB, borrow-checker error, or architecture review to Claude Brain (API or local claude CLI).", json!({
            "type": "object",
            "properties": {
                "prompt": { "type": "string", "description": "Concise goal, exact error message, and relevant code snippets/paths" }
            }
        }), &["prompt"])
    ])
}

pub fn anthropic_tool_schemas() -> Vec<Value> {
    ollama_tool_schemas()
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|t| {
            let f = t.get("function")?;
            Some(json!({
                "name": f.get("name")?,
                "description": f.get("description")?,
                "input_schema": f.get("parameters")?,
            }))
        })
        .collect()
}

fn tool_schema(name: &str, description: &str, parameters: Value, required: &[&str]) -> Value {
    let mut params = parameters;
    if let Some(obj) = params.as_object_mut() {
        obj.insert("required".into(), json!(required));
    }
    json!({
        "type": "function",
        "function": {
            "name": name,
            "description": description,
            "parameters": params
        }
    })
}

pub fn is_mutating_tool(name: &str) -> bool {
    matches!(name, "write_file" | "edit_file" | "run_command")
}

pub fn summarize_tool_call(name: &str, args: &Value) -> String {
    match name {
        "write_file" => format!("Write {}", args["path"].as_str().unwrap_or("?")),
        "edit_file" => format!("Edit {}", args["path"].as_str().unwrap_or("?")),
        "run_command" => format!(
            "Run: {}",
            truncate_utf8(args["command"].as_str().unwrap_or(""), 120)
        ),
        _ => name.to_string(),
    }
}

pub fn execute_tool(
    name: &str,
    args: &Value,
    cfg: &AgentConfig,
    http: &ureq::Agent,
) -> String {
    let ws = PathBuf::from(&cfg.workspace);
    match name {
        "list_files" => tool_list_files(&ws, args),
        "read_file" => tool_read_file(&ws, args),
        "grep_files" => tool_grep_files(&ws, args),
        "write_file" => tool_write_file(&ws, args),
        "edit_file" => tool_edit_file(&ws, args),
        "run_command" => tool_run_command(&ws, args),
        "ask_claude" => tool_ask_claude(&ws, args, cfg, http),
        _ => format!("Unknown tool: {name}"),
    }
}

fn resolve_workspace_path(ws: &Path, raw: &str, must_exist: bool) -> Result<PathBuf, String> {
    let ws_canon = ws
        .canonicalize()
        .map_err(|e| format!("Invalid workspace {}: {e}", ws.display()))?;
    let rel = Path::new(raw.trim());
    if rel.is_absolute() {
        return Err("Absolute paths outside relative workspace syntax are rejected; pass a workspace-relative path.".into());
    }
    let mut clean = PathBuf::new();
    for comp in rel.components() {
        match comp {
            Component::CurDir => {}
            Component::Normal(c) => clean.push(c),
            Component::ParentDir => {
                if !clean.pop() {
                    return Err("Path escapes workspace root.".into());
                }
            }
            Component::Prefix(_) | Component::RootDir => {
                return Err("Path must be relative to workspace.".into());
            }
        }
    }
    let full = ws_canon.join(&clean);
    if must_exist {
        let canon = full
            .canonicalize()
            .map_err(|e| format!("Cannot resolve {}: {e}", clean.display()))?;
        if !canon.starts_with(&ws_canon) {
            return Err("Path escapes workspace root.".into());
        }
        Ok(canon)
    } else {
        Ok(full)
    }
}

fn tool_list_files(ws: &Path, args: &Value) -> String {
    let sub = args["path"].as_str().unwrap_or(".");
    let max_results = args["max_results"].as_u64().unwrap_or(120).min(300) as usize;
    let start = match resolve_workspace_path(ws, sub, true) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let ws_canon = ws.canonicalize().unwrap_or_else(|_| ws.to_path_buf());
    let mut out = Vec::new();
    walk_dir(&start, &ws_canon, &mut out, max_results);
    if out.is_empty() {
        "(no files found)".into()
    } else {
        out.join("\n")
    }
}

fn walk_dir(dir: &Path, root: &Path, out: &mut Vec<String>, limit: usize) {
    if out.len() >= limit {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut items: Vec<_> = entries.flatten().collect();
    items.sort_by_key(|e| e.file_name());
    for entry in items {
        if out.len() >= limit {
            break;
        }
        let path = entry.path();
        let name = entry.file_name();
        let name_s = name.to_string_lossy();
        if path.is_dir() {
            if SKIP_DIRS.contains(&name_s.as_ref()) {
                continue;
            }
            walk_dir(&path, root, out, limit);
        } else if let Ok(rel) = path.strip_prefix(root) {
            out.push(rel.to_string_lossy().replace('\\', "/"));
        }
    }
}

fn tool_read_file(ws: &Path, args: &Value) -> String {
    let Some(rel) = args["path"].as_str() else {
        return "Missing required parameter: path".into();
    };
    let path = match resolve_workspace_path(ws, rel, true) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let Ok(text) = fs::read_to_string(&path) else {
        return format!("Failed to read UTF-8 text from {rel}");
    };
    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();
    let start = args["start_line"].as_u64().unwrap_or(1).max(1) as usize;
    let end = args["end_line"]
        .as_u64()
        .map(|e| e as usize)
        .unwrap_or(start + 149)
        .min(start + 199)
        .min(total);
    if start > total {
        return format!("{rel} has {total} lines (requested start_line={start}).");
    }
    let mut out = format!("# {rel} (lines {start}-{end} of {total})\n");
    for (idx, line) in lines.iter().enumerate().take(end).skip(start - 1) {
        out.push_str(&format!("{}: {}\n", idx + 1, line));
    }
    truncate_utf8(&out, 24_576)
}

fn tool_grep_files(ws: &Path, args: &Value) -> String {
    let Some(query) = args["query"].as_str() else {
        return "Missing required parameter: query".into();
    };
    if query.is_empty() {
        return "query must not be empty".into();
    }
    let sub = args["path"].as_str().unwrap_or(".");
    let start = match resolve_workspace_path(ws, sub, true) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let ws_canon = ws.canonicalize().unwrap_or_else(|_| ws.to_path_buf());
    let mut files = Vec::new();
    walk_dir(&start, &ws_canon, &mut files, 1000);
    let mut hits = Vec::new();
    for rel in files {
        if hits.len() >= 60 {
            break;
        }
        let full = ws_canon.join(&rel);
        let Ok(meta) = fs::metadata(&full) else { continue };
        if meta.len() > 1_048_576 {
            continue;
        }
        let Ok(text) = fs::read_to_string(&full) else { continue };
        for (i, line) in text.lines().enumerate() {
            if line.contains(query) {
                hits.push(format!("{}:{}: {}", rel, i + 1, truncate_utf8(line.trim(), 200)));
                if hits.len() >= 60 {
                    break;
                }
            }
        }
    }
    if hits.is_empty() {
        format!("No matches for `{query}`.")
    } else {
        hits.join("\n")
    }
}

fn tool_write_file(ws: &Path, args: &Value) -> String {
    let (Some(rel), Some(content)) = (args["path"].as_str(), args["content"].as_str()) else {
        return "Missing required parameters: path, content".into();
    };
    let path = match resolve_workspace_path(ws, rel, false) {
        Ok(p) => p,
        Err(e) => return e,
    };
    if let Some(parent) = path.parent()
        && let Err(e) = fs::create_dir_all(parent)
    {
        return format!("Failed to create parent directories: {e}");
    }
    match fs::write(&path, content) {
        Ok(()) => format!("Wrote {} bytes to {rel}", content.len()),
        Err(e) => format!("Failed to write {rel}: {e}"),
    }
}

fn tool_edit_file(ws: &Path, args: &Value) -> String {
    let (Some(rel), Some(target), Some(replacement)) = (
        args["path"].as_str(),
        args["target"].as_str(),
        args["replacement"].as_str(),
    ) else {
        return "Missing required parameters: path, target, replacement".into();
    };
    if target.is_empty() {
        return "target must not be empty".into();
    }
    let path = match resolve_workspace_path(ws, rel, true) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let Ok(text) = fs::read_to_string(&path) else {
        return format!("Failed to read {rel}");
    };
    let count = text.matches(target).count();
    if count == 0 {
        return format!("Target text not found in {rel}. Read the file first and match whitespace exactly.");
    }
    if count > 1 {
        return format!("Target text matched {count} times in {rel}; include more context to make it unique.");
    }
    let updated = text.replacen(target, replacement, 1);
    match fs::write(&path, updated) {
        Ok(()) => format!("Updated {rel} (1 replacement)."),
        Err(e) => format!("Failed to write {rel}: {e}"),
    }
}

pub fn run_shell_in_workspace(ws: &Path, command: &str) -> String {
    let output = if cfg!(windows) {
        Command::new("powershell")
            .args(["-NoProfile", "-Command", command])
            .current_dir(ws)
            .output()
    } else {
        Command::new("sh")
            .args(["-lc", command])
            .current_dir(ws)
            .output()
    };
    match output {
        Ok(out) => {
            let code = out.status.code().unwrap_or(-1);
            let mut combined = String::from_utf8_lossy(&out.stdout).into_owned();
            let err = String::from_utf8_lossy(&out.stderr);
            if !err.trim().is_empty() {
                if !combined.is_empty() && !combined.ends_with('\n') {
                    combined.push('\n');
                }
                combined.push_str(&err);
            }
            let bounded = bound_lines(&combined, 20, 60);
            format!("exit_code={code}\n{bounded}")
        }
        Err(e) => format!("Failed to execute command: {e}"),
    }
}

fn tool_run_command(ws: &Path, args: &Value) -> String {
    let Some(cmd) = args["command"].as_str() else {
        return "Missing required parameter: command".into();
    };
    run_shell_in_workspace(ws, cmd)
}

fn bound_lines(text: &str, head: usize, tail: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= head + tail {
        return truncate_utf8(text.trim_end(), 8192);
    }
    let omitted = lines.len() - head - tail;
    let mut out = lines[..head].join("\n");
    out.push_str(&format!("\n... [{omitted} lines omitted for context] ...\n"));
    out.push_str(&lines[lines.len() - tail..].join("\n"));
    truncate_utf8(&out, 8192)
}

pub fn run_quick_check(ws: &Path, lang: &str) -> String {
    let cmd = match lang {
        "rust" => "cargo check --message-format=short && cargo test",
        "python" => "python -m compileall -q . && python -m pytest -q",
        "cpp" => {
            if ws.join("CMakeLists.txt").exists() {
                "cmake -B build && cmake --build build"
            } else {
                "echo 'No CMakeLists.txt found; run g++ / clang++ via chat or command.'"
            }
        }
        _ => return format!("Unsupported quick-check language: {lang}"),
    };
    run_shell_in_workspace(ws, cmd)
}

pub fn tool_ask_claude(
    ws: &Path,
    args: &Value,
    cfg: &AgentConfig,
    http: &ureq::Agent,
) -> String {
    let prompt = args["prompt"].as_str().unwrap_or("");
    if prompt.trim().is_empty() {
        return "Missing prompt for Claude Brain.".into();
    }
    ask_claude_text(ws, prompt, cfg, http)
}

pub fn ask_claude_text(
    ws: &Path,
    prompt: &str,
    cfg: &AgentConfig,
    http: &ureq::Agent,
) -> String {
    if let Ok(api_key) = std::env::var("ANTHROPIC_API_KEY") {
        let key = api_key.trim();
        if !key.is_empty() {
            let body = json!({
                "model": cfg.claude_model,
                "max_tokens": 8192,
                "system": "You are the senior Claude Brain advisor helping a local 9B coding agent. Give concise, root-cause diagnosis and exact edits/commands.",
                "messages": [{ "role": "user", "content": prompt }]
            });
            match http
                .post("https://api.anthropic.com/v1/messages")
                .header("x-api-key", key)
                .header("anthropic-version", "2023-06-01")
                .send_json(&body)
            {
                Ok(mut resp) => {
                    let status = resp.status().as_u16();
                    let text = resp.body_mut().read_to_string().unwrap_or_default();
                    if status >= 400 {
                        return format!("Anthropic API error ({status}): {text}");
                    }
                    if let Ok(v) = serde_json::from_str::<Value>(&text) {
                        let joined: String = v["content"]
                            .as_array()
                            .map(|arr| {
                                arr.iter()
                                    .filter_map(|b| b["text"].as_str())
                                    .collect::<Vec<_>>()
                                    .join("\n")
                            })
                            .unwrap_or_default();
                        if !joined.is_empty() {
                            return joined;
                        }
                    }
                    return text;
                }
                Err(e) => return format!("Anthropic API request failed: {e}"),
            }
        }
    }

    // Fallback to signed-in `claude -p` CLI
    match Command::new("claude")
        .args(["-p", prompt])
        .current_dir(ws)
        .output()
    {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
            let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
            if out.status.success() && !stdout.trim().is_empty() {
                truncate_utf8(stdout.trim(), 16_384)
            } else {
                format!(
                    "claude CLI exited {}: {}{}",
                    out.status.code().unwrap_or(-1),
                    stdout,
                    stderr
                )
            }
        }
        Err(e) => format!(
            "Claude unavailable: set ANTHROPIC_API_KEY or sign in to `claude` CLI ({e})."
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_workspace_escape_and_bounds_logs() {
        let cwd = std::env::current_dir().unwrap();
        assert!(resolve_workspace_path(&cwd, "../escape.txt", false).is_err());
        assert!(resolve_workspace_path(&cwd, "/etc/passwd", false).is_err());
        let ok = resolve_workspace_path(&cwd, "Cargo.toml", true);
        assert!(ok.is_ok());

        let big: String = (1..=120).map(|i| format!("line {i}\n")).collect();
        let bounded = bound_lines(&big, 5, 10);
        assert!(bounded.contains("lines omitted for context"));
        assert!(bounded.contains("line 120"));
    }
}
