//! Auditoría VPS de Adolfo sobre `onyx-workspace` (API pública):
//! FIX-38 (escritura a través de hardlinks hacia fuera del workspace) y FIX-39 (límites
//! `max_pids` / `max_memory_mb` sin aplicar al hijo en Unix). Cada bug lleva su control.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use onyx_workspace::{SandboxConfig, SandboxMode, WorkspacePolicy, WorkspaceTools};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Tempdir con `ws/` (workspace) y `outside/` en el MISMO sistema de ficheros (hardlinks).
struct Env(PathBuf);

impl Env {
    fn new(prefix: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "onyx_vps_{prefix}_{}_{n}_{nanos}",
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
    fn tools(&self) -> WorkspaceTools {
        WorkspaceTools::new(WorkspacePolicy::new(self.ws()).unwrap())
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        // Los comandos de `shell_exec` crean `<temp_dir>/onyx-sandbox-<hash(ws)>` en el
        // proceso de test. No se puede redirigir `TMPDIR` aquí (el entorno del proceso es
        // compartido por los hilos del test: `set_var` es inseguro), así que se borra SOLO el
        // directorio que corresponde a este `ws()` único (su nombre sale de la ruta del
        // workspace); los restos de ejecuciones anteriores no se tocan.
        if let Ok(tmp) = onyx_workspace::ensure_sandbox_tmp_dir(&self.ws()) {
            let _ = fs::remove_dir_all(tmp);
        }
        let _ = fs::remove_dir_all(&self.0);
    }
}

// ───────────── FIX-38: hardlinks ─────────────

/// Control FIX-38: un fichero normal del workspace se escribe y se edita.
#[test]
fn fs_write_and_edit_regular_file_inside_workspace() {
    let env = Env::new("regular");
    fs::write(env.ws().join("a.txt"), "hello\n").unwrap();
    let t = env.tools();
    t.execute(
        "fs_edit",
        r#"{"path":"a.txt","old_text":"hello","new_text":"bye"}"#,
    )
    .unwrap();
    assert_eq!(fs::read_to_string(env.ws().join("a.txt")).unwrap(), "bye\n");
    t.execute("fs_write", r#"{"path":"a.txt","content":"new"}"#)
        .unwrap();
    assert_eq!(fs::read_to_string(env.ws().join("a.txt")).unwrap(), "new");
}

/// FIX-38: `ws/link` es un hardlink a `outside/secret.txt`. La ruta canónica está dentro del
/// workspace, así que `resolve_*` la acepta, pero escribir modifica el inodo compartido: el
/// fichero de fuera cambia.
#[cfg(unix)]
#[test]
#[ignore = "BUG: FIX-38 fs_write y fs_edit escriben a través de un hardlink a un fichero de fuera del workspace: la comprobación es por ruta canónica, no por inodo/nlink, y fs::write trunca el inodo compartido (onyx-workspace/src/tools.rs:1135-1157)"]
fn writes_through_hardlink_do_not_modify_file_outside_workspace() {
    let env = Env::new("hardlink");
    let outside = env.outside().join("secret.txt");
    fs::write(&outside, "original secret\n").unwrap();
    fs::hard_link(&outside, env.ws().join("link")).unwrap();
    let t = env.tools();
    let w = t.execute(
        "fs_write",
        r#"{"path":"link","content":"pwned by fs_write\n"}"#,
    );
    let after_write = fs::read_to_string(&outside).unwrap();
    fs::write(&outside, "original secret\n").unwrap();
    let e = t.execute(
        "fs_edit",
        r#"{"path":"link","old_text":"original","new_text":"pwned by fs_edit"}"#,
    );
    let after_edit = fs::read_to_string(&outside).unwrap();
    let mut breaches = Vec::new();
    if after_write != "original secret\n" {
        breaches.push(format!("fs_write: {w:?}"));
    }
    if after_edit != "original secret\n" {
        breaches.push(format!("fs_edit: {e:?}"));
    }
    assert!(
        breaches.is_empty(),
        "outside file changed through hardlink: {breaches:?}"
    );
}

// ───────────── FIX-39: límites del hijo ─────────────

/// Tope de procesos pedido. Distinto del `Max processes` por defecto de cualquier máquina y
/// MAYOR que los procesos que ya tiene el usuario: `RLIMIT_NPROC` cuenta por usuario, no por
/// hijo, así que un tope bajo (p. ej. 37) hace fallar `fork`/`clone` del propio sandbox
/// (`bwrap: Creating new namespace failed`) en cualquier máquina con más procesos del mismo
/// usuario, aunque la corrección esté bien hecha. Lo que se comprueba es que el límite se
/// APLICA al hijo, no que frene una fork bomb.
const PIDS: u32 = 30_011;
const MEM_MB: u64 = 300;

fn limited_tools(env: &Env, mode: SandboxMode) -> WorkspaceTools {
    let sandbox = SandboxConfig {
        mode,
        max_pids: Some(PIDS),
        max_memory_mb: Some(MEM_MB),
        ..SandboxConfig::default()
    };
    WorkspaceTools::new(
        WorkspacePolicy::new(env.ws())
            .unwrap()
            .with_sandbox(sandbox)
            .with_command_timeout(Duration::from_secs(20)),
    )
}

/// Lo que el hijo observa: `/proc/self/limits` y, si existe, `pids.max`/`memory.max` de su
/// cgroup v2. Cualquiera de los dos mecanismos cuenta como límite aplicado.
///
/// La sonda usa SOLO builtins de `sh` (`read`, `echo`, expansión de parámetros): nada de
/// `cat`, `cut` ni `$(…)`, que hacen `fork`. Con un `RLIMIT_NPROC` bajo (p. ej. 37; límite por
/// USUARIO, y el usuario ya tiene más procesos que eso) un `fork` falla con «Cannot fork» aunque el
/// límite esté bien aplicado, y la sonda daría un falso rojo con la corrección hecha.
const PROBE: &str = r#"while IFS= read -r l; do echo "$l"; done </proc/self/limits
cg=
{ IFS= read -r l; } </proc/self/cgroup 2>/dev/null && cg=${l##*:}
v=; { IFS= read -r v; } </sys/fs/cgroup$cg/pids.max 2>/dev/null; echo "CG_PIDS=$v"
v=; { IFS= read -r v; } </sys/fs/cgroup$cg/memory.max 2>/dev/null; echo "CG_MEM=$v""#;

fn limit_soft(out: &str, label: &str) -> Option<String> {
    out.lines()
        .find(|l| l.starts_with(label))
        .and_then(|l| l[label.len()..].split_whitespace().next())
        .map(str::to_string)
}

fn probe(env: &Env, mode: SandboxMode) -> String {
    let t = limited_tools(env, mode);
    let args = format!(r#"{{"command":{}}}"#, json_escape(PROBE));
    t.execute("shell_exec", &args).unwrap()
}

fn json_escape(s: &str) -> String {
    format!(
        "\"{}\"",
        s.replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
    )
}

fn pids_applied(out: &str) -> bool {
    limit_soft(out, "Max processes").as_deref() == Some(&PIDS.to_string())
        || out.contains(&format!("CG_PIDS={PIDS}\n"))
}

fn mem_applied(out: &str) -> bool {
    let bytes = (MEM_MB * 1024 * 1024).to_string();
    limit_soft(out, "Max address space").as_deref() == Some(&bytes)
        || limit_soft(out, "Max data size").as_deref() == Some(&bytes)
        || out.contains(&format!("CG_MEM={bytes}\n"))
}

/// Control FIX-39: la sonda funciona (el hijo ve sus límites) y lee `Max processes`, `Max
/// address space` y el cgroup sin hacer `fork`.
#[cfg(target_os = "linux")]
#[test]
fn child_limits_probe_reads_proc_self_limits() {
    let env = Env::new("probe");
    let out = probe(&env, SandboxMode::Off);
    assert!(out.contains("Max processes"), "{out}");
    assert!(out.contains("Max address space"), "{out}");
    // La sonda no hace `fork`: ni «Cannot fork» ni comandos externos que falten.
    assert!(
        !out.contains("Cannot fork") && !out.contains("not found"),
        "{out}"
    );
    assert!(out.contains("CG_PIDS=") && out.contains("CG_MEM="), "{out}");
}

/// FIX-39: con `max_pids = 30_011` ([`PIDS`]) y `max_memory_mb = 300` ([`MEM_MB`]), el hijo de `shell_exec` no ve
/// ninguno de los dos límites (ni rlimit ni cgroup). En Unix `ProcessJob::assign` ignora
/// `ProcessLimits`.
#[cfg(target_os = "linux")]
#[test]
#[ignore = "BUG: FIX-39 en Unix max_pids y max_memory_mb no se aplican al hijo de shell_exec: ProcessJob::assign ignora ProcessLimits (solo crea el grupo de procesos) (onyx-workspace/src/sandbox.rs:1824-1834)"]
fn sandbox_limits_are_applied_to_child_process() {
    let env = Env::new("limits");
    let mut missing = Vec::new();
    for mode in [SandboxMode::Off, SandboxMode::WorkspaceWrite] {
        let out = probe(&env, mode);
        if !pids_applied(&out) {
            missing.push(format!("max_pids in {mode:?}"));
        }
        if !mem_applied(&out) {
            missing.push(format!("max_memory_mb in {mode:?}"));
        }
    }
    assert!(missing.is_empty(), "limits not applied: {missing:?}");
}
