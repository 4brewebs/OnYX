# 06. Complete CLI Reference & Troubleshooting

> 🇪🇸 **[Ver este capítulo en Español](06-referencia-cli-y-troubleshooting.md)**

Quick reference for all CLI subcommands, flags, environment variables, HTTP/SSE resilience, and solutions to common real-world issues on Windows, Linux, and macOS.

---

## 1. Subcommand Overview

```text
openonyx chat [OPTIONS] [--resume <dir> [--tick <n>] [--fork-dir <dir>] [--truncate-future]]
openonyx run <goal> [OPTIONS]
openonyx replay <transcript> [--workspace <dir>] [--snapshot-dir <dir>] [--snapshot-every <n>] [--max-snapshots <n>]
openonyx inspect <snapshot_dir> [--tick <n>] [--list] [--json]
openonyx rewind <snapshot_dir> <tick> [--transcript <file>] [--interactive | --tui] [--fork-dir <dir>] [--truncate-future] [--dry-run] [OPTIONS]
openonyx bench --snapshot-dir <dir> --tick <n> --models <list> [--goal <text>] [--verify <cmd>] [--out <file.json>] [--max-steps <n>] [OPTIONS]
openonyx doctor [--workspace <dir>] [--snapshot-dir <dir>] [--transcript <file>]
openonyx trust [--workspace <dir>] [add | list | check | revoke] [--list | --check | --revoke]
openonyx serve [--workspace <dir>] [--bind <ip:port>] [--api-key <key>] [--cors-origin <origin>] [--behind-tls] [--allowed-hosts <a,b>] [OPTIONS]
openonyx [--help | -h | --version | -V]
```

---

## 2. Complete Flag Table

| Flag | Applies To | Description / Default |
| --- | --- | --- |
| `-w, --workspace <dir>` | All | Target workspace directory (default: current working directory). |
| `--provider <gemini\|openai\|anthropic>` | `chat`, `run`, `rewind` | LLM backend (`gemini` default; `openai` for OpenAI/OpenRouter/Ollama/vLLM/LM Studio; `anthropic` for the native Messages API). |
| `--model <name>` | `chat`, `run`, `rewind` | Model name (defaults: `gemini-4-hydrogen` on Gemini, `gpt-4o` on OpenAI, `claude-sonnet-4-5` on Anthropic). |
| `--effort <low\|medium\|high>` | `chat`, `run`, `rewind` | Reasoning/thinking effort (`thinkingLevel` / `reasoning_effort`; on `anthropic` it maps to a thinking budget of 2048/8192/16384 unless an explicit budget is set). |
| `--thinking-budget <n>` | `chat`, `run`, `rewind` | Anthropic extended-thinking budget in tokens (`0` = off, or `1024..=60000`; or `ANTHROPIC_THINKING_BUDGET`). Raises `max_tokens` above the budget. |
| `--api-base <url>` | `chat`, `run`, `rewind` | Custom API base URL (`http://` allowed only on loopback `localhost`/`127.0.0.1`/`[::1]`; remote hosts require `https://`). |
| `--allow-custom-auth-host` | `chat`, `run`, `rewind` | Explicit opt-in to send API keys to non-official remote HTTPS endpoints (or `ONYX_ALLOW_CUSTOM_AUTH_HOST=1`). |
| `--trust-workspace` | `chat`, `run`, `rewind`, `serve` | Trust the current workspace in memory for this run (without writing `~/.onyx/trusted_workspaces.toml`). |
| `--bind <ip:port>` | `serve` | HTTP/SSE bind address (default: `127.0.0.1:8787`; a non-loopback bind requires an explicit `--api-key` `>= 16` chars and enables exposed/secure mode, as does `--behind-tls`). |
| `--api-key <key>` | `serve` | API key for authentication (`HttpOnly` cookie or `Authorization: Bearer`; randomly generated on loopback if omitted; required, `>= 16` chars, for non-loopback binds). |
| `--cors-origin <url>` | `serve` | Explicit allowed browser origin for external SPAs (e.g. Svelte dev server `http://localhost:5173`; never `*`; exposed mode accepts `https://` only). |
| `--behind-tls` | `serve` | A TLS-terminating reverse proxy sits in front: enables exposed/secure mode (HSTS, `__Host-`/`Secure` cookies, `https` links) without trusting `X-Forwarded-Proto`. |
| `--allowed-hosts <a,b>` | `serve` | Strict allowlist (opt-in) of accepted `Host` header values; a missing or foreign `Host` is rejected with `421 Misdirected Request`. |
| `--sandbox <off\|workspace-write\|strict-container>` | `chat`, `run`, `rewind` | OS sandbox profile for `shell_exec` (default: `workspace-write`). |
| `--sandbox-net <offline\|localhost\|unrestricted>` | `chat`, `run`, `rewind` | Sandbox network policy (default: `offline`). |
| `--auto-approve` | `chat`, `run`, `rewind` | Skip interactive approvals for mutating tools (`deny` rules still apply; sandbox elevates `off` → `workspace-write` + `offline` unless `--sandbox off`). |
| `--ignore-deny` | `chat`, `run`, `rewind` | Explicitly bypass `[permissions].deny` rules from `.onyx/config.toml`. |
| `--max-steps <n>` | `chat`, `run` | Max root agent steps per turn (default: `128` in `chat`, `64` in `run`; subagents capped at `32` by default). |
| `--max-subagents <n>` | `chat`, `run`, `serve`, `rewind --interactive` | Max parallel async subagent Git worktrees (default: `3`, or `ONYX_MAX_SUBAGENTS`). |
| `--shell-timeout <secs>` | `chat`, `run` | Timeout in seconds per `shell_exec` command (default: `30s`). |
| `--llm-timeout-connect <secs>` | `chat`, `run`, `rewind`, `bench`, `serve` | LLM HTTP connect (TCP/TLS) timeout in seconds (default: `30s`; or `ONYX_LLM_TIMEOUT_CONNECT_SECS`). |
| `--llm-timeout-read <secs>` | `chat`, `run`, `rewind`, `bench`, `serve` | LLM HTTP read timeout in seconds between bytes/SSE chunks (default: `120s`; or `ONYX_LLM_TIMEOUT_READ_SECS`). A hung provider fails the turn in bounded time instead of blocking the session forever. |
| `--auto-save` | `chat`, `run` | Auto-create `.onyx/vault/<YYYYMMDD-HHMMSS>_<model>[_<branch\|goal>]/` storing both `snapshots.onyxpack` and `<id>.tr` together (or `ONYX_AUTO_SAVE=1`). |
| `--record <f>`, `--save-transcript <f>` | `chat`, `run` | Save deterministic `ONYXTR2` transcript (short names go inside `--snapshot-dir`, or `.onyx/vault/unassigned/` if no snapshot dir). |
| `--snapshot-dir <dir>` | `chat`, `run`, `replay`, `doctor` | Snapshot directory for `snapshots.onyxpack` (short names resolve under `.onyx/vault/<dir>/`). |
| `--snapshot-every <n>` | `chat`, `run`, `replay` | Also capture periodic `periodic` milestones every `<n>` ticks. |
| `--max-snapshots <n>` | `chat`, `run`, `replay` | Opt-in sliding-window retention (keeps `init` + latest `n - 1` milestones). |
| `-r, --resume <dir>` | `chat` | Resume from latest milestone (or `--tick <n>`) in `<dir>` (resolves short names in `.onyx/vault/`). |
| `--tick <n>` | `inspect`, `chat --resume` | Specific tick to inspect or resume/fork from. |
| `--fork-dir <dir>` | `chat --resume --tick`, `rewind --interactive` | Destination directory for forked snapshots (short names resolve under `.onyx/vault/`). |
| `--truncate-future` | `chat --resume --tick`, `rewind` | Opt-in destructive truncation: overwrite source pack in place instead of creating a sibling fork. |
| `--interactive`, `--tui` | `rewind` | Open interactive TUI at the restored tick instead of running headless. |
| `-l, --list` | `inspect` | List all stored milestones with tags, sizes, FNV-1a checksums, and Git checkpoints. |
| `--json` | `inspect` | Emit machine-readable JSON output. |
| `--transcript <file>` | `rewind`, `doctor` | Replay transcript from rewind tick (`rewind`) or validate transcript (`doctor`). |
| `--dry-run` | `rewind` | Preview orphaned effects and Git rollback plan without modifying disk. |
| `--models <list>` | `bench` | Comma-separated candidate list: `<provider>:<model>` (live LLM, e.g. `gemini:gemini-2.5-pro,openai:gpt-4o`) and/or `replay:<label>=<file.tr>` / `replay:<file.tr>` (offline `ONYXTR2` transcript, deterministic). |
| `--verify <cmd>` | `bench` | Verification command executed **sandboxed inside the candidate's worktree** after it settles (same pipeline as `shell_exec`); its `exit 0` decides `passed_verify`. Without `--verify`, `passed_verify` = agent finished `Done` and the session settled. |
| `--out <file.json>` | `bench` | Write the full pretty-printed JSON report (metadata + results). |
| `--goal <text>` | `bench` | Root agent goal when no agent is active at `--tick` (required in that case). |
| `-h, --help` / `-V, --version` | Global | Print help or version and exit `0`. |

### `openonyx bench` — full reference

Multi-model benchmark from an **identical snapshot**: every candidate runs in a **throwaway worktree** (`bench-<pid>-<idx>-<label>`) rewound to `--tick` (`reconcile_rewind_effects_report`), with a fresh session host (`request_restore` + 1 step); afterwards the root and `HEAD` remain **100 % intact** and the worktree is removed (RAII guard; also on error or panic).

```sh
# Live + offline replay comparison, with verification and a JSON report
openonyx bench \
  --snapshot-dir .onyx/vault/20261125-100000_gpt-4o \
  --tick 12 \
  --models "gemini:gemini-2.5-pro,openai:gpt-4o,replay:baseline=run_baseline.tr" \
  --goal "fix the failing test in src/app.rs" \
  --verify "cargo test --workspace" \
  --out bench_report.json \
  --auto-approve \
  --sandbox workspace-write \
  --sandbox-net offline
```

- **Candidates (`--models`):** `<provider>:<model>` with `provider ∈ gemini|openai|anthropic` (credentials via the usual `GEMINI_API_KEY`/`OPENAI_API_KEY`/`ANTHROPIC_API_KEY` variables or `--api-base`); `replay:<label>=<path.tr>` sets the label explicitly; `replay:<path.tr>` uses the file stem as the label. Parsing splits on `:` first, then `=`, so Windows drive-letter paths (`replay:C:\runs\a.tr`) survive intact.
- **Per candidate (sequential):** rewind the worktree to the tick → fresh restored host → if no agent is active at the tick, spawn a root agent from `--goal` → run until settled (internal 256-tick cap) with real telemetry (tokens/cost) for live candidates (replay: tokens/cost `0`).
- **`--auto-approve`:** for **unattended** runs on disposable worktrees: each candidate runs only inside its own worktree and the root is never touched; `[permissions].deny` still applies.
- **Output:** a 9-column Markdown table on `stdout` — `| model | passed_verify | ticks | tool_calls | input_tokens | cached_tokens | output_tokens | estimated_cost_usd | wall_time_ms |` — plus, with `--out`, the JSON report (`metadata` with snapshot/tick/models/verify + per-candidate `results` with every column).
- **Reused flags:** `--workspace <dir>`, `--sandbox <off|workspace-write|strict-container>`, `--sandbox-net <offline|localhost|unrestricted>`, `--shell-timeout <secs>`, `--llm-timeout-connect <secs>`, `--llm-timeout-read <secs>`, `--trust-workspace`, `--api-base <url>`, `--allow-custom-auth-host`, `--effort <low|medium|high>`, `--thinking-budget <n>` (a live candidate determines its own `provider:model`).
- **Actionable validation:** missing `--snapshot-dir`/`--tick`/`--models` produces an error naming the missing flag; unknown provider → `unsupported provider ... (expected `gemini`, `openai` or `anthropic`)`; a missing snapshot store → floor warning, not an error.

---

## 3. Environment Variables

| Variable | Provider | Description |
| --- | --- | --- |
| `ONYX_PROVIDER` | All | Default provider (`gemini` \| `openai` \| `anthropic`). |
| `GEMINI_API_KEY` | Gemini | Google AI Studio API key (fallback: `ONYX_API_KEY`). |
| `OPENAI_API_KEY` | OpenAI | API key for `--provider openai` (fallback: `ONYX_API_KEY`; optional with local `--api-base`). |
| `ANTHROPIC_API_KEY` | Anthropic | API key for `--provider anthropic` (fallback: `ONYX_API_KEY`). |
| `ONYX_API_KEY` | All | Shared fallback key if provider-specific key is unset. |
| `ONYX_MODEL` | All | Default model override (fallbacks: `ONYX_GEMINI_MODEL`, `OPENAI_MODEL`, `ANTHROPIC_MODEL`). |
| `ONYX_EFFORT` | All | Default reasoning effort (`low` \| `medium` \| `high`; fallbacks: `GEMINI_THINKING_LEVEL`, `OPENAI_REASONING_EFFORT`; on Anthropic, `ANTHROPIC_THINKING_BUDGET` governs thinking). |
| `ONYX_AUTO_SAVE` | Both | Enable `--auto-save` by default (`1` \| `true` \| `yes` \| `on`) storing sessions in `.onyx/vault/`. |
| `ONYX_MAX_SUBAGENTS` | Both | Max parallel async subagent worktrees (used when `--max-subagents <n>` is omitted; default: `3`). |
| `ONYX_LLM_TIMEOUT_CONNECT_SECS` | All | LLM HTTP connect timeout in seconds (default: `30`; fallback for `--llm-timeout-connect`). |
| `ONYX_LLM_TIMEOUT_READ_SECS` | All | LLM HTTP read timeout in seconds between bytes/SSE chunks (default: `120`; fallback for `--llm-timeout-read`). |
| `ONYX_WEB_BIND` | `serve` | Default bind address for `openonyx serve` (default: `127.0.0.1:8787`; fallback for `--bind`). |
| `ONYX_WEB_API_KEY` | `serve` | Explicit API key for `openonyx serve` (required for non-loopback binds; fallback for `--api-key`). |
| `ONYX_WEB_CORS_ORIGIN` | `serve` | Allowed browser origin for external SPAs (fallback for `--cors-origin`). |
| `GEMINI_API_BASE` | Gemini | Custom Gemini API base URL. |
| `OPENAI_API_BASE` / `OPENAI_BASE_URL` | OpenAI | Custom OpenAI-compatible API endpoint (default: `https://api.openai.com/v1`). |
| `ANTHROPIC_API_BASE` | Anthropic | Custom Anthropic API base URL (default: `https://api.anthropic.com`). |
| `ANTHROPIC_THINKING_BUDGET` | Anthropic | Extended-thinking budget in tokens (`0` = off, or `1024..=60000`); takes precedence over the `--effort`-derived budget. |

> **LLM, driver & MCP config in `config.toml`:** beyond flags and environment variables, `.onyx/config.toml` accepts `[[llm_profiles]]` (chapter 03, section 2.6), `[[external_drivers]]` (chapter 03, section 2.7), and `[mcp.servers.<name>]` (chapter 03, section 2.8) blocks. A profile's key resolves in the order `api_key_env` → the provider's standard variable → `ONYX_API_KEY`.

---

## 4. HTTP/SSE Resilience on OpenAI, vLLM & Corporate Proxies

Local inference servers and proxies (vLLM, LiteLLM, nginx, Cloudflare) occasionally close chunked SSE streams abruptly right after emitting `finish_reason` or return transient errors (`408`, `429`, `500`, `502`, `503`, `504`, or brief transport drops).

`open-onyx/src/openai.rs` builds in three automatic safeguards:
1. **Completed-SSE EOF tolerance**: if a chunk decode error happens after `finish_reason` / `[DONE]` with complete assistant content or `tool_calls`, the response is accepted cleanly instead of failing the turn.
2. **Automatic transient retries with backoff**: retries transient transport/408/429/5xx/stream errors up to **2 times** (`400ms`, `1200ms`) before surfacing an error to the agent.
3. **Explicit HTTP timeouts (`FIX-02`)**: every request to the three providers sets a configurable `timeout_connect` (default `30s`) and `timeout_read` (default `120s`, per SSE chunk) via `--llm-timeout-connect` / `--llm-timeout-read` (or `ONYX_LLM_TIMEOUT_CONNECT_SECS` / `ONYX_LLM_TIMEOUT_READ_SECS`). A provider that accepts the connection and then goes silent surfaces a classified transport error in bounded time instead of blocking the session (e.g. `serve`) forever. The read timeout is **not** retried by the transient policy (point 2): a silent provider fails once, within the read budget, because blindly retrying after the full prompt was sent could trigger duplicate inference; connect failures, resets and `408`/`429`/`5xx` remain retried with backoff.

The native `open-onyx/src/anthropic.rs` adapter applies the same retry policy (`408`, `429`, `500`, `502`, `503`, `504`, and Anthropic's own `overloaded_error` `529`), honoring the `retry-after` header when the server sends it.

---

## 5. Common Troubleshooting

### 1. `GEMINI_API_KEY environment variable is not set`
- For **Gemini**: set `GEMINI_API_KEY` (PowerShell: `$env:GEMINI_API_KEY="..."`; bash/zsh: `export GEMINI_API_KEY="..."`).
- For **Ollama / vLLM / LM Studio**: pass `--provider openai --api-base http://localhost:11434/v1 --model <model>` (no API key needed).
- For `inspect`, `replay`, `rewind --dry-run`, and `doctor`: no API key is ever required.

### 2. Agent attempts to read a `.tr`, `.onyxpack`, or `tick-*.bin` file with `fs_read`
`fs_read` detects binary OnYX headers (`ONYXTR1`, `ONYXTR2`, `ONYXPK1`, `ONYXAG`) and returns an actionable error pointing the agent to `openonyx inspect` / `openonyx replay`.

### 3. `shell_exec` fails with an offline/network error during `cargo build` or `curl`
The default sandbox policy is `--sandbox-net offline` (also injecting `CARGO_NET_OFFLINE=true`):
- To reach local Sprout services (`127.0.0.1`): pass `--sandbox-net localhost`.
- To fetch crates or internet resources: pass `--sandbox-net unrestricted`.

### 4. Ubuntu 24.04+: `bwrap` cannot create unprivileged user namespaces (AppArmor)
Ubuntu 24.04 restricts unprivileged user namespaces via AppArmor. `openonyx` probes `bwrap_is_usable()` at runtime and automatically falls back cleanly so `shell_exec` keeps working. Verify your host driver with `openonyx doctor`.

### 5. `Ctrl+Tab` does not switch agents in my terminal
Some terminal emulators (Windows Terminal, tmux, SSH) swallow `Ctrl+Tab`. Use the **`F2`** key (which now opens the **Agents** view, chapter 02) or **`/agent [id|name]`**, which work across all terminals.

### 6. `/rewind` refuses to run while an agent is busy
Live `/rewind <tick>` requires no active agents (`thinking`, `acting`, `waiting_children`) and no in-flight LLM calls. Wait for `Settled`, or quit with `Esc` and run `openonyx chat --resume <session> --tick <n>`.

### 7. Windows PowerShell quoting or bash-isms in `shell_exec`
`openonyx` tells the model the host OS (Windows PowerShell vs Unix/POSIX), and you can reinforce in `<workspace>/ONYX.md` to prefer native tools (`code_search`, `glob_files`, `fs_read`, `git_diff`) over shell commands.

### 8. Unexpected crash during a long session
1. Run `openonyx doctor --snapshot-dir <session>` to check for any truncated tail bytes in `snapshots.onyxpack`.
2. Inspect intact milestones with `openonyx inspect <session> --list`.
3. Resume with `openonyx chat --resume <session>` — truncated tail bytes are safely ignored on read and healed automatically on the next append.

### 9. `openonyx doctor` warns or errors on an external driver (`[[external_drivers]]`)
`doctor` audits the `[[external_drivers]]` declarations (chapter 03, section 2.7):
- Built-in `opencode` preset: `[WARN]` when the `opencode` binary is not on `PATH` (the preset is skipped).
- Declared driver whose `bin` is missing: `[ERR]` — `/spawn --driver <id> will fail at runtime`.
- `bin` found but the `--version` probe (3 s deadline, sanitized environment) times out: `[ERR]`; if it errors out instead (e.g., wrong flags): `[WARN]`.
- `prompt_transport = "arg"` on Windows: `[WARN]` when the binary resolves to a `.cmd`/`.bat` wrapper (rejected at spawn time).
- `network_allowlist` without `--sandbox strict-container`: `[WARN]` — the allowlist is advisory outside `strict-container`; a declared driver without `network_allowlist` is `LocalhostOnly` unless the sandbox relaxes it.

### 10. An MCP server does not connect, or `[mcp.servers.*]` is ignored
`openonyx doctor` audits the servers declared in `[mcp.servers.<name>]` (chapter 03, section 2.8) with a 5 s-capped handshake (a dead server can never hang `doctor`):
- `Connected` or `Disabled`: informational `[CHECK]`.
- `Failed(...)` (missing binary, handshake timeout, server error): actionable `[ERR]` with an already-scrubbed message; the session keeps running without that server (failure isolation).
- **Untrusted** workspace with project `[mcp.servers.*]`: `[WARN]` `MCP config in ... ignored: workspace is not trusted (use --trust-workspace or openonyx trust)` — approve with `openonyx trust` or use `--trust-workspace`.
- `mcp__*` tools always ask for approval in `BUILD` and are `Deny` in `PLAN`/`EXPLORE`; to auto-approve them, declare `allow = ["mcp__<server>__*"]` in `.onyx/config.toml` (trusted workspace).

### 11. The Web (or the TUI) rejects my prompt: `409 Conflict` / "Paused in TIME FROZEN"
That is the `TimeFreeze` guard (chapter 04, section 9): the session is **frozen** (an active time breakpoint) and an out-of-order plain prompt would break the bit-for-bit replay. It is not a bug — these are the escape routes:
- `/go` (or `/unfreeze`): branches if the cursor looks at the past, then resumes at present.
- `/inject <text>` / `/graph …` / `/read …` / `/step [n]` / `/step-llm` / `! <cmd>`: keep working under freeze (sealed 0-token primitives).
- In the Web, the amber bar (press `#tick-pill`) has a `Resume` button; in the TUI, `Esc` returns to present without quitting and `Ctrl+T` toggles the freeze.
- On `POST /api/v1/command` the 409 carries the full actionable message; a `400` on `POST /api/v1/time/scrub` means a future/purged tick or an unknown action (the body explains it).

### 12. `openonyx serve` returns a `4xx` when deploying to a VPS
The `serve` hardening answers with actionable errors instead of accepting suspicious traffic:
- `429 Too Many Requests` + `Retry-After`: more than 5 attempts per IP in 60 s (or 20 globally) before the key is validated.
- `421 Misdirected Request`: with `--allowed-hosts`, the `Host` header is missing or not in the list.
- `400 Bad Request`: duplicated headers (e.g. double `Content-Length`) or another violation of the strict HTTP parser.
- `431 Request Header Fields Too Large` / `413 Request Entity Too Large`: headers or body above the limit.
- `403 Forbidden`: a POST with a cookie but no `Sec-Fetch-Site` whose `Origin` is neither the server's own host nor the matched CORS origin.
- `415 Unsupported Media Type`: a POST with a cookie whose `Content-Type` is not `application/json`.

---

[← 05. Tools, Scent, Sprout & Skills](05-tools-scent-sprout-and-skills.md) · [Back to English Index](index-en.md)
