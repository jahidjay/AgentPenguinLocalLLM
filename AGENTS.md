# Workspace Agent Rules (C++ / Python / Rust)

## 1. Reason and Plan Before Executing Scripts
- Explore before editing or running commands: inspect files with targeted searches (`rg`/`grep`) and bounded line ranges instead of dumping whole files.
- Before modifying code or running build/test scripts, state a concise plan: target files, exact change, and verification command.
- Keep edits minimal and idiomatic; reuse standard libraries and existing project structure.

## 2. Context-Window Awareness
- Treat context as a scarce resource (local 9B runs best with compact history).
- Never print giant build logs, dependency trees, or full source files into chat. Pipe noisy compiler/test output through `| tail -n 60` (POSIX) or `| Select-Object -Last 60` (PowerShell).
- When switching tasks or after long debug loops, summarize root cause + touched files cleanly and compact (`/compact`) or delegate isolated diagnostics to a subagent.

## 3. Language Workflows (Build -> Verify -> Fix)
- **Python:** Syntax check with `python -m py_compile <file>` and test with `python -m pytest -q`. Prefer stdlib (`pathlib`, `subprocess`, `json`, `unittest`) over extra dependencies.
- **C++:** Compile with warnings enabled (`g++ -std=c++20 -Wall -Wextra` or `clang++` / `cmake --build`), run the binary/tests, and fix the first root compiler error before chasing cascaded errors.
- **Rust:** Run `cargo check --message-format=short` first for fast diagnostics, then `cargo test` and `cargo clippy`.

## 4. Escalation to Claude ("Brain")
- Local 9B owns normal exploration, script authoring, builds, and test loops.
- Escalate when stuck for 2+ fix cycles on multi-file architecture, subtle UB/borrow-checker/template issues, or design reviews:
  - In DSH Web UI: switch model to an `anthropic` Claude model or delegate via `subagent` if configured.
  - From CLI (when Claude Code is signed in): `claude -p "<concise goal, exact error, and file paths>"`.
