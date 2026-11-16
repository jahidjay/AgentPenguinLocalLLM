# AgentPenguinLocalLLM

**Single-binary Rust agentic coding server + LAN Web UI + CLI** for **Qwen 3.5 9B** / **Ornith 1.0 9B** (via Ollama) with optional **Claude Brain** escalation.

Run the local GPU LLM on your Windows PC (RTX 3070 8 GB + 32 GB RAM) or M2 Mac (16 GB RAM), then **open and control it from your Mac over your local Wi-Fi/Ethernet network** — switching models, thinking depth, context window (`<= 200K`), output tokens (`> 64K`), workspace folder, `AGENTS.md` rules, and `plan-first` vs `auto` execution live with zero restarts.

---

## Why Rust + LAN Control?
- **No Node.js/npm runtime required** for the main agent (`penguin-agent.exe` / `penguin-agent`).
- **True LAN Web UI (`0.0.0.0:3080`)** with token auth (`~/.penguin-agent-token` so your Mac bookmark stays stable across restarts) and live SSE streaming.
- **Change agentic work live from Mac:**
  - Switch between `qwen3.5:latest`, `hf.co/deepreinforce-ai/Ornith-1.0-9B-GGUF:latest`, any model in Ollama `/api/tags`, `anthropic` API, or local `claude` CLI (`claude-cli`).
  - Adjust `num_ctx` (`32K` / `64K` / `128K` / `192K`) and `max_tokens` (`69632` `> 64K` default) **per request** via Ollama `/api/chat`.
  - Toggle **Mode** (`plan-first` with browser Approve/Reject vs `auto`) and **Think Effort** (`off`/`low`/`medium`/`high`).
  - Switch host **Workspace**, live-edit/save **`AGENTS.md`**, run 1-click **Python / C++ / Rust** checks, **Compact** context, or click **Ask Claude Brain** when stuck.
- **Context-Window Safe:** Real `prompt_eval_count` meter, bounded `read_file` (<= 200 lines), bounded `run_command` output (head + tail truncation), and auto-compaction at 78% of `num_ctx`.

---

## Hardware & Context Budget (Measured on RTX 3070 8 GB + 31.2 GB RAM)

With `OLLAMA_FLASH_ATTENTION=1` and `OLLAMA_KV_CACHE_TYPE=q4_0`:

| Machine | Default `AGENT_CTX` | `max_tokens` | Memory Footprint |
| :--- | :--- | :--- | :--- |
| **Windows (RTX 3070 8 GB + 32 GB RAM)** | `131072` (128K) | `69632` (> 64K) | **~8.1 GB** (`qwen3.5:latest`, ~69% GPU / 31% RAM) |
| **Windows 192K (`<= 200K`)** | `196608` (192K) | `69632` (> 64K) | **~9.2 GB** (`qwen3.5:latest`, ~59% GPU / 41% RAM) |
| **M2 Mac (16 GB Unified RAM)** | `131072` (128K) | `69632` (> 64K) | **~8.1 GB** (~5 GB left for macOS; pick `65536` in UI if tight) |

---

## 1-Click Quick Start

### 1. Start on Windows (Host with 8 GB GPU)
Double-click [`start-win.cmd`](start-win.cmd) (or run `.\start-win.cmd shortcut` once to add a Desktop shortcut).
On first run it builds `target\release\penguin-agent.exe`, starts Ollama with `q4_0` KV cache, and prints:
```text
PenguinAgent LAN Server
  Local URL: http://127.0.0.1:3080/?token=...
  Mac / LAN: http://192.168.x.x:3080/?token=...
```
> **Windows Firewall:** If your Mac cannot open `http://192.168.x.x:3080`, allow `penguin-agent.exe` on **Private networks** when Windows prompts (or allow TCP port `3080` on Private profile).

### 2. Connect from Your Mac on the Same Network
- **From Mac Browser (Safari / Chrome):** Open the printed `Mac / LAN` URL (`http://<WIN_LAN_IP>:3080/?token=...`).
- **From Mac Terminal:**
  ```bash
  # Check status
  ./start-mac.command remote http://192.168.x.x:3080 --token <TOKEN>

  # Change model, mode, or context live on the Windows host
  ./start-mac.command remote http://192.168.x.x:3080 --token <TOKEN> \
    --set model=hf.co/deepreinforce-ai/Ornith-1.0-9B-GGUF:latest --set mode=auto --set num_ctx=196608

  # Send a coding task from Mac Terminal to Windows GPU
  ./start-mac.command remote http://192.168.x.x:3080 --token <TOKEN> --cli "Run cargo test and fix any error"
  ```
- **Or run standalone locally on M2 Mac:** `chmod +x start-mac.command` once, then double-click `start-mac.command` in Finder.

### Other Modes
- **Switch default model to Ornith 1.0 9B:** `.\start-win.cmd ornith` (or change it live in the UI)
- **Single-shot CLI in Rust:** `.\start-win.cmd cli "Check src/main.rs and run cargo test"`
- **Legacy DeepSeek Harness (`dsh`) / Claude Code local TUI:** `.\start-win.cmd dsh` or `.\start-win.cmd claude`

---

## Files
| File | Purpose |
| :--- | :--- |
| [`Cargo.toml`](Cargo.toml) & [`src/`](src/) | Single-binary Rust server, Ollama `/api/chat` streamer, Anthropic/Claude Brain bridge, workspace tools, and embedded Web UI |
| [`start-win.cmd`](start-win.cmd) | 1-click Windows launcher (auto-builds Rust binary, starts Ollama, prints LAN URL) |
| [`start-mac.command`](start-mac.command) | 1-click macOS Finder/Terminal launcher & `--remote` LAN client |
| [`AGENTS.md`](AGENTS.md) | Think-first, context-bounded C++ / Python / Rust rules (editable live from the Mac UI) |
| [`dsh/`](dsh/) | Optional `@deepseek-ai/dsh` YAML overlays (`start-win.cmd dsh`) |
