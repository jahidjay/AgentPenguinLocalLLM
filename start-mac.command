#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")" && pwd)"
cd "$ROOT"

export AGENT_MODEL="${AGENT_MODEL:-qwen3.5:latest}"
export AGENT_CTX="${AGENT_CTX:-131072}"
export AGENT_MAX_TOKENS="${AGENT_MAX_TOKENS:-69632}"
export OLLAMA_FLASH_ATTENTION="${OLLAMA_FLASH_ATTENTION:-1}"
export OLLAMA_KV_CACHE_TYPE="${OLLAMA_KV_CACHE_TYPE:-q4_0}"
export OLLAMA_CONTEXT_LENGTH="${OLLAMA_CONTEXT_LENGTH:-$AGENT_CTX}"
export OLLAMA_API_KEY="${OLLAMA_API_KEY:-ollama}"
export DSH_TELEMETRY_MODE="${DSH_TELEMETRY_MODE:-DISABLED}"
export DSH_PERMISSION_MODE="${DSH_PERMISSION_MODE:-workspace-write}"

BIN="$ROOT/target/release/penguin-agent"

ensure_rust() {
  if [[ ! -x "$BIN" ]]; then
    if ! command -v cargo >/dev/null 2>&1; then
      echo "[ERROR] Rust cargo not found. Install from https://rustup.rs" >&2
      exit 1
    fi
    echo "Building penguin-agent (first run)..."
    cargo build --release
  fi
}

if [[ "${1:-}" == "ornith" ]]; then
  export AGENT_MODEL="hf.co/deepreinforce-ai/Ornith-1.0-9B-GGUF:latest"
  shift
fi

if [[ "${1:-}" == "remote" ]]; then
  shift
  ensure_rust
  exec "$BIN" --remote "$@"
fi

if [[ "${1:-}" == "rebuild" ]]; then
  shift
  cargo build --release
fi

if ! command -v ollama >/dev/null 2>&1; then
  echo "[ERROR] Ollama is not installed. Install from https://ollama.com" >&2
  exit 1
fi

if ! curl -sf http://127.0.0.1:11434/api/tags >/dev/null 2>&1; then
  echo "Starting Ollama server (ctx=$OLLAMA_CONTEXT_LENGTH, kv=$OLLAMA_KV_CACHE_TYPE, flash=$OLLAMA_FLASH_ATTENTION)..."
  nohup ollama serve >/tmp/ollama-agent.log 2>&1 &
  for _ in $(seq 1 20); do
    sleep 1
    if curl -sf http://127.0.0.1:11434/api/tags >/dev/null 2>&1; then
      break
    fi
  done
  if ! curl -sf http://127.0.0.1:11434/api/tags >/dev/null 2>&1; then
    echo "[ERROR] Ollama failed to start. Check /tmp/ollama-agent.log" >&2
    exit 1
  fi
fi

if ! ollama list | grep -qiF "$AGENT_MODEL"; then
  echo "Pulling $AGENT_MODEL..."
  ollama pull "$AGENT_MODEL"
fi

case "${1:-}" in
  claude)
    shift
    export CLAUDE_CODE_MAX_CONTEXT_TOKENS="$AGENT_CTX"
    if [[ $# -gt 0 ]]; then
      exec ollama launch claude --model "$AGENT_MODEL" -- "$@"
    else
      exec ollama launch claude --model "$AGENT_MODEL"
    fi
    ;;
  dsh)
    shift
    if command -v dsh >/dev/null 2>&1; then
      exec dsh --profile web --patch "$ROOT/dsh/base.patch.yml" --patch "$ROOT/dsh/web.patch.yml" "$@"
    else
      exec npx -y @deepseek-ai/dsh --profile web --patch "$ROOT/dsh/base.patch.yml" --patch "$ROOT/dsh/web.patch.yml" "$@"
    fi
    ;;
  cli|headless)
    shift
    ensure_rust
    exec "$BIN" --cli "$@"
    ;;
  *)
    ensure_rust
    if [[ $# -gt 0 ]]; then
      exec "$BIN" "$@"
    else
      exec "$BIN" --open
    fi
    ;;
esac
