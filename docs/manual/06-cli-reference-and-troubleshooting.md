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
openonyx doctor [--workspace <dir>] [--snapshot-dir <dir>] [--transcript <file>]
openonyx trust [--workspace <dir>] [add | list | check | revoke] [--list | --check | --revoke]
openonyx serve [--workspace <dir>] [--bind <ip:port>] [--api-key <key>] [--cors-origin <origin>] [OPTIONS]
openonyx [--help | -h | --version | -V]
```

---

## 2. Complete Flag Table

| Flag | Applies To | Description / Default |
| --- | --- | --- |
| `-w, --workspace <dir>` | All | Target workspace directory (default: current working directory). |
| `--provider <gemini\|openai>` | `chat`, `run`, `rewind` | LLM backend (`gemini` default; `openai` for OpenAI/OpenRouter/Ollama/vLLM/LM Studio). |
| `--model <name>` | `chat`, `run`, `rewind` | Model name (defaults: `gemini-4-hydrogen` on Gemini, `gpt-4o` on OpenAI). |
| `--effort <low\|medium\|high>` | `chat`, `run`, `rewind` | Reasoning/thinking effort (`thinkingLevel` / `reasoning_effort`). |
| `--api-base <url>` | `chat`, `run`, `rewind` | Custom API base URL (`http://` allowed only on loopback `localhost`/`127.0.0.1`/`[::1]`; remote hosts require `https://`). |
| `--allow-custom-auth-host` | `chat`, `run`, `rewind` | Explicit opt-in to send API keys to non-official remote HTTPS endpoints (or `ONYX_ALLOW_CUSTOM_AUTH_HOST=1`). |
| `--trust-workspace` | `chat`, `run`, `rewind`, `serve` | Trust the current workspace in memory for this run (without writing `~/.onyx/trusted_workspaces.toml`). |
| `--bind <ip:port>` | `serve` | HTTP/SSE bind address (default: `127.0.0.1:8787`; non-loopback requires explicit `--api-key` `>= 16` chars). |
| `--api-key <key>` | `serve` | API key for authentication (`HttpOnly` cookie or `Authorization: Bearer`; randomly generated on loopback if omitted). |
| `--cors-origin <url>` | `serve` | Explicit allowed browser origin for external SPAs (e.g. Svelte dev server `http://localhost:5173`; never `*`). |
| `--sandbox <off\|workspace-write\|strict-container>` | `chat`, `run`, `rewind` | OS sandbox profile for `shell_exec` (default: `workspace-write`). |
| `--sandbox-net <offline\|localhost\|unrestricted>` | `chat`, `run`, `rewind` | Sandbox network policy (default: `offline`). |
| `--auto-approve` | `chat`, `run`, `rewind` | Skip interactive approvals for mutating tools (`deny` rules still apply; sandbox elevates `off` → `workspace-write` + `offline` unless `--sandbox off`). |
| `--ignore-deny` | `chat`, `run`, `rewind` | Explicitly bypass `[permissions].deny` rules from `.onyx/config.toml`. |
| `--max-steps <n>` | `chat`, `run` | Max root agent steps per turn (default: `128` in `chat`, `64` in `run`; subagents capped at `32` by default). |
| `--shell-timeout <secs>` | `chat`, `run` | Timeout in seconds per `shell_exec` command (default: `30s`). |
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
| `-h, --help` / `-V, --version` | Global | Print help or version and exit `0`. |

---

## 3. Environment Variables

| Variable | Provider | Description |
| --- | --- | --- |
| `ONYX_PROVIDER` | All | Default provider (`gemini` \| `openai`). |
| `GEMINI_API_KEY` | Gemini | Google AI Studio API key (fallback: `ONYX_API_KEY`). |
| `OPENAI_API_KEY` | OpenAI | API key for `--provider openai` (fallback: `ONYX_API_KEY`; optional with local `--api-base`). |
| `ONYX_API_KEY` | Both | Shared fallback key if provider-specific key is unset. |
| `ONYX_MODEL` | Both | Default model override (fallbacks: `ONYX_GEMINI_MODEL`, `OPENAI_MODEL`). |
| `ONYX_EFFORT` | Both | Default reasoning effort (`low` \| `medium` \| `high`; fallbacks: `GEMINI_THINKING_LEVEL`, `OPENAI_REASONING_EFFORT`). |
| `ONYX_AUTO_SAVE` | Both | Enable `--auto-save` by default (`1` \| `true` \| `yes` \| `on`) storing sessions in `.onyx/vault/`. |
| `GEMINI_API_BASE` | Gemini | Custom Gemini API base URL. |
| `OPENAI_API_BASE` / `OPENAI_BASE_URL` | OpenAI | Custom OpenAI-compatible API endpoint (default: `https://api.openai.com/v1`). |

---

## 4. HTTP/SSE Resilience on OpenAI, vLLM & Corporate Proxies

Local inference servers and proxies (vLLM, LiteLLM, nginx, Cloudflare) occasionally close chunked SSE streams abruptly right after emitting `finish_reason` or return transient errors (`408`, `429`, `500`, `502`, `503`, `504`, or brief transport drops).

`open-onyx/src/openai.rs` builds in two automatic safeguards:
1. **Completed-SSE EOF tolerance**: if a chunk decode error happens after `finish_reason` / `[DONE]` with complete assistant content or `tool_calls`, the response is accepted cleanly instead of failing the turn.
2. **Automatic transient retries with backoff**: retries transient transport/408/429/5xx/stream errors up to **2 times** (`400ms`, `1200ms`) before surfacing an error to the agent.

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
Some terminal emulators (Windows Terminal, tmux, SSH) swallow `Ctrl+Tab`. Use **`F2`** (interactive **Agent Selector** popup) or **`/agent [id|name]`**, which work across all terminals.

### 6. `/rewind` refuses to run while an agent is busy
Live `/rewind <tick>` requires no active agents (`thinking`, `acting`, `waiting_children`) and no in-flight LLM calls. Wait for `Settled`, or quit with `Esc` and run `openonyx chat --resume <session> --tick <n>`.

### 7. Windows PowerShell quoting or bash-isms in `shell_exec`
`openonyx` tells the model the host OS (Windows PowerShell vs Unix/POSIX), and you can reinforce in `<workspace>/ONYX.md` to prefer native tools (`code_search`, `glob_files`, `fs_read`, `git_diff`) over shell commands.

### 8. Unexpected crash during a long session
1. Run `openonyx doctor --snapshot-dir <session>` to check for any truncated tail bytes in `snapshots.onyxpack`.
2. Inspect intact milestones with `openonyx inspect <session> --list`.
3. Resume with `openonyx chat --resume <session>` — truncated tail bytes are safely ignored on read and healed automatically on the next append.

---

[← 05. Tools, Scent, Sprout & Skills](05-tools-scent-sprout-and-skills.md) · [Back to English Index](index-en.md)
