//! Tools `scent_*` de `WorkspaceTools` (FIX-81, Colorado P2-22). Requiere la feature `scent`:
//!
//! ```text
//! cargo test -p onyx-workspace --features scent --test scent_tools
//! ```
//!
//! Igual que el binario `scent` (FIX-72), el workflow actual no activa esta feature, así que
//! este fichero no se compila en CI. Solo API pública: `PermissionRules`, `WorkspacePolicy`,
//! `WorkspaceTools::execute`.
#![cfg(feature = "scent")]

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use onyx_workspace::{PermissionDecision, PermissionRules, WorkspacePolicy, WorkspaceTools};

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct Ws(PathBuf);

impl Ws {
    fn new(prefix: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "onyx_scent_tools_{prefix}_{}_{n}_{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        Self(fs::canonicalize(&dir).unwrap())
    }
    fn policy(&self) -> WorkspacePolicy {
        WorkspacePolicy::new(self.0.clone()).unwrap()
    }
}

impl Drop for Ws {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn rules(deny: &[&str], allow: &[&str]) -> PermissionRules {
    let v = |xs: &[&str]| xs.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    PermissionRules {
        deny: v(deny),
        allow: v(allow),
        ask: Vec::new(),
    }
}

fn seed_secret(ws: &Ws) {
    fs::create_dir_all(ws.0.join("src")).unwrap();
    fs::create_dir_all(ws.0.join("secrets")).unwrap();
    fs::write(
        ws.0.join("src/lib.rs"),
        "pub fn public_api() { secret_token_fn() }\n",
    )
    .unwrap();
    fs::write(
        ws.0.join("secrets/k.rs"),
        "pub fn secret_token_fn(key: &str /* forja-dummy-secret */) {}\n",
    )
    .unwrap();
}

/// Control: la regla `deny fs_read(secrets/*)` bloquea leer el fichero, y sin esa regla
/// `scent_references` sí lo resuelve (el índice funciona).
#[test]
fn deny_rule_blocks_fs_read_and_scent_indexes_without_rules() {
    let ws = Ws::new("ctl");
    seed_secret(&ws);
    let r = rules(&["fs_read(secrets/*)"], &["*"]);
    assert_eq!(
        r.evaluate("fs_read", r#"{"path":"secrets/k.rs"}"#, false),
        PermissionDecision::Deny
    );
    let tools = WorkspaceTools::new(ws.policy());
    let out = tools
        .execute("scent_references", r#"{"target":"secret_token_fn"}"#)
        .unwrap();
    assert!(out.contains("secret_token_fn"), "{out}");
}

/// Si `fs_read(secrets/*)` está denegado, ninguna tool `scent_*` que las mismas reglas
/// permitan puede devolver el contenido (firmas) de `secrets/k.rs`.
#[test]
#[ignore = "BUG: FIX-81 las tools scent_* indexan todo el root sin pasar por deny: con fs_read(secrets/*) denegado, scent_outline/scent_references/scent_impact devuelven firmas de secrets/k.rs (onyx-workspace/src/tools.rs:1711-1806)"]
fn scent_tools_do_not_reveal_files_denied_to_fs_read() {
    let ws = Ws::new("deny");
    seed_secret(&ws);
    let r = rules(&["fs_read(secrets/*)"], &["*"]);
    let tools = WorkspaceTools::new(ws.policy());
    let calls = [
        ("scent_outline", r#"{"path":"secrets/k.rs"}"#),
        ("scent_outline", r#"{}"#),
        ("scent_references", r#"{"target":"secret_token_fn"}"#),
        ("scent_impact", r#"{"target":"public_api"}"#),
    ];
    let mut leaks = Vec::new();
    for (tool, args) in calls {
        if r.evaluate(tool, args, false) != PermissionDecision::Allow {
            continue;
        }
        if let Ok(out) = tools.execute(tool, args) {
            if out.contains("forja-dummy-secret") || out.contains("secrets/k.rs") {
                leaks.push(format!("{tool} {args}"));
            }
        }
    }
    assert!(leaks.is_empty(), "denied file revealed by: {leaks:?}");
}

fn many_fns(ws: &Ws, n: usize) {
    use std::fmt::Write as _;
    let mut src = String::new();
    for i in 0..n {
        writeln!(
            src,
            "pub fn function_number_{i}(a: u32, b: u32) -> u32 {{ a + b }}"
        )
        .unwrap();
    }
    fs::write(ws.0.join("lib.rs"), src).unwrap();
}

/// Cada línea es cabecera/comentario (`#`, `@TOON`) o una fila completa de 4 campos, y la
/// última es `# page:` o `# total:`.
fn whole_rows_and_footer(out: &str) -> Result<(), String> {
    let lines: Vec<&str> = out.lines().filter(|l| !l.is_empty()).collect();
    let last = lines.last().copied().unwrap_or_default();
    if !(last.starts_with("# page:") || last.starts_with("# total:")) {
        return Err(format!("missing page footer, last line: {last:?}"));
    }
    for l in &lines {
        if l.starts_with('#') || l.starts_with("@TOON") {
            continue;
        }
        if l.matches(" | ").count() != 3 {
            return Err(format!("partial row: {l:?}"));
        }
    }
    Ok(())
}

#[test]
fn scent_outline_fits_default_output_limit_with_footer() {
    let ws = Ws::new("fit");
    many_fns(&ws, 20);
    let out = WorkspaceTools::new(ws.policy())
        .execute("scent_outline", "{}")
        .unwrap();
    whole_rows_and_footer(&out).unwrap();
}

#[test]
#[ignore = "BUG: FIX-81 truncate_utf8(max_output_bytes) corta la página TOON de scent_* a mitad de fila y se pierde el # page: (next_cursor) (onyx-workspace/src/tools.rs:1745, :1781, :1834-1843)"]
fn scent_outline_truncation_keeps_whole_rows_and_page_cursor() {
    let ws = Ws::new("trunc");
    many_fns(&ws, 200);
    let mut policy = ws.policy();
    policy.max_output_bytes = 1000;
    let out = WorkspaceTools::new(policy)
        .execute("scent_outline", "{}")
        .unwrap();
    whole_rows_and_footer(&out).unwrap();
}
