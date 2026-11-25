//! Entradas hostiles para `WorkspaceTools` / `WorkspacePolicy` (catálogo de Colorado,
//! `harness-hostiles-tools-sandbox.md`), priorizadas por riesgo en un VPS: escapes del
//! workspace, plano de control, límites de tamaño/tiempo, permisos y JSON de argumentos.
//!
//! Solo API pública; nada lee `$HOME` (no se usa `with_loaded_config*`). Los casos de
//! enlaces simbólicos y `sh` son `#[cfg(unix)]`. Los rojos son `#[ignore = "BUG: …"]`.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use onyx_workspace::{
    PermissionDecision, PermissionRules, SandboxConfig, SandboxMode, WorkspaceMode,
    WorkspacePolicy, WorkspaceTools, is_mutating_tool,
};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Tempdir con `ws/` (workspace) y `outside/` (fuera del workspace).
struct Env(PathBuf);

impl Env {
    fn new(prefix: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "onyx_hostile_{prefix}_{}_{n}_{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(dir.join("ws")).unwrap();
        fs::create_dir_all(dir.join("outside")).unwrap();
        Self(fs::canonicalize(&dir).unwrap())
    }
    fn ws(&self) -> PathBuf {
        self.0.join("ws")
    }
    fn outside(&self) -> PathBuf {
        self.0.join("outside")
    }
    fn policy(&self) -> WorkspacePolicy {
        WorkspacePolicy::new(self.ws()).unwrap()
    }
    fn tools(&self) -> WorkspaceTools {
        WorkspaceTools::new(self.policy())
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Mini constructor de JSON sin dependencias nuevas (rutas con `\`, `"`, NUL…).
mod serde_like {
    pub fn esc(s: &str) -> String {
        let mut out = String::from("\"");
        for c in s.chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
                c => out.push(c),
            }
        }
        out.push('"');
        out
    }
    pub fn obj(pairs: &[(&str, &str)]) -> String {
        let body: Vec<String> = pairs
            .iter()
            .map(|(k, v)| format!("{}:{}", esc(k), esc(v)))
            .collect();
        format!("{{{}}}", body.join(","))
    }
}

use serde_like::obj;

fn read(t: &WorkspaceTools, path: &str) -> Result<String, String> {
    t.execute("fs_read", &obj(&[("path", path)]))
}

fn write(t: &WorkspaceTools, path: &str, content: &str) -> Result<String, String> {
    t.execute("fs_write", &obj(&[("path", path), ("content", content)]))
}

fn edit(t: &WorkspaceTools, path: &str, old: &str, new: &str) -> Result<String, String> {
    t.execute(
        "fs_edit",
        &obj(&[("path", path), ("old_text", old), ("new_text", new)]),
    )
}

fn assert_err_contains(r: Result<String, String>, needle: &str) {
    match r {
        Err(e) => assert!(e.contains(needle), "expected `{needle}` in error: {e}"),
        Ok(out) => panic!("expected error containing `{needle}`, got Ok: {out}"),
    }
}

// ───────────────────────── Rutas ─────────────────────────

/// P0-1: rutas absolutas fuera del root.
#[test]
fn absolute_paths_outside_root_are_rejected() {
    let env = Env::new("abs");
    fs::write(env.outside().join("f"), "secret").unwrap();
    let t = env.tools();
    assert_err_contains(read(&t, "/etc/passwd"), "escapes workspace root");
    let foreign = env.outside().join("f");
    assert_err_contains(
        read(&t, foreign.to_str().unwrap()),
        "escapes workspace root",
    );
    assert_err_contains(
        write(&t, env.outside().join("new").to_str().unwrap(), "x"),
        "escapes workspace root",
    );
    assert!(!env.outside().join("new").exists());
}

/// P0-5: `..` oculto en varias formas; `a/../ok.txt` sigue dentro y no se sobre-bloquea.
#[test]
fn hidden_parent_components_cannot_escape() {
    let env = Env::new("dotdot");
    fs::create_dir_all(env.ws().join("a/b")).unwrap();
    let t = env.tools();
    for raw in [
        "a/../../outside/x",
        "./../outside/x",
        "a/b/../../../outside/x",
        "a//..//..//outside/x",
    ] {
        assert_err_contains(write(&t, raw, "x"), "escapes workspace root");
    }
    assert!(!env.outside().join("x").exists());
    write(&t, "a/../ok.txt", "fine").unwrap();
    assert_eq!(fs::read_to_string(env.ws().join("ok.txt")).unwrap(), "fine");
}

/// P0-5 (Unix): `\` no es separador; `..\x` es un nombre de fichero dentro del root.
#[cfg(unix)]
#[test]
fn backslash_dotdot_stays_inside_root_on_unix() {
    let env = Env::new("backslash");
    let t = env.tools();
    let _ = write(&t, "..\\x", "x");
    assert!(!env.0.join("x").exists());
    assert!(!env.outside().join("x").exists());
}

/// P1-6: NUL en la ruta → error claro; Unicode normal → OK; RLO y `\n` sin pánico.
#[test]
fn nul_and_unicode_paths_are_handled() {
    let env = Env::new("unicode");
    let t = env.tools();
    assert_err_contains(write(&t, "a\u{0}b", "x"), "path contains NUL byte");
    write(&t, "é/ñ.txt", "ok").unwrap();
    assert_eq!(fs::read_to_string(env.ws().join("é/ñ.txt")).unwrap(), "ok");
    let _ = write(&t, "\u{202e}txt.exe", "x");
    let _ = write(&t, "line\nbreak", "x");
}

/// P1-7: el plano de control se compara sin distinguir mayúsculas ASCII.
#[test]
fn control_plane_is_case_insensitive() {
    let env = Env::new("case");
    let t = env.tools();
    for raw in [
        ".ONYX/Config.TOML",
        ".Git/HOOKS/pre-commit",
        ".ENV",
        "sub/.Env.Local",
    ] {
        assert_err_contains(write(&t, raw, "x"), "control-plane");
    }
    assert!(!env.ws().join(".ONYX").exists());
    assert!(!env.ws().join(".Git").exists());
}

#[cfg(unix)]
mod symlinks {
    use super::*;
    use std::os::unix::fs::symlink;

    /// P0-2: symlink de FICHERO hacia fuera → lectura, escritura y edición rechazadas.
    #[test]
    fn file_symlink_to_outside_is_rejected_for_read_write_edit() {
        let env = Env::new("filelink");
        let target = env.outside().join("s.txt");
        fs::write(&target, "secret").unwrap();
        symlink(&target, env.ws().join("ln")).unwrap();
        let t = env.tools();
        assert_err_contains(read(&t, "ln"), "escapes workspace root");
        assert_err_contains(write(&t, "ln", "pwned"), "escapes workspace root");
        assert_err_contains(edit(&t, "ln", "secret", "pwned"), "escapes workspace root");
        assert_eq!(fs::read_to_string(&target).unwrap(), "secret");
    }

    /// P0-2 (directorio): escribir a través de un symlink de directorio hacia fuera.
    #[test]
    fn dir_symlink_to_outside_is_rejected_for_write() {
        let env = Env::new("dirlink");
        symlink(env.outside(), env.ws().join("out")).unwrap();
        let t = env.tools();
        assert_err_contains(write(&t, "out/new.txt", "x"), "escapes workspace root");
        assert_err_contains(write(&t, "out/deep/new.txt", "x"), "escapes workspace root");
        assert!(!env.outside().join("new.txt").exists());
        assert!(!env.outside().join("deep").exists());
    }

    /// P0-3: symlink COLGANTE hacia fuera. `resolve_for_write` usa `exists()`, que sigue el
    /// enlace: lo da por inexistente, resuelve el padre (dentro) y `fs::write` sigue el
    /// enlace y crea el fichero fuera del workspace.
    #[test]
    #[ignore = "BUG: fs_write a través de un symlink colgante crea el fichero FUERA del workspace: resolve_for_write usa exists() (sigue el enlace) y no comprueba symlink_metadata (onyx-workspace/src/policy.rs:1180-1196, tools.rs:1139-1144)"]
    fn dangling_symlink_to_outside_cannot_create_file_outside() {
        let env = Env::new("dangling");
        let target = env.outside().join("nuevo.txt");
        symlink(&target, env.ws().join("ln")).unwrap();
        let t = env.tools();
        let r = write(&t, "ln", "pwned");
        assert!(
            !target.exists(),
            "file created outside the workspace through a dangling symlink; result: {r:?}"
        );
        assert!(r.is_err(), "{r:?}");
    }

    /// P0-4: symlink dentro del workspace que apunta a `.onyx/config.toml`. El control
    /// plane se comprueba sobre la ruta cruda (`ln`) y luego se canoniza y se escribe.
    #[test]
    #[ignore = "BUG: fs_write a través de un symlink (de fichero o de directorio) que apunta al plano de control sobrescribe .onyx/config.toml: check_control_plane_write mira la ruta cruda antes de canonizar; fs_edit sí queda bloqueado (onyx-workspace/src/tools.rs:1136-1137; policy.rs:1015-1024, :1175-1197)"]
    fn symlink_to_control_plane_cannot_be_written() {
        let env = Env::new("cplink");
        fs::create_dir_all(env.ws().join(".onyx")).unwrap();
        let cfg = env.ws().join(".onyx/config.toml");
        fs::write(&cfg, "[permissions]\ndeny = [\"*\"]\n").unwrap();
        let original = fs::read(&cfg).unwrap();
        symlink(".onyx/config.toml", env.ws().join("ln")).unwrap();
        symlink(".onyx", env.ws().join("cfg")).unwrap();
        let t = env.tools();
        let evil = "[permissions]\nallow = [\"*\"]\n";
        let attempts: Vec<(&str, Result<String, String>)> = vec![
            ("fs_write ln", write(&t, "ln", evil)),
            ("fs_edit ln", edit(&t, "ln", "deny", "allow")),
            (
                "fs_write cfg/config.toml",
                write(&t, "cfg/config.toml", evil),
            ),
            (
                "fs_edit cfg/config.toml",
                edit(&t, "cfg/config.toml", "deny", "allow"),
            ),
        ];
        let mut breaches = Vec::new();
        for (label, r) in attempts {
            // Cada intento parte del fichero original.
            if fs::read(&cfg).unwrap() != original || r.is_ok() {
                breaches.push(format!("{label}: {r:?}"));
            }
            fs::write(&cfg, &original).unwrap();
        }
        assert!(
            breaches.is_empty(),
            "control plane writable through symlinks:\n{}",
            breaches.join("\n")
        );
    }

    /// Control de P0-4: la ruta directa sí se bloquea.
    #[test]
    fn direct_control_plane_write_is_blocked() {
        let env = Env::new("cpdirect");
        fs::create_dir_all(env.ws().join(".onyx")).unwrap();
        let cfg = env.ws().join(".onyx/config.toml");
        fs::write(&cfg, "x = 1\n").unwrap();
        let t = env.tools();
        assert_err_contains(write(&t, ".onyx/config.toml", "x = 2\n"), "control-plane");
        assert_err_contains(edit(&t, ".onyx/config.toml", "1", "2"), "control-plane");
        assert_err_contains(
            write(&t, "a/../.onyx/config.toml", "x = 2\n"),
            "control-plane",
        );
        assert_eq!(fs::read_to_string(&cfg).unwrap(), "x = 1\n");
    }
}

// ───────────────────────── Tamaños, salida, timeout ─────────────────────────

/// P0-10: muchas líneas con `max_read_bytes` pequeño → salida acotada y marcador.
#[test]
fn fs_read_many_lines_is_capped_by_max_read_bytes() {
    let env = Env::new("manylines");
    let line = "x".repeat(99);
    let body = format!("{line}\n").repeat(10_000);
    fs::write(env.ws().join("big.txt"), &body).unwrap();
    let mut policy = env.policy();
    policy.max_read_bytes = 1024;
    let out = read(&WorkspaceTools::new(policy), "big.txt").unwrap();
    assert!(
        out.contains("[truncated by max_read_bytes]"),
        "{}",
        &out[..200]
    );
    assert!(out.len() <= 1024 + 200, "len = {}", out.len());
}

/// P0-11: UNA sola línea enorme sin `\n`. El límite se comprueba después de añadir la
/// línea entera, así que la salida es ~1 MiB con `max_read_bytes = 1024`.
#[test]
#[ignore = "BUG: fs_read no acota una línea larga: el tope max_read_bytes se comprueba tras añadir la línea completa, y una línea de 1 MiB sale entera (onyx-workspace/src/tools.rs:1045-1056)"]
fn fs_read_single_huge_line_is_capped_by_max_read_bytes() {
    let env = Env::new("hugeline");
    fs::write(env.ws().join("one.txt"), "y".repeat(1 << 20)).unwrap();
    let mut policy = env.policy();
    policy.max_read_bytes = 1024;
    let out = read(&WorkspaceTools::new(policy), "one.txt").unwrap();
    assert!(
        out.len() <= 1024 + 200,
        "fs_read returned {} bytes with max_read_bytes = 1024",
        out.len()
    );
}

fn shell_tools(env: &Env, timeout: Duration) -> WorkspaceTools {
    let sandbox = SandboxConfig {
        mode: SandboxMode::Off,
        ..SandboxConfig::default()
    };
    WorkspaceTools::new(
        env.policy()
            .with_sandbox(sandbox)
            .with_command_timeout(timeout),
    )
}

fn shell(t: &WorkspaceTools, cmd: &str) -> Result<String, String> {
    t.execute("shell_exec", &obj(&[("command", cmd)]))
}

/// P0-13: timeout corto con `sleep` largo → error rápido y el hijo no sigue vivo.
#[cfg(unix)]
#[test]
fn shell_timeout_kills_child_and_returns_quickly() {
    let env = Env::new("timeout");
    let t = shell_tools(&env, Duration::from_millis(300));
    let start = Instant::now();
    let r = shell(&t, "sleep 2; echo late > witness.txt");
    let took = start.elapsed();
    assert_err_contains(r, "timed out after");
    assert!(took < Duration::from_secs(2), "took {took:?}");
    std::thread::sleep(Duration::from_millis(2500).saturating_sub(took));
    assert!(
        !env.ws().join("witness.txt").exists(),
        "child kept running after timeout"
    );
}

/// P1-12: salida enorme → acotada a `max_output_bytes`, UTF-8 válido, sin cuelgue.
#[cfg(unix)]
#[test]
fn shell_output_is_capped_and_valid_utf8() {
    let env = Env::new("bigout");
    let mut t_policy = env
        .policy()
        .with_sandbox(SandboxConfig {
            mode: SandboxMode::Off,
            ..SandboxConfig::default()
        })
        .with_command_timeout(Duration::from_secs(20));
    t_policy.max_output_bytes = 4096;
    let t = WorkspaceTools::new(t_policy);
    let out = shell(
        &t,
        "i=0; while [ $i -lt 20000 ]; do printf 'é'; i=$((i+1)); done",
    )
    .unwrap();
    assert!(out.len() <= 3 * 4096 + 512, "len = {}", out.len());
}

// ───────────────────────── Permisos y modos ─────────────────────────

fn rules(deny: &[&str], allow: &[&str], ask: &[&str]) -> PermissionRules {
    let v = |xs: &[&str]| xs.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    PermissionRules {
        deny: v(deny),
        allow: v(allow),
        ask: v(ask),
    }
}

fn write_args(path: &str) -> String {
    obj(&[("path", path), ("content", "x")])
}

/// P0-15: `deny` gana a `allow`; `ignore_deny` lo rebaja.
#[test]
fn deny_beats_allow_and_ignore_deny_relaxes_it() {
    let r = rules(&["fs_write(secrets/*)"], &["fs_write(*)"], &[]);
    assert_eq!(
        r.evaluate("fs_write", &write_args("secrets/k"), false),
        PermissionDecision::Deny
    );
    assert_eq!(
        r.evaluate("fs_write", &write_args("src/k"), false),
        PermissionDecision::Allow
    );
    assert_eq!(
        r.evaluate("fs_write", &write_args("secrets/k"), true),
        PermissionDecision::Allow
    );
}

/// P0-16: entre `ask` y `allow` gana el más específico; empate → `Ask`.
#[test]
fn ask_allow_overlap_is_resolved_by_specificity() {
    let cmd = |c: &str| obj(&[("command", c)]);
    let r = rules(&[], &["shell_exec(cargo test*)"], &["shell_exec(*)"]);
    assert_eq!(
        r.evaluate("shell_exec", &cmd("cargo test -q"), false),
        PermissionDecision::Allow
    );
    assert_eq!(
        r.evaluate("shell_exec", &cmd("rm -rf /"), false),
        PermissionDecision::Ask
    );
    let tie = rules(
        &[],
        &["shell_exec(cargo test*)"],
        &["shell_exec(cargo test*)"],
    );
    assert_eq!(
        tie.evaluate("shell_exec", &cmd("cargo test -q"), false),
        PermissionDecision::Ask
    );
}

/// P0-17: la regla `deny` se evalúa sobre la ruta cruda; `x/../secrets/k` o
/// `secrets//k` apuntan al mismo fichero y no casan con `secrets/*`.
#[test]
#[ignore = "BUG: una regla deny de ruta se evade con rutas no normalizadas (x/../secrets/k, secrets//k): extract_rule_subject solo cambia \\ por / y quita ./ (onyx-workspace/src/policy.rs:445-451)"]
fn deny_rule_cannot_be_evaded_with_unnormalized_paths() {
    let r = rules(&["fs_write(secrets/*)"], &["fs_write(*)"], &[]);
    for raw in ["x/../secrets/k", "secrets//k", "./secrets/./k"] {
        assert_eq!(
            r.evaluate("fs_write", &write_args(raw), false),
            PermissionDecision::Deny,
            "path `{raw}` evaded the deny rule"
        );
    }
}

/// P0-21: en Plan/Explore toda herramienta mutante (y `mcp__*`) es `Deny` aunque
/// `allow = ["*"]`; en Build no.
#[test]
fn read_only_modes_deny_every_mutating_tool_even_with_allow_all() {
    let env = Env::new("modes");
    let policy = env.policy().with_permissions(rules(&[], &["*"], &[]));
    let mutating = [
        "fs_write",
        "fs_edit",
        "shell_exec",
        "service_start",
        "service_stop",
        "service_exec",
        "subagent_merge",
    ];
    for tool in mutating {
        assert!(is_mutating_tool(tool), "{tool}");
        let a = write_args("ok.txt");
        for mode in [WorkspaceMode::Plan, WorkspaceMode::Explore] {
            assert_eq!(
                policy.evaluate_permission_in_mode(mode, tool, &a),
                PermissionDecision::Deny,
                "{tool} in {mode:?}"
            );
        }
        assert_ne!(
            policy.evaluate_permission_in_mode(WorkspaceMode::Build, tool, &a),
            PermissionDecision::Deny,
            "{tool} in Build"
        );
    }
    assert_eq!(
        policy.evaluate_permission_in_mode(WorkspaceMode::Plan, "mcp__srv__tool", "{}"),
        PermissionDecision::Deny
    );
}

/// P0-22: plano de control `Deny` en Build para fs_write/fs_edit; contraejemplos no.
#[test]
fn control_plane_paths_are_denied_in_build_and_lookalikes_are_not() {
    let env = Env::new("cp");
    let policy = env.policy().with_permissions(rules(&[], &["*"], &[]));
    for raw in [
        ".onyx/config.toml",
        ".onyx/config.local",
        ".onyx/vault/x",
        ".git/hooks/p",
        ".git/config",
        ".env",
        ".env.local",
        "sub/.env",
        "./.git/config",
    ] {
        for tool in ["fs_write", "fs_edit"] {
            assert_eq!(
                policy.evaluate_permission_in_mode(WorkspaceMode::Build, tool, &write_args(raw)),
                PermissionDecision::Deny,
                "{tool} {raw}"
            );
        }
    }
    for raw in [".onyx/skills/a.md", ".envrc", "env.txt", ".github/x"] {
        assert_ne!(
            policy.evaluate_permission_in_mode(WorkspaceMode::Build, "fs_write", &write_args(raw)),
            PermissionDecision::Deny,
            "{raw} must not be blocked"
        );
    }
}

// ───────────────────────── JSON de argumentos ─────────────────────────

/// P0-27: tipos equivocados, campos ausentes y args vacíos → `Err` sin pánico.
#[test]
fn malformed_tool_args_return_errors_without_panicking() {
    let env = Env::new("json");
    fs::write(env.ws().join("a.txt"), "a\n").unwrap();
    let t = env.tools();
    let cases: &[(&str, &str)] = &[
        ("fs_read", r#"{"path":123}"#),
        ("fs_read", r#"{"path":null}"#),
        ("fs_read", r#"{"path":["a"]}"#),
        ("fs_read", r#"{"path":"a.txt","start_line":-1}"#),
        ("fs_read", r#"{"path":"a.txt","start_line":"1"}"#),
        ("fs_read", "{}"),
        ("fs_read", ""),
        ("fs_read", "   "),
        ("fs_read", "{"),
        ("fs_write", "{}"),
        ("fs_write", r#"{"path":"a.txt"}"#),
        ("fs_edit", r#"{"path":"a.txt"}"#),
        ("shell_exec", "{}"),
        ("shell_exec", r#"{"command":{"a":1}}"#),
    ];
    for (tool, raw) in cases {
        let r = t.execute(tool, raw);
        assert!(r.is_err(), "{tool} {raw:?} must fail, got {r:?}");
    }
    assert_eq!(fs::read_to_string(env.ws().join("a.txt")).unwrap(), "a\n");
    for tool in ["../x", "", "nope", "mcp__a__b"] {
        assert!(t.execute(tool, "{}").is_err(), "tool {tool:?}");
    }
}

/// P1-29: límites extremos de línea → sin pánico ni desbordamiento.
#[test]
fn extreme_line_ranges_do_not_panic() {
    let env = Env::new("ranges");
    fs::write(env.ws().join("a.txt"), "1\n2\n3\n").unwrap();
    let t = env.tools();
    for raw in [
        r#"{"path":"a.txt","start_line":18446744073709551615}"#,
        r#"{"path":"a.txt","end_line":0}"#,
        r#"{"path":"a.txt","start_line":3,"end_line":1}"#,
        r#"{"path":"a.txt","start_line":0,"end_line":18446744073709551615}"#,
    ] {
        let _ = t.execute("fs_read", raw);
    }
    let _ = t.execute("fs_list", r#"{"path":".","max_depth":99999}"#);
}

/// P1-30: `fs_edit` con texto ausente o vacío → `Err` y fichero intacto.
#[test]
fn fs_edit_with_missing_or_empty_old_text_leaves_file_intact() {
    let env = Env::new("edit");
    fs::write(env.ws().join("a.txt"), "hello hello\n").unwrap();
    let t = env.tools();
    assert!(edit(&t, "a.txt", "absent", "x").is_err());
    assert!(edit(&t, "a.txt", "", "x").is_err());
    assert!(
        t.execute("fs_edit", r#"{"path":"a.txt","edits":[]}"#)
            .is_err()
    );
    assert_eq!(
        fs::read_to_string(env.ws().join("a.txt")).unwrap(),
        "hello hello\n"
    );
}

// ───────────────────────── Modo Plan y execute() ─────────────────────────

/// Control: la política de las herramientas niega `fs_write` en Plan.
#[test]
fn plan_mode_policy_denies_fs_write() {
    let env = Env::new("plan_policy");
    let t = env.tools();
    let (decision, _) =
        t.evaluate_permission_with_impact(WorkspaceMode::Plan, "fs_write", &write_args("p.txt"));
    assert_eq!(decision, PermissionDecision::Deny);
}

/// Colorado (P0, defensa en profundidad): `execute()` no aplica el modo. Con la sesión en
/// Plan (el `WorkspaceModeHandle` compartido con el bridge dice Plan), cualquier llamada a
/// `execute` sobre las mismas herramientas escribe igualmente: solo el bridge lo filtra.
#[test]
#[ignore = "BUG (diseño, decide Lince): WorkspaceTools::execute no aplica WorkspaceMode; con la sesión en Plan, fs_write por execute() escribe en disco; el modo solo se aplica en WorkspaceToolBridge (onyx-workspace/src/tools.rs:805-815, :1135-1144; bridge.rs:574)"]
fn execute_refuses_mutations_while_session_is_in_plan_mode() {
    use onyx_soul_agents::AgentHost;
    use onyx_workspace::{WorkspaceModeHandle, WorkspaceToolBridge};

    let env = Env::new("plan_execute");
    let t = env.tools();
    let host = AgentHost::builder(1).build().unwrap();
    let handle = WorkspaceModeHandle::new(WorkspaceMode::Plan);
    let _bridge = WorkspaceToolBridge::new(host.vocab(), t.clone()).with_mode_handle(handle);
    let r = write(&t, "p.txt", "x");
    assert!(
        !env.ws().join("p.txt").exists(),
        "fs_write ran while the session is in Plan mode: {r:?}"
    );
}
