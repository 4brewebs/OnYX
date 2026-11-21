# 03. Modes, Permissions & OS Sandbox

> 🇪🇸 **[Ver este capítulo en Español](03-modos-permisos-y-sandbox.md)**

`openonyx` enforces **defense in depth** across every agent action: workspace modes, declarative `.onyx/config.toml` rules, read-only Control Plane protection, strict environment sanitization (`env_clear()`), OS-level sandboxing, and RAII process-tree supervision.

---

## 1. Workspace Modes: `PLAN`, `BUILD` & `EXPLORE`

The active mode (`ws.mode`) lives inside the EAV graph and is recorded in `ONYXTR2` transcripts so `rewind` and `replay` reproduce it faithfully.

| Mode | Read / Search / AST | Mutations (`fs_write`, `fs_edit`, `shell_exec`, `service_start/stop/exec`) | Best For |
| --- | --- | --- | --- |
| **`BUILD`** *(default)* | Allowed | Allowed with approval (or per `.onyx/config.toml` rules). `fs_edit` requires prior `fs_read`. | Implementation, refactoring, running tests. |
| **`PLAN`** | Allowed | **Unconditionally blocked** (even with `allow` rules or `--auto-approve`). | Architecture analysis, design planning, PR reviews. |
| **`EXPLORE`** | Allowed | **Unconditionally blocked**. | Safe read-only exploration of unfamiliar repositories. |

### Switching Modes in `chat`
- Press **`Tab`** to toggle `[BUILD]` ↔ `[PLAN]` in place.
- Or run `/mode plan`, `/mode build`, `/mode explore` (or `/mode` with no argument to toggle `PLAN` ↔ `BUILD`).

---

## 2. Declarative Permissions (`.onyx/config.toml`)

Granular rules are loaded and merged automatically from two locations:

1. **Global user config**: `~/.onyx/config.toml`
2. **Project config**: `<workspace>/.onyx/config.toml`

> **Security invariant:** project config **can never remove** a global `deny` rule defined in `~/.onyx/config.toml`.

### Precedence: `deny > ask > allow`
- **`deny`**: blocks the tool unconditionally — even when `--auto-approve` is active. Only bypassed if you explicitly pass `--ignore-deny`.
- **`ask`**: requires interactive confirmation (`y`/`n`), prevailing over `allow` at equal or higher specificity.
- **`allow`**: auto-approves matching calls in `BUILD` mode without interrupting you (`PLAN`/`EXPLORE` still block all mutations). A more specific `allow` (e.g. `shell_exec(cargo test*)`) overrides a generic wildcard `ask` (`shell_exec(*)`).

### Complete `.onyx/config.toml` Example

```toml
[permissions]
# Hard blocks (enforced even with --auto-approve)
deny = [
  "shell_exec(rm -rf*)",
  "shell_exec(git push*)",
  "shell_exec(curl*)",
  "fs_write(.env*)",
]

# Auto-approve safe verification commands in BUILD mode
allow = [
  "shell_exec(cargo check*)",
  "shell_exec(cargo test*)",
  "shell_exec(cargo clippy*)",
  "shell_exec(cargo fmt --all -- --check)",
]

# Prompt for all other shell executions and file writes
ask = [
  "shell_exec(*)",
  "fs_write(*)",
  "fs_edit(*)",
]

[sandbox]
# "workspace-write" (default) | "strict-container" | "off"
mode = "workspace-write"
# "offline" (default) | "localhost" | "unrestricted"
network = "offline"
# Extra env vars allowed after env_clear() (never pass API keys)
env_passthrough = ["RUST_LOG", "RUST_BACKTRACE"]
# Additional read-only paths outside workspace (optional)
allow_read_paths = []
# Resource limits for child processes
max_memory_mb = 4096
max_pids = 128
# OCI image when mode = "strict-container" (optional)
# container_image = "rust:1.85-slim"

[impact]
# Scent-Heated Approval: escalates Allow -> Ask when a diff touches public symbols with callers
enabled = true
max_hops = 2          # clamped 1..=4
ask_heat_bp = 2500    # threshold for public scope:src symbols (basis points 0..=10000)
hard_ask_bp = 8000    # hard threshold for any high-fanout symbol
```

> **Monotonicity of *Scent-Heated Approval*:** `[impact]` can only **escalate** `Allow → Ask` and display `# impact: N callers, H hops, scope:src(pub) [heat Xbp]` above the diff in TUI/CLI. It never overrides `deny`, never relaxes `ask` to `allow`, and degrades gracefully to static rules when unindexed or `enabled = false`.

---

## 2.5. Workspace Trust (`~/.onyx/trusted_workspaces.toml`)

To defend against malicious cloned repositories or untrusted PRs (*Indirect Prompt Injection* + `allow = ["shell_exec(*)"]` or arbitrary `sprout.toml` commands), `openOnyx` pins `blake3` hashes of `.onyx/config.toml` and `sprout.toml` per workspace in `~/.onyx/trusted_workspaces.toml`:

- **Clean repository (no `.onyx/config.toml` or `sprout.toml`):** zero friction (`no-project-config`).
- **Untrusted or modified repository (`untrusted` / `modified`, `[UNTRUSTED]` badge in TUI):**
  - Project `.onyx/config.toml` can **only tighten** policy (`deny`/`ask`, stricter sandbox/impact); project `allow` rules, `env_passthrough`, `allow_read_paths`, `container_image`, and any `[sandbox]`/`[impact]` relaxations are ignored.
  - `sprout.toml` blocks service execution (`service_start`, `service_exec`, `stop_cmd`) until explicitly approved.
- **Trust commands:** `openonyx trust` (approve current workspace), `openonyx trust check`, `openonyx trust list`, `openonyx trust revoke`, per-run `--trust-workspace`, or `/trust [status|revoke]` inside the TUI.

---

## 3. Read-Only Control Plane Protection

Even in `BUILD` mode with `--auto-approve`, `onyx-workspace` protects sensitive control-plane paths as **read-only**:

- `.onyx/config.toml` (prevents an agent from widening its own permissions or disabling the sandbox).
- `.onyx/vault/**` (prevents an agent from deleting or tampering with session snapshots and transcripts).
- `.git/hooks/**` and `.git/config` (prevents arbitrary code injection via Git hooks).
- `.env` and `.env.*` (prevents secret exfiltration or corruption).

This check applies across `fs_write`, `fs_edit`, and write inspection on `shell_exec`.

---

## 4. OS Sandbox, `env_clear()` & RAII `ProcessJob`

Every `shell_exec` call (and Sprout local process) runs on top of the shared `onyx-workspace/src/sandbox.rs` layer.

### Sandbox Profiles (`--sandbox`)
| Mode | TUI Badge | Behavior |
| --- | --- | --- |
| `workspace-write` *(default)* | `[SANDBOX:workspace]` (Linux `bwrap` / macOS `sandbox-exec`), `[SANDBOX:env+job]` (Windows), or `[SANDBOX:env+pgid]` | `env_clear()`, OS write confinement (Linux/macOS), Win32 Job Objects / PGID, memory/PID limits, and `offline` network by default. On Windows, use `strict-container` for hard kernel FS/network isolation. |
| `strict-container` | `[SANDBOX:strict]` | Executes commands inside an OCI container (`docker`, `nerdctl`, or `podman`) mounting the workspace. Ideal for unattended automation. |
| `off` | `[SANDBOX:off]` | Disables OS sandbox wrapper (`WorkspacePolicy`, timeout, process cleanup, and Control Plane RO remain active). |

### Network Policy (`--sandbox-net`)
| Policy | Behavior |
| --- | --- |
| `offline` *(default)* | Blocks outbound network traffic and network commands pre-execution, and sets offline tool hints (`CARGO_NET_OFFLINE`, `PIP_NO_INDEX`, `NPM_CONFIG_OFFLINE`). |
| `localhost` | Allows loopback traffic (`127.0.0.1` / `localhost`) for testing local APIs or databases started by `sprout.toml`. |
| `unrestricted` | No outbound network restriction (use only when downloading dependencies). |

### Automatic Elevation Under `--auto-approve`
When `--auto-approve` is passed and config had `mode = "off"`, `openonyx` **automatically elevates** the sandbox to `workspace-write` + `offline` unless you explicitly pass `--sandbox off` on the CLI.

### Strict Environment Sanitization (`env_clear()`)
Before spawning any child process:
1. Calls `env_clear()` and rebuilds a minimal safe allowlist (clean `PATH`, `HOME`/`USERPROFILE`, standard toolchains).
2. Strips **all** secret-bearing variables (`*_API_KEY`, `*_TOKEN`, `AWS_*`, etc.), ensuring `shell_exec` cannot read your `GEMINI_API_KEY` or `OPENAI_API_KEY`.
3. Redirects `TMP`/`TEMP`/`TMPDIR` to a session-scoped temp directory.
4. Scrubs all captured output through `scrub_secrets` before returning to the LLM or recording to transcripts.

### Platform Drivers & RAII `ProcessJob`
- **Windows (`win32-job`)**: wraps commands in a **Win32 Job Object** with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` plus memory/PID limits so timeouts or exiting `openonyx` cleanly terminate the entire child tree.
- **Linux (`bwrap` / `landlock` + `PGID`)**: uses **Bubblewrap (`bwrap`)** when available and passing a live runtime probe (`bwrap_is_usable()`). On hosts like **Ubuntu 24.04** where AppArmor restricts unprivileged user namespaces, `openonyx` detects it automatically and falls back safely while keeping `env_clear()`, offline command blocking, Control Plane RO, and POSIX `PGID` process-group cleanup.
- **macOS (`sandbox-exec` + `PGID`)**: applies Seatbelt `sandbox-exec` profiles alongside `env_clear()` and POSIX process-group cleanup.

Inspect your host's detected sandbox driver and OCI runtime anytime with:

```sh
openonyx doctor
```

---

[← 02. TUI & Daily Workflow](02-tui-and-daily-workflow.md) · [English Index](index-en.md) · [Next: 04. Time-Travel, Snapshots & Forking →](04-time-travel-snapshots-and-forking.md)
