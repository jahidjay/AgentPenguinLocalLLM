# AgentPenguinLocalLLM

Minimal, 1-click local agentic coding setup built on **DeepSeek Harness (`@deepseek-ai/dsh`)** + **Ollama** for **Qwen 3.5 9B** / **Ornith 1.0 9B**, with **Claude** ready as an optional escalation brain.

Focused on **C++**, **Python**, and **Rust** workflows with:
- **Think & Plan before executing scripts** (`AGENTS.md`, `dsh-plan-mode`, and Ollama `reasoning_effort` enabled)
- **Context-window awareness** (`dsh-token-meter`, `dsh-compaction-basic`, and `dsh-compaction-tool-result-pruner` tuned to match Ollama's real `num_ctx`)
- **Two launch surfaces**: Antigravity-style browser IDE (`dsh web`) or terminal (`dsh headless` / `ollama launch claude`)

---

## Hardware & Context Budget

Qwen3.5-9B uses a hybrid architecture (24 Gated DeltaNet recurrent layers + 8 Gated Full Attention layers with 4 KV heads), keeping long-context KV cache relatively small when paired with Flash Attention and 4-bit KV cache quantization (`OLLAMA_FLASH_ATTENTION=1`, `OLLAMA_KV_CACHE_TYPE=q4_0`).

| Machine | Default `AGENT_CTX` | `maxTokens` (Output) | Measured / Expected Memory |
| :--- | :--- | :--- | :--- |
| **Windows (RTX 3070 8 GB + 32 GB RAM)** | `131072` (128K) | `69632` (> 64K) | **~8.1 GB** (`qwen3.5:latest`, ~69% GPU / 31% RAM) |
| **Windows 192K (`<= 200K`)** | `196608` (192K) | `69632` (> 64K) | **~9.2 GB** (`qwen3.5:latest`, ~59% GPU / 41% RAM) |
| **M2 Mac (16 GB Unified RAM)** | `131072` (128K) | `69632` (> 64K) | **~8.1 GB** (~5 GB left for macOS; set `AGENT_CTX=65536` if tight) |

> **Why `headroomTokens: 8192` matters:** DeepSeek Harness's `@deepseek-ai/dsh-compaction-basic` caps auto-compaction at `contextWindow - maxTokens - headroomTokens`, and `headroomTokens` defaults to `65536`. Without lowering `headroomTokens` to `8192`, a `131072` context with `> 64K` output would leave `0` usable input tokens and fail validation. `dsh/base.patch.yml` also clamps `maxTokens` automatically if you lower `AGENT_CTX`.

---

## Prerequisites

1. **Ollama** installed (`ollama` in `PATH`)
2. **Node.js** (already ships `npx`, or install DSH globally once: `npm i -g @deepseek-ai/dsh`)
3. *(Optional)* **Claude Code (`claude`)** or an `ANTHROPIC_API_KEY` if you want Claude as a backup brain

---

## 1-Click Quick Start

### Windows (8 GB GPU / 32 GB RAM)
- **1-click Browser IDE:** Double-click [`start-win.cmd`](start-win.cmd) (or run `.\start-win.cmd shortcut` once to place a launcher on your Desktop).
- **Use Ornith 1.0 9B (RL-tuned for coding/terminal):**
  ```cmd
  start-win.cmd ornith
  ```
- **Push to 192K context (`<= 200K`):**
  ```cmd
  set AGENT_CTX=196608 && start-win.cmd
  ```
- **Pure Terminal Mode (one-shot headless):**
  ```cmd
  start-win.cmd headless "Build and test the Rust crate, fix any errors"
  ```
- **Pure Terminal TUI (Claude Code UI backed by local 9B):**
  ```cmd
  start-win.cmd claude
  ```

### M2 Mac (16 GB Unified RAM)
Make [`start-mac.command`](start-mac.command) executable once (`chmod +x start-mac.command`), then **double-click it in Finder** for 1-click Browser IDE launch, or run from Terminal:
```bash
./start-mac.command                 # Web UI (qwen3.5:latest, 128K ctx, >64K output)
./start-mac.command ornith          # Web UI with Ornith-1.0-9B
AGENT_CTX=65536 ./start-mac.command # Lower memory mode for M2 16GB
./start-mac.command headless "Run pytest and fix failures"
./start-mac.command claude          # Terminal TUI backed by local 9B
```

---

## Using Claude as the "Brain" Alongside Local 9B

Two zero-code ways to combine local Qwen 9B with Claude:
1. **Inside DSH Web UI:** `dsh/base.patch.yml` registers the built-in `anthropic` route alongside `ollama`. Open **Settings -> Models** in the Web UI (or set `ANTHROPIC_API_KEY`) to authenticate, then switch the main chat or subagent model to Claude whenever a tricky C++/Rust/architecture problem needs heavy reasoning.
2. **Via Claude Code CLI (`claude`):** Keep local 9B doing routine edits/builds, and run `claude -p "<problem + file paths>"` from the terminal or DSH shell when you want your Claude Pro/Max subscription to diagnose a stubborn issue.

---

## Files

| File | Purpose |
| :--- | :--- |
| [`start-win.cmd`](start-win.cmd) | 1-click Windows launcher (starts Ollama with `q4_0` KV + flash attention, pulls model if missing, launches DSH or Claude TUI) |
| [`start-mac.command`](start-mac.command) | 1-click macOS Finder/Terminal launcher for M2 16 GB |
| [`dsh/base.patch.yml`](dsh/base.patch.yml) | Ollama (`qwen3.5:latest`, `Ornith-1.0-9B`) + `anthropic` provider catalog, thinking/reasoning effort, dynamic context/output caps |
| [`dsh/web.patch.yml`](dsh/web.patch.yml) | Default `penguin` Web UI preset (think-first persona, plan mode, `headroomTokens: 8192` compaction, tool-result pruning) |
| [`dsh/headless.patch.yml`](dsh/headless.patch.yml) | Compaction + tool-output pruning overlay for `dsh --profile headless` |
| [`AGENTS.md`](AGENTS.md) | Shared C++ / Python / Rust verification & context-awareness instructions loaded automatically by DSH and Claude Code |
