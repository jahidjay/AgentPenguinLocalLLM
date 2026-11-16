mod llm;
mod server;
mod state;
mod tools;

use serde_json::json;
use state::{load_or_create_token, AgentConfig, AppState};
use std::env;
use std::net::{TcpListener, UdpSocket};
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        print_help();
        return;
    }

    let mut bind_host = env::var("PENGUIN_BIND").unwrap_or_else(|_| "0.0.0.0".into());
    let mut port: u16 = env::var("PENGUIN_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(3080);
    let mut workspace = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut token_override: Option<String> = None;
    let mut open_browser = false;
    let mut remote_url: Option<String> = None;
    let mut cli_prompt: Option<String> = None;
    let mut remote_set: Vec<(String, String)> = Vec::new();

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--bind" => {
                i += 1;
                if let Some(v) = args.get(i) {
                    bind_host = v.clone();
                }
            }
            "--port" => {
                i += 1;
                if let Some(v) = args.get(i).and_then(|s| s.parse().ok()) {
                    port = v;
                }
            }
            "--workspace" | "-w" => {
                i += 1;
                if let Some(v) = args.get(i) {
                    workspace = PathBuf::from(v);
                }
            }
            "--token" => {
                i += 1;
                if let Some(v) = args.get(i) {
                    token_override = Some(v.clone());
                }
            }
            "--open" => open_browser = true,
            "--remote" => {
                i += 1;
                if let Some(v) = args.get(i) {
                    remote_url = Some(v.trim_end_matches('/').to_string());
                }
            }
            "--set" => {
                i += 1;
                if let Some(v) = args.get(i)
                    && let Some((k, val)) = v.split_once('=')
                {
                    remote_set.push((k.trim().to_string(), val.trim().to_string()));
                }
            }
            "--cli" => {
                cli_prompt = Some(args[i + 1..].join(" "));
                break;
            }
            other => {
                if !other.starts_with('-') && cli_prompt.is_none() {
                    cli_prompt = Some(args[i..].join(" "));
                    break;
                }
            }
        }
        i += 1;
    }

    let token = token_override.unwrap_or_else(load_or_create_token);

    if let Some(remote) = remote_url {
        run_remote_client(&remote, &token, &remote_set, cli_prompt.as_deref());
        return;
    }

    let mut cfg = AgentConfig::from_env(workspace.canonicalize().unwrap_or(workspace));
    cfg.normalize();

    if let Some(prompt) = cli_prompt {
        if prompt.trim().is_empty() {
            eprintln!("Provide a prompt after --cli.");
            std::process::exit(1);
        }
        cfg.mode = "auto".into();
        let state = AppState::new(token, cfg);
        llm::run_agent_turn(state.clone(), prompt);
        let msgs = state.messages.lock().unwrap();
        if let Some(last) = msgs.iter().rev().find(|m| m.role == "assistant") {
            println!("{}", last.content);
        }
        return;
    }

    let addr = format!("{bind_host}:{port}");
    let listener = TcpListener::bind(&addr).unwrap_or_else(|e| {
        eprintln!("Failed to bind {addr}: {e}");
        std::process::exit(1);
    });
    let actual_port = listener.local_addr().map(|a| a.port()).unwrap_or(port);

    let local_url = format!("http://127.0.0.1:{actual_port}/?token={token}");
    println!("============================================================");
    println!("PenguinAgent LAN Server");
    println!("  Workspace: {}", cfg.workspace);
    println!("  Model:     {} ({}, ctx={}, max_out={})", cfg.model, cfg.provider, cfg.num_ctx, cfg.max_tokens);
    println!("  Local URL: {local_url}");
    for lan_ip in detect_lan_ips() {
        println!("  Mac / LAN: http://{lan_ip}:{actual_port}/?token={token}");
    }
    println!("============================================================");

    if open_browser {
        open_url(&local_url);
    }

    let state = AppState::new(token, cfg);
    server::serve_http(listener, state);
}

fn run_remote_client(
    remote: &str,
    token: &str,
    sets: &[(String, String)],
    prompt: Option<&str>,
) {
    let http = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .http_status_as_error(false)
            .build(),
    );

    if !sets.is_empty() {
        let mut patch = serde_json::Map::new();
        for (k, v) in sets {
            if k == "num_ctx" || k == "max_tokens" {
                if let Ok(n) = v.parse::<u64>() {
                    patch.insert(k.clone(), json!(n));
                }
            } else {
                patch.insert(k.clone(), json!(v));
            }
        }
        let resp = http
            .post(&format!("{remote}/api/config"))
            .header("Authorization", &format!("Bearer {token}"))
            .send_json(serde_json::Value::Object(patch));
        match resp {
            Ok(mut r) => println!("{}", r.body_mut().read_to_string().unwrap_or_default()),
            Err(e) => {
                eprintln!("Remote config update failed: {e}");
                std::process::exit(1);
            }
        }
    }

    if let Some(p) = prompt {
        let p = p.trim();
        if !p.is_empty() {
            let resp = http
                .post(&format!("{remote}/api/chat"))
                .header("Authorization", &format!("Bearer {token}"))
                .send_json(json!({ "prompt": p }));
            match resp {
                Ok(mut r) => println!("{}", r.body_mut().read_to_string().unwrap_or_default()),
                Err(e) => {
                    eprintln!("Remote chat failed: {e}");
                    std::process::exit(1);
                }
            }
        }
    } else if sets.is_empty() {
        let resp = http
            .get(&format!("{remote}/api/state"))
            .header("Authorization", &format!("Bearer {token}"))
            .call();
        match resp {
            Ok(mut r) => println!("{}", r.body_mut().read_to_string().unwrap_or_default()),
            Err(e) => {
                eprintln!("Remote state fetch failed: {e}");
                std::process::exit(1);
            }
        }
    }
}

fn detect_lan_ips() -> Vec<String> {
    let mut ips: Vec<String> = Vec::new();
    #[cfg(target_os = "windows")]
    if let Ok(out) = Command::new("ipconfig").output() {
        let text = String::from_utf8_lossy(&out.stdout);
        let mut skip_adapter = false;
        for line in text.lines() {
            let trimmed = line.trim();
            if !line.starts_with(' ') && trimmed.ends_with(':') {
                let lower = trimmed.to_ascii_lowercase();
                skip_adapter = ["wsl", "hyper-v", "vethernet", "warp", "tunnel", "virtual", "loopback"]
                    .iter()
                    .any(|kw| lower.contains(kw));
            } else if !skip_adapter && trimmed.contains("IPv4") && let Some((_, raw)) = trimmed.split_once(':') {
                let ip = raw.trim().trim_end_matches("(Preferred)").trim();
                if is_private_lan_ip(ip) && !ips.iter().any(|x| x == ip) {
                    ips.push(ip.to_string());
                }
            }
        }
    }
    #[cfg(not(target_os = "windows"))]
    if let Ok(out) = Command::new("ifconfig").output() {
        let text = String::from_utf8_lossy(&out.stdout);
        for line in text.lines() {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 2 && parts[0] == "inet" {
                let ip = parts[1];
                if is_private_lan_ip(ip) && !ips.iter().any(|x| x == ip) {
                    ips.push(ip.to_string());
                }
            }
        }
    }
    if ips.is_empty()
        && let Ok(sock) = UdpSocket::bind("0.0.0.0:0")
        && sock.connect("8.8.8.8:80").is_ok()
        && let Ok(addr) = sock.local_addr()
        && !addr.ip().is_loopback()
    {
        ips.push(addr.ip().to_string());
    }
    ips
}

fn is_private_lan_ip(ip: &str) -> bool {
    let octets: Vec<u8> = ip.split('.').filter_map(|s| s.parse().ok()).collect();
    if octets.len() != 4 {
        return false;
    }
    matches!(
        (octets[0], octets[1]),
        (192, 168) | (10, _) | (172, 16..=31)
    )
}

fn open_url(url: &str) {
    #[cfg(target_os = "windows")]
    let _ = Command::new("cmd").args(["/c", "start", "", url]).spawn();
    #[cfg(target_os = "macos")]
    let _ = Command::new("open").arg(url).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let _ = Command::new("xdg-open").arg(url).spawn();
}

fn print_help() {
    println!(
        "penguin-agent — Local 9B & Claude LAN coding harness

Usage:
  penguin-agent [options]                   Start LAN Web UI & API server (default 0.0.0.0:3080)
  penguin-agent --cli \"<task>\"              Run one task locally in terminal and exit
  penguin-agent --remote <url> [opts]       Inspect or control a running LAN host from Mac/terminal

Options:
  --bind <ip>          Bind IP (default 0.0.0.0 for LAN access)
  --port <port>        Listen port (default 3080)
  --workspace <dir>    Initial workspace directory
  --token <secret>     Override auth token (or set PENGUIN_TOKEN)
  --open               Open local browser UI on startup
  --remote <url>       Target LAN server (e.g. http://192.168.10.111:3080)
  --set <k=v>          Update config on --remote (repeatable: model=..., mode=auto, num_ctx=131072)
  --cli <prompt>       Send prompt locally or via --remote"
    );
}
