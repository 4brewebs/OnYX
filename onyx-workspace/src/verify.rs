//! Verify-on-Edit guiado por impacto (`SDD-05` REQ-05.4, TEST-05.3).
//!
//! Tras una mutación exitosa (`fs_edit`/`fs_write`), si `[verify].on_edit = true` y hay
//! `[[verify.rules]]` declarados, se ejecutan los comandos cuya `glob` empareja el
//! archivo editado (y, con `scent`, los archivos `scope:test` alcanzables en ≤ 2 hops)
//! dentro de la misma tubería de sandbox de `shell_exec`. El resultado se inyecta al
//! final de la observación `Ok(...)` del tool:
//!
//! - `\n[verify: PASS] <N> rule(s) succeeded (<cmds>)` (+ `impacted_tests: [...]` si hay)
//! - `\n[verify: FAIL] <cmd> (exit <code>|timed out after Ns)\n<diagnóstico>\n(Fix the error before finishing the turn.)`
//! - `\n[verify: BLOCKED] <cmd> (denied by [permissions])` — sin ejecutar nada.
//!
//! **Seguridad:**
//! - **Workspace Trust Gate:** con `[UNTRUSTED]`, `[verify]`/`[[verify.rules]]` del
//!   `<workspace>/.onyx/config.toml` se ignoran por completo (con aviso accionable en
//!   `policy.trust_warnings`); los del `~/.onyx/config.toml` global sí aplican
//!   (fusión `global < workspace (trusted)`; las reglas de workspace reemplazan a las globales).
//! - **Gobernanza `[permissions].deny`:** cada `command` se evalúa como `shell_exec`
//!   antes de ejecutarse; un `deny` bloquea la regla y detiene el bucle.
//! - Ejecución aislada: sandbox, `ProcessJob`, timeout por regla (`max_secs` 1..=120),
//!   cancelación, `scrub_secrets` y diagnóstico acotado a 4 KiB.

use std::fs;
use std::path::Path;
use std::process::{Child, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::policy::{
    PermissionDecision, WorkspacePolicy, compile_glob_regex, strip_toml_comment_pub,
};
use crate::sandbox::{
    ProcessJob, clean_process_path, configure_process_isolation, ensure_sandbox_tmp_dir,
    prepare_sandboxed_shell_command,
};
use crate::tools::{read_pipe_capped, scrub_secrets, truncate_utf8};

/// Tope de diagnóstico capturado por regla (stdout+stderr) para el feedback inyectado.
pub const VERIFY_DIAGNOSTIC_CAP_BYTES: usize = 4 * 1024;
/// Timeout por regla en segundos, acotado `1..=120` (default del `[verify].max_secs`: 20).
pub const VERIFY_MAX_SECS_DEFAULT: u64 = 20;
const MAX_SECS_MIN: u64 = 1;
const MAX_SECS_MAX: u64 = 120;
/// Hops de radio de impacto AST para el resumen `impacted_tests` (REQ-05.4: ≤ 2).
pub const IMPACTED_TEST_MAX_HOPS: usize = 2;

fn clamp_max_secs(secs: u64) -> u64 {
    secs.clamp(MAX_SECS_MIN, MAX_SECS_MAX)
}

/// Regla declarada en `[[verify.rules]]`: si la `glob` empareja el archivo editado (o un
/// archivo `scope:test` impactado), se ejecuta `command` en sandbox con timeout propio.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyRule {
    /// Glob de ruta relativa (p. ej. `**/*.rs`); se compila con `compile_glob_regex(g, false)`.
    pub glob: String,
    /// Comando de shell ejecutado con la tubería de `shell_exec`.
    pub command: String,
    /// Timeout por regla en segundos (acotado `1..=120`); `None` → `VerifyConfig.max_secs`.
    pub max_secs: Option<u64>,
}

/// Configuración `[verify]` cargada desde `~/.onyx/config.toml` y
/// `<workspace>/.onyx/config.toml` (precedencia: workspace > global > default).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyConfig {
    /// Activa el Verify-on-Edit tras `fs_edit`/`fs_write` exitosos (default: `false`,
    /// opt-in explícito: 0 overhead sin configuración).
    pub on_edit: bool,
    /// Timeout por defecto por regla en segundos (default: `20`, acotado `1..=120`).
    pub max_secs: u64,
    /// Reglas declaradas en orden determinista de declaración.
    pub rules: Vec<VerifyRule>,
}

impl Default for VerifyConfig {
    fn default() -> Self {
        Self {
            on_edit: false,
            max_secs: VERIFY_MAX_SECS_DEFAULT,
            rules: Vec::new(),
        }
    }
}

/// Parche parcial leído desde `[verify]`/`[[verify.rules]]` de un `.onyx/config.toml`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VerifyPatch {
    pub on_edit: Option<bool>,
    pub max_secs: Option<u64>,
    /// `None` → la fuente no declara reglas (se conservan las anteriores);
    /// `Some(vec)` → reemplazo completo del conjunto de reglas.
    pub rules: Option<Vec<VerifyRule>>,
}

impl VerifyConfig {
    /// Aplica un parche parcial sobre la configuración actual (acota `max_secs`).
    pub fn apply_patch(&mut self, patch: VerifyPatch) {
        if let Some(on_edit) = patch.on_edit {
            self.on_edit = on_edit;
        }
        if let Some(max_secs) = patch.max_secs {
            self.max_secs = clamp_max_secs(max_secs);
        }
        if let Some(rules) = patch.rules {
            self.rules = rules
                .into_iter()
                .map(|mut r| {
                    if let Some(ms) = r.max_secs {
                        r.max_secs = Some(clamp_max_secs(ms));
                    }
                    r
                })
                .collect();
        }
    }

    /// Parsea las secciones `[verify]` y `[[verify.rules]]` de un texto TOML (0 deps),
    /// ignorando el resto de secciones. Errores: clave desconocida o duplicada, valor
    /// inválido, o `glob`/`command` ausente/vacío en una regla.
    pub fn parse_toml(input: &str) -> Result<VerifyPatch, String> {
        let mut patch = VerifyPatch::default();
        let text = input.strip_prefix('\u{feff}').unwrap_or(input);
        let mut in_verify = false;
        let mut current_rule: Option<VerifyRule> = None;
        let mut rules: Vec<VerifyRule> = Vec::new();
        let mut rule_index = 0usize;

        for (idx, raw_line) in text.lines().enumerate() {
            let line_no = idx + 1;
            let stripped = strip_toml_comment_pub(raw_line).trim();
            if stripped.is_empty() {
                continue;
            }
            if stripped.starts_with("[[") && stripped.ends_with("]]") {
                let header = stripped[2..stripped.len() - 2].trim();
                if header == "verify.rules" {
                    close_rule(&mut current_rule, rule_index, &mut rules)?;
                    rule_index += 1;
                    current_rule = Some(VerifyRule {
                        glob: String::new(),
                        command: String::new(),
                        max_secs: None,
                    });
                } else {
                    // Cualquier otra array-of-tables termina `[verify]` y regla abierta.
                    close_rule(&mut current_rule, rule_index, &mut rules)?;
                    current_rule = None;
                }
                in_verify = false;
                continue;
            }
            if stripped.starts_with('[') && stripped.ends_with(']') {
                let header = stripped[1..stripped.len() - 1].trim();
                close_rule(&mut current_rule, rule_index, &mut rules)?;
                current_rule = None;
                in_verify = header == "verify";
                continue;
            }

            // Contenido de secciones ajenas (arrays multilínea de `[permissions]`,
            // tablas de `[mcp.servers.*]`, `[[external_drivers]]`, …) nunca rompe
            // `[verify]`: se ignora en bloque.
            if current_rule.is_none() && !in_verify {
                continue;
            }

            let Some((raw_key, raw_val)) = stripped.split_once('=') else {
                return Err(format!(
                    "config.toml line {line_no}: expected `key = value` inside [verify]"
                ));
            };
            let key = raw_key.trim();
            let val = raw_val.trim();

            if let Some(rule) = current_rule.as_mut() {
                // Claves de `[[verify.rules]]` (sin duplicados por regla).
                match key {
                    "glob" => {
                        if !rule.glob.is_empty() {
                            return Err(format!(
                                "config.toml line {line_no}: duplicate key `glob` in [[verify.rules]] entry #{rule_index}"
                            ));
                        }
                        rule.glob = parse_toml_string(val, key, line_no)?;
                    }
                    "command" => {
                        if !rule.command.is_empty() {
                            return Err(format!(
                                "config.toml line {line_no}: duplicate key `command` in [[verify.rules]] entry #{rule_index}"
                            ));
                        }
                        rule.command = parse_toml_string(val, key, line_no)?;
                    }
                    "max_secs" => {
                        if rule.max_secs.is_some() {
                            return Err(format!(
                                "config.toml line {line_no}: duplicate key `max_secs` in [[verify.rules]] entry #{rule_index}"
                            ));
                        }
                        let parsed = val.parse::<u64>().map_err(|_| {
                            format!(
                                "config.toml line {line_no}: `max_secs` must be a positive integer, got `{val}`"
                            )
                        })?;
                        rule.max_secs = Some(parsed);
                    }
                    other => {
                        return Err(format!(
                            "config.toml line {line_no}: unknown key `{other}` in [[verify.rules]] (expected `glob`, `command`, or `max_secs`)"
                        ));
                    }
                }
                continue;
            }

            // Aquí `in_verify == true` (asegurado por el skip de secciones ajenas
            // al inicio del loop) y no hay regla abierta.
            // Claves de `[verify]`.
            match key {
                "on_edit" => {
                    if patch.on_edit.is_some() {
                        return Err(format!(
                            "config.toml line {line_no}: duplicate key `on_edit` in [verify]"
                        ));
                    }
                    let parsed = val.parse::<bool>().map_err(|_| {
                        format!(
                            "config.toml line {line_no}: `on_edit` must be `true` or `false`, got `{val}`"
                        )
                    })?;
                    patch.on_edit = Some(parsed);
                }
                "max_secs" => {
                    if patch.max_secs.is_some() {
                        return Err(format!(
                            "config.toml line {line_no}: duplicate key `max_secs` in [verify]"
                        ));
                    }
                    let parsed = val.parse::<u64>().map_err(|_| {
                        format!(
                            "config.toml line {line_no}: `max_secs` must be a positive integer, got `{val}`"
                        )
                    })?;
                    patch.max_secs = Some(parsed);
                }
                other => {
                    return Err(format!(
                        "config.toml line {line_no}: unknown key `{other}` in [verify] (expected `on_edit` or `max_secs`)"
                    ));
                }
            }
        }

        close_rule(&mut current_rule, rule_index, &mut rules)?;
        if !rules.is_empty() {
            patch.rules = Some(rules);
        }

        Ok(patch)
    }

    /// Carga y fusiona `[verify]`/`[[verify.rules]]` aplicando el **Workspace Trust Gate**:
    /// con `trust_project == false`, el workspace NO confiado ignora por completo su
    /// `[verify]` (con aviso accionable si contenía reglas); el global siempre aplica.
    pub fn load_from_paths_with_trust(
        global_path: Option<&Path>,
        project_path: Option<&Path>,
        trust_project: bool,
    ) -> Result<(Self, Vec<String>), String> {
        let mut cfg = Self::default();
        let mut warnings = Vec::new();

        if let Some(gp) = global_path {
            if gp.is_file() {
                let raw = fs::read_to_string(gp)
                    .map_err(|e| format!("failed to read `{}`: {e}", gp.display()))?;
                cfg.apply_patch(Self::parse_toml(&raw)?);
            }
        }

        if let Some(pp) = project_path {
            if pp.is_file() {
                let raw = fs::read_to_string(pp)
                    .map_err(|e| format!("failed to read `{}`: {e}", pp.display()))?;
                let patch = Self::parse_toml(&raw)?;
                if trust_project {
                    cfg.apply_patch(patch);
                } else if patch.on_edit.is_some()
                    || patch.max_secs.is_some()
                    || patch.rules.as_ref().is_some_and(|r| !r.is_empty())
                {
                    warnings.push(
                        "untrusted project `.onyx/config.toml`: `[verify]` and `[[verify.rules]]` ignored (auto-run shell commands require trust); run `openonyx trust` or `/trust` to approve"
                            .to_string(),
                    );
                }
            }
        }

        Ok((cfg, warnings))
    }
}

/// Extrae una string TOML (basic `"..."` con escapes mínimos `\"` `\\` `\n` `\t`, o
/// literal `'...'` sin escapes) o devuelve error descriptivo.
fn parse_toml_string(value: &str, key: &str, line_no: usize) -> Result<String, String> {
    // String literal TOML: `'...'` sin escapes; no puede contener `'`.
    if let Some(stripped) = value.strip_prefix('\'') {
        let inner = stripped.strip_suffix('\'').ok_or_else(|| {
            format!("config.toml line {line_no}: `{key}` must be a quoted string, got `{value}`")
        })?;
        if inner.contains('\'') {
            return Err(format!(
                "config.toml line {line_no}: `{key}`: single-quoted strings cannot contain `'`"
            ));
        }
        return Ok(inner.to_string());
    }
    let inner = value
        .strip_prefix('"')
        .filter(|_| value.ends_with('"'))
        .and_then(|v| v.strip_suffix('"'))
        .ok_or_else(|| {
            format!(
                "config.toml line {line_no}: `{key}` must be a quoted string (single or double), got `{value}`"
            )
        })?;
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                return Err(format!(
                    "config.toml line {line_no}: invalid escape `\\{other}` in `{key}`"
                ));
            }
            None => {
                return Err(format!(
                    "config.toml line {line_no}: unterminated escape in `{key}`"
                ));
            }
        }
    }
    Ok(out)
}

/// Valida y cierra una regla `[[verify.rules]]` pendiente, empujándola a `rules`.
fn close_rule(
    current_rule: &mut Option<VerifyRule>,
    rule_index: usize,
    rules: &mut Vec<VerifyRule>,
) -> Result<(), String> {
    let Some(rule) = current_rule.take() else {
        return Ok(());
    };
    if rule.glob.is_empty() {
        return Err(format!(
            "config.toml: `[[verify.rules]]` entry #{rule_index}: `glob` cannot be empty"
        ));
    }
    if rule.command.is_empty() {
        return Err(format!(
            "config.toml: `[[verify.rules]]` entry #{rule_index}: `command` cannot be empty"
        ));
    }
    rules.push(rule);
    Ok(())
}

/// Resultado de ejecutar una regla de verificación.
#[derive(Debug)]
enum VerifyRunError {
    /// El comando se ejecutó y devolvió un código de salida no cero (con diagnóstico acotado).
    Exit(i32, String),
    /// El proceso superó el timeout y fue terminado por `ProcessJob` (diagnóstico parcial).
    Timeout(u64, String),
    /// No se pudo lanzar/esperar el proceso.
    Spawn(String),
    /// El usuario canceló la sesión durante la ejecución.
    Cancelled,
}

/// Bucle Verify-on-Edit (`SDD-05` REQ-05.4). Devuelve `None` si `[verify]` está apagado,
/// no hay reglas o ninguna regla empareja los archivos candidatos (sin alterar la salida
/// del tool). En caso contrario devuelve el bloque `[verify: ...]` a adjuntar al final
/// de la observación `Ok(...)` de `fs_edit`/`fs_write`.
///
/// - `rel_path`: archivo editado (relativo, con `/`).
/// - `impacted_tests`: símbolos `scope:test` alcanzables (con feature `scent`, ≤ 2 hops);
///   sin la feature se degrada limpiamente a matching solo por `rel_path`.
/// - `cancel`: bandera de cancelación de la sesión (mismo espíritu que `shell_exec`);
///   solo se invoca en el hilo actual, por lo que no exige `Send + Sync` en el capturador.
pub fn run_verify_on_edit(
    policy: &WorkspacePolicy,
    rel_path: &str,
    impacted_tests: &[String],
    cancel: impl Fn() -> bool,
) -> Option<String> {
    let cfg = &policy.verify;
    if !cfg.on_edit || cfg.rules.is_empty() {
        return None;
    }

    // 1) Candidatos deterministas: el editado primero, luego los archivos de los tests
    //    impactados (≤ 2 hops) sin duplicados.
    let mut candidates: Vec<String> = Vec::new();
    let norm = rel_path.replace('\\', "/");
    if !norm.is_empty() {
        candidates.push(norm);
    }
    for test in impacted_tests {
        if let Some(file) = test.split("::").next() {
            if !file.is_empty() && !candidates.iter().any(|c| c == file) {
                candidates.push(file.to_string());
            }
        }
    }
    if candidates.is_empty() {
        return None;
    }

    // 2) Matching determinista de reglas (orden de declaración, dedup por comando).
    let mut matched: Vec<&VerifyRule> = Vec::new();
    let mut seen_commands: Vec<String> = Vec::new();
    for rule in &cfg.rules {
        let Ok(re) = compile_glob_regex(&rule.glob, false) else {
            continue;
        };
        if !candidates.iter().any(|c| re.is_match(c)) {
            continue;
        }
        if seen_commands.iter().any(|c| c == &rule.command) {
            continue;
        }
        seen_commands.push(rule.command.clone());
        matched.push(rule);
    }
    if matched.is_empty() {
        return None;
    }

    // 3) Ejecución fail-fast con gobernanza `[permissions].deny`.
    // Sufijo determinista `impacted_tests` (mismo en PASS y FAIL, si no está vacío).
    let impacted_suffix = if impacted_tests.is_empty() {
        String::new()
    } else {
        let tests = impacted_tests
            .iter()
            .map(|t| format!("\"{t}\""))
            .collect::<Vec<_>>()
            .join(", ");
        format!(" impacted_tests: [{tests}]")
    };
    let mut succeeded: Vec<&str> = Vec::new();
    for rule in &matched {
        let args_json = format!(
            r#"{{"command":{}}}"#,
            serde_json::to_string(&rule.command).unwrap_or_else(|_| "\"\"".to_string())
        );
        if policy.evaluate_permission("shell_exec", &args_json) == PermissionDecision::Deny {
            return Some(format!(
                "\n[verify: BLOCKED] `{}` (denied by `[permissions]` — the command was not executed; adjust the `deny` rules or run it manually)",
                rule.command
            ));
        }
        let secs = clamp_max_secs(rule.max_secs.unwrap_or(cfg.max_secs));
        match exec_verify_command(policy, &rule.command, Duration::from_secs(secs), &cancel) {
            Ok(()) => succeeded.push(rule.command.as_str()),
            Err(VerifyRunError::Exit(code, diag)) => {
                return Some(format!(
                    "\n[verify: FAIL] `{}` (exit {code})\n{diag}\n(Fix the error before finishing the turn.){impacted_suffix}",
                    rule.command
                ));
            }
            Err(VerifyRunError::Timeout(secs, diag)) => {
                return Some(format!(
                    "\n[verify: FAIL] `{}` (timed out after {secs}s)\n{diag}\n(Fix the error before finishing the turn.){impacted_suffix}",
                    rule.command
                ));
            }
            Err(VerifyRunError::Spawn(msg)) => {
                return Some(format!(
                    "\n[verify: FAIL] `{}` (spawn failed: {msg})\n(Fix the error before finishing the turn.){impacted_suffix}",
                    rule.command
                ));
            }
            Err(VerifyRunError::Cancelled) => {
                return Some(format!(
                    "\n[verify: FAIL] `{}` (cancelled by user)\n(Fix the error before finishing the turn.){impacted_suffix}",
                    rule.command
                ));
            }
        }
    }

    // 4) Feedback PASS (con resumen `impacted_tests` si hay).
    let cmds = succeeded.join(" + ");
    let base = format!(
        "\n[verify: PASS] {} rule(s) succeeded ({cmds})",
        succeeded.len()
    );
    Some(format!("{base}{impacted_suffix}"))
}

/// Ejecuta `command` con la tubería aislada de `shell_exec` y un `timeout` propio.
fn exec_verify_command(
    policy: &WorkspacePolicy,
    command: &str,
    timeout: Duration,
    cancel: &dyn Fn() -> bool,
) -> Result<(), VerifyRunError> {
    let sandbox_tmp = ensure_sandbox_tmp_dir(&policy.root).map_err(VerifyRunError::Spawn)?;
    let mut cmd =
        prepare_sandboxed_shell_command(&policy.sandbox, &policy.root, &sandbox_tmp, command)
            .map_err(VerifyRunError::Spawn)?;
    cmd.current_dir(clean_process_path(&policy.root))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    configure_process_isolation(&mut cmd, false);

    let mut child: Child = cmd
        .spawn()
        .map_err(|e| VerifyRunError::Spawn(format!("failed to spawn verify command: {e}")))?;
    let mut job = ProcessJob::assign(&child, policy.sandbox.limits());
    let out_handle = read_pipe_capped(child.stdout.take(), VERIFY_DIAGNOSTIC_CAP_BYTES);
    let err_handle = read_pipe_capped(child.stderr.take(), VERIFY_DIAGNOSTIC_CAP_BYTES);
    let deadline = Instant::now() + timeout;

    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                let cancelled = cancel();
                if cancelled || Instant::now() >= deadline {
                    if let Some(ref mut j) = job {
                        j.terminate();
                    } else {
                        #[cfg(windows)]
                        {
                            let _ = std::process::Command::new("taskkill")
                                .args(["/F", "/T", "/PID", &child.id().to_string()])
                                .stdout(Stdio::null())
                                .stderr(Stdio::null())
                                .status();
                        }
                    }
                    let _ = child.kill();
                    let _ = child.wait();
                    drop(job);
                    if cancelled {
                        let _ = out_handle.join();
                        let _ = err_handle.join();
                        return Err(VerifyRunError::Cancelled);
                    }
                    // Captura la salida parcial ya emitida por el proceso (si lo hubo)
                    // como diagnóstico acotado para el feedback `[verify: FAIL]`.
                    let partial_out = scrub_secrets(&out_handle.join().unwrap_or_default());
                    let partial_err = scrub_secrets(&err_handle.join().unwrap_or_default());
                    let diag = truncate_utf8(
                        &format!("stdout:\n{partial_out}\nstderr:\n{partial_err}"),
                        VERIFY_DIAGNOSTIC_CAP_BYTES,
                    );
                    let secs = timeout.as_secs().max(1);
                    return Err(VerifyRunError::Timeout(secs, diag));
                }
                thread::sleep(Duration::from_millis(20));
            }
            Err(e) => {
                if let Some(ref mut j) = job {
                    j.terminate();
                }
                drop(job);
                return Err(VerifyRunError::Spawn(format!("wait failed: {e}")));
            }
        }
    };

    // Termina explícitamente el `ProcessJob` RAII (sin huérfanos en Windows).
    if let Some(ref mut j) = job {
        j.terminate();
    }
    drop(job);
    let stdout = scrub_secrets(&out_handle.join().unwrap_or_default());
    let stderr = scrub_secrets(&err_handle.join().unwrap_or_default());
    let code = status.code().unwrap_or(-1);
    if code == 0 {
        Ok(())
    } else {
        let diag = truncate_utf8(
            &format!("stdout:\n{stdout}\nstderr:\n{stderr}"),
            VERIFY_DIAGNOSTIC_CAP_BYTES,
        );
        Err(VerifyRunError::Exit(code, diag))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "onyx_verify_{prefix}_{nanos}_{}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn verify_defaults_are_opt_in_and_max_secs_is_clamped() {
        assert!(!VerifyConfig::default().on_edit);
        assert_eq!(VerifyConfig::default().max_secs, 20);
        assert!(VerifyConfig::default().rules.is_empty());

        let patch =
            VerifyConfig::parse_toml("[verify]\non_edit = true\nmax_secs = 9999\n").unwrap();
        let mut cfg = VerifyConfig::default();
        cfg.apply_patch(patch);
        assert!(cfg.on_edit);
        assert_eq!(cfg.max_secs, 120);

        let patch = VerifyConfig::parse_toml("[verify]\nmax_secs = 0\n").unwrap();
        let mut cfg = VerifyConfig::default();
        cfg.apply_patch(patch);
        assert_eq!(cfg.max_secs, 1);
    }

    #[test]
    fn verify_rules_parse_globs_commands_and_per_rule_timeout() {
        let patch = VerifyConfig::parse_toml(
            r#"
[verify]
on_edit = true

[[verify.rules]]
glob = "**/*.rs"
command = "cargo check --message-format=short"
max_secs = 45

[[verify.rules]]
glob = "src/**/*_test.go"
command = "go vet ./..."
"#,
        )
        .unwrap();
        let mut cfg = VerifyConfig::default();
        cfg.apply_patch(patch);
        assert!(cfg.on_edit);
        assert_eq!(cfg.rules.len(), 2);
        assert_eq!(cfg.rules[0].glob, "**/*.rs");
        assert_eq!(cfg.rules[0].command, "cargo check --message-format=short");
        assert_eq!(cfg.rules[0].max_secs, Some(45));
        assert_eq!(cfg.rules[1].glob, "src/**/*_test.go");
        assert_eq!(cfg.rules[1].command, "go vet ./...");
        assert_eq!(cfg.rules[1].max_secs, None);
    }

    #[test]
    fn verify_parse_rejects_unknown_keys_empty_globs_and_missing_command() {
        assert!(VerifyConfig::parse_toml("[verify]\nunknown_key = true\n").is_err());
        assert!(VerifyConfig::parse_toml("[verify]\non_edit = \"yes\"\n").is_err());
        assert!(VerifyConfig::parse_toml("[verify]\non_edit = true\non_edit = false\n").is_err());
        assert!(
            VerifyConfig::parse_toml("[[verify.rules]]\nglob = \"\"\ncommand = \"echo x\"\n")
                .is_err()
        );
        assert!(VerifyConfig::parse_toml("[[verify.rules]]\nglob = \"*.rs\"\n").is_err());
        assert!(
            VerifyConfig::parse_toml("[[verify.rules]]\nglob = \"*.rs\"\ncommand = 12\n").is_err()
        );
        assert!(
            VerifyConfig::parse_toml(
                "[[verify.rules]]\nglob = \"*.rs\"\ncommand = \"x\"\nextra = 1\n"
            )
            .is_err()
        );
        // `max_secs` inválido y duplicados.
        assert!(
            VerifyConfig::parse_toml(
                "[[verify.rules]]\nglob = \"*.rs\"\ncommand = \"x\"\nmax_secs = \"fast\"\n"
            )
            .is_err()
        );
        assert!(
            VerifyConfig::parse_toml(
                "[[verify.rules]]\nglob = \"*.rs\"\nglob = \"*.go\"\ncommand = \"x\"\n"
            )
            .is_err()
        );
        // Secciones ajenas no contaminan el parseo.
        let patch = VerifyConfig::parse_toml(
            r#"
[sandbox]
mode = "off"

[verify]
on_edit = true

[permissions]
deny = ["shell_exec(*)"]
"#,
        )
        .unwrap();
        assert_eq!(patch.on_edit, Some(true));
        assert!(patch.rules.is_none());
    }

    #[test]
    fn verify_parse_ignores_foreign_sections_with_multiline_arrays_and_bare_lines() {
        // Arrays multilínea de `[permissions]`, tablas `[mcp.servers.*]` y
        // `[[external_drivers]]` (líneas sin `=`) antes de `[verify]` no rompen el parseo.
        let patch = VerifyConfig::parse_toml(
            r#"
[permissions]
deny = [
  "shell_exec(rm*)",
  "shell_exec(sudo*)",
]

[mcp.servers.local]
command = "node"
args = [
  "/srv/mcp.js",
]

[[external_drivers]]
id = "opencode"
args = [
  "run",
  "--quiet",
]

[verify]
on_edit = true
max_secs = 30

[[verify.rules]]
glob = "**/*.rs"
command = "cargo check --message-format=short"
"#,
        )
        .unwrap();
        assert_eq!(patch.on_edit, Some(true));
        assert_eq!(patch.max_secs, Some(30));
        let rules = patch.rules.expect("rules must parse");
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].glob, "**/*.rs");
        assert_eq!(rules[0].command, "cargo check --message-format=short");
    }

    #[test]
    fn verify_parse_accepts_single_quoted_literal_strings() {
        let patch = VerifyConfig::parse_toml(
            r#"
[[verify.rules]]
glob = '**/*.rs'
command = 'cargo check --message-format=short'
"#,
        )
        .unwrap();
        let rule = &patch.rules.as_ref().expect("rules")[0];
        assert_eq!(rule.glob, "**/*.rs");
        assert_eq!(rule.command, "cargo check --message-format=short");

        // Literal sin comilla de cierre → error.
        assert!(
            VerifyConfig::parse_toml("[[verify.rules]]\nglob = '*\ncommand = \"x\"\n").is_err()
        );
        // `"` dentro de un literal es válido tal cual (sin escapes).
        let patch = VerifyConfig::parse_toml(
            r#"
[[verify.rules]]
glob = '*.rs'
command = 'echo "a literal quote"'
"#,
        )
        .unwrap();
        assert_eq!(
            patch.rules.as_ref().unwrap()[0].command,
            "echo \"a literal quote\""
        );
    }

    #[test]
    fn verify_untrusted_workspace_is_ignored_with_actionable_warning() {
        let dir = temp_dir("untrusted");
        fs::create_dir_all(dir.join(".onyx")).unwrap();
        fs::write(
            dir.join(".onyx/config.toml"),
            r#"
[verify]
on_edit = true

[[verify.rules]]
glob = "**/*.rs"
command = "dangerous-cmd"
"#,
        )
        .unwrap();
        let project = dir.join(".onyx").join("config.toml");

        // No confiado: [verify] ignorado por completo + aviso accionable.
        let (cfg, warns) =
            VerifyConfig::load_from_paths_with_trust(None, Some(&project), false).unwrap();
        assert_eq!(cfg, VerifyConfig::default());
        assert_eq!(warns.len(), 1);
        assert!(warns[0].contains("ignored"), "{}", warns[0]);
        assert!(warns[0].contains("openonyx trust"), "{}", warns[0]);

        // Confiado: se aplica.
        let (cfg, warns) =
            VerifyConfig::load_from_paths_with_trust(None, Some(&project), true).unwrap();
        assert!(cfg.on_edit);
        assert_eq!(cfg.rules.len(), 1);
        assert!(warns.is_empty());

        // Sin contenido [verify] en un proyecto no confiado: sin warnings.
        fs::write(dir.join(".onyx/config.toml"), "[sandbox]\nmode = \"off\"\n").unwrap();
        let (cfg, warns) =
            VerifyConfig::load_from_paths_with_trust(None, Some(&project), false).unwrap();
        assert_eq!(cfg, VerifyConfig::default());
        assert!(warns.is_empty());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn verify_workspace_rules_replace_global_when_trusted() {
        let dir = temp_dir("merge");
        fs::create_dir_all(dir.join(".onyx")).unwrap();
        let global = dir.join("global.toml");
        let project = dir.join(".onyx").join("config.toml");
        fs::write(
            &global,
            r#"
[verify]
on_edit = true
max_secs = 15

[[verify.rules]]
glob = "**/*.rs"
command = "global-cmd"
"#,
        )
        .unwrap();
        fs::write(
            &project,
            r#"
[verify]
max_secs = 3

[[verify.rules]]
glob = "**/*.go"
command = "workspace-cmd"
"#,
        )
        .unwrap();

        let (cfg, _) =
            VerifyConfig::load_from_paths_with_trust(Some(&global), Some(&project), true).unwrap();
        assert!(
            cfg.on_edit,
            "global on_edit survives (workspace did not set it)"
        );
        assert_eq!(cfg.max_secs, 3, "workspace max_secs wins");
        assert_eq!(cfg.rules.len(), 1, "workspace rules replace global rules");
        assert_eq!(cfg.rules[0].command, "workspace-cmd");

        // Sin reglas en el workspace: las globales se conservan.
        fs::write(&project, "[verify]\nmax_secs = 4\n").unwrap();
        let (cfg, _) =
            VerifyConfig::load_from_paths_with_trust(Some(&global), Some(&project), true).unwrap();
        assert_eq!(cfg.rules.len(), 1);
        assert_eq!(cfg.rules[0].command, "global-cmd");
        assert_eq!(cfg.max_secs, 4);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn verify_permissions_deny_blocks_without_executing() {
        let dir = temp_dir("blocked");
        let policy = WorkspacePolicy::new(&dir)
            .unwrap()
            .with_permissions(
                crate::policy::PermissionRules::parse_toml(
                    "[permissions]\ndeny = [\"shell_exec(*)\"]\n",
                )
                .unwrap(),
            )
            .with_verify(VerifyConfig {
                on_edit: true,
                max_secs: 5,
                rules: vec![VerifyRule {
                    glob: "**/*.rs".to_string(),
                    command: "touch should_not_run.txt".to_string(),
                    max_secs: None,
                }],
            });

        let out = run_verify_on_edit(&policy, "src/a.rs", &[], || false)
            .expect("a matching rule must produce a block");
        assert!(out.contains("[verify: BLOCKED]"), "{out}");
        assert!(out.contains("denied by `[permissions]`"), "{out}");
        assert!(
            !dir.join("should_not_run.txt").exists(),
            "deny must not execute"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn verify_no_matching_rule_yields_no_block() {
        let dir = temp_dir("nomatch");
        let policy = WorkspacePolicy::new(&dir)
            .unwrap()
            .with_verify(VerifyConfig {
                on_edit: true,
                max_secs: 5,
                rules: vec![VerifyRule {
                    glob: "**/*.go".to_string(),
                    command: "echo never".to_string(),
                    max_secs: None,
                }],
            });

        assert!(run_verify_on_edit(&policy, "src/a.rs", &[], || false).is_none());
        // Un test impactado cuyo archivo tampoco empareja: nada que ejecutar.
        assert!(
            run_verify_on_edit(&policy, "src/a.rs", &["src/a.rs::t".to_string()], || false)
                .is_none()
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn verify_disabled_or_empty_rules_yield_no_block() {
        let dir = temp_dir("off");
        let policy_off = WorkspacePolicy::new(&dir).unwrap();
        assert!(run_verify_on_edit(&policy_off, "src/a.rs", &[], || false).is_none());

        let policy_empty = WorkspacePolicy::new(&dir)
            .unwrap()
            .with_verify(VerifyConfig {
                on_edit: true,
                max_secs: 5,
                rules: Vec::new(),
            });
        assert!(run_verify_on_edit(&policy_empty, "src/a.rs", &[], || false).is_none());
        let _ = fs::remove_dir_all(&dir);
    }
}
