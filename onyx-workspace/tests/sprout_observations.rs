//! Hallazgos de la revisión de `onyx-workspace/src/sprout.rs` numerados por Lince
//! (FIX-32, FIX-33, FIX-34). Cada test fija el comportamiento esperado y queda ignorado
//! hasta que se corrija; con `--ignored` falla hoy.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use onyx_workspace::{SproutConfig, SproutSupervisor, WorkspacePolicy};

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct TempWs(PathBuf);

impl TempWs {
    fn new(prefix: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "onyx_sprout_obs_{prefix}_{}_{n}_{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        Self(fs::canonicalize(&dir).unwrap())
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempWs {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// TOML 1.0: «Defining a table more than once is invalid». El propio parser ya rechaza
/// ids duplicados en `[[service]]` («duplicate service id»), pero dos `[services.api]`
/// se fusionan en silencio y la segunda sobrescribe `cmd` de la primera.
#[test]
#[ignore = "BUG: FIX-32 dos secciones [services.<id>] con el mismo id se fusionan en silencio (named_svcs.entry(id).or_default(), sprout.rs:268) en vez de dar error de id duplicado"]
fn duplicate_named_service_tables_are_rejected() {
    let ws = TempWs::new("dup_table");
    let result = SproutConfig::parse_toml(
        "[services.api]\ncmd = \"echo first\"\n\n[services.api]\ncmd = \"echo second\"\n",
        ws.path(),
        None,
    );
    match result {
        Err(err) => assert!(err.contains("duplicate"), "{err}"),
        Ok(cfg) => panic!(
            "duplicate [services.api] accepted; merged cmd = {:?}",
            cfg.services["api"].cmd
        ),
    }
}

/// La tabla `@TOON v1` separa columnas con `|` (y `as_toon_cell` ya sanea `|` en los
/// motivos de fallo). Un id con `|` no se valida y descuadra la fila: o se rechaza el id al
/// cargar, o cada fila de datos debe seguir teniendo exactamente 6 columnas.
#[test]
#[ignore = "BUG: FIX-33 los ids de servicio no se validan (sprout.rs:262 y :339-358); un id con `|` rompe las columnas de la tabla TOON de status_toon"]
fn service_ids_cannot_break_the_toon_table() {
    let ws = TempWs::new("ids");
    let toml = "[services.\"api|evil\"]\ncmd = \"echo a\"\n";
    if SproutConfig::parse_toml(toml, ws.path(), None).is_err() {
        return; // Corrección válida: id rechazado al cargar.
    }
    fs::write(ws.path().join("sprout.toml"), toml).unwrap();
    let sup = SproutSupervisor::new(WorkspacePolicy::new(ws.path()).unwrap());
    let toon = sup.status_toon().unwrap();
    for row in toon
        .lines()
        .filter(|l| !l.starts_with('#') && *l != "@TOON v1")
    {
        assert_eq!(
            row.split('|').count(),
            6,
            "TOON row must keep 6 columns: {row:?}\n{toon}"
        );
    }
}

/// `SproutSupervisor::new` respeta `policy.trust.sprout_trusted`; `from_config` lo ignora y
/// marca el supervisor como confiado aunque la política diga lo contrario.
#[cfg(unix)]
#[test]
#[ignore = "BUG: FIX-34 SproutSupervisor::from_config fija sprout_trusted=true y Trusted ignorando policy.trust (sprout.rs:912-913); un sprout.toml no confiado puede ejecutar servicios"]
fn from_config_respects_untrusted_policy() {
    use onyx_workspace::{WorkspaceTrustReport, WorkspaceTrustStatus};

    let ws = TempWs::new("from_config_trust");
    let toml = "[services.p]\ncmd = \"echo ran > ran.txt\"\n";
    let report = WorkspaceTrustReport {
        status: WorkspaceTrustStatus::Untrusted {
            has_config: false,
            has_sprout: true,
        },
        sprout_trusted: false,
        ..WorkspaceTrustReport::no_project_config()
    };
    let policy = WorkspacePolicy::new(ws.path())
        .unwrap()
        .with_trust_report(report);
    let cfg = SproutConfig::parse_toml(toml, ws.path(), Some(&policy)).unwrap();

    // Control: `new` con la misma política bloquea.
    fs::write(ws.path().join("sprout.toml"), toml).unwrap();
    let via_new = SproutSupervisor::new(policy.clone());
    assert!(
        via_new
            .start("p", Some(5))
            .unwrap_err()
            .contains("Workspace Trust")
    );

    let via_from_config = SproutSupervisor::from_config(policy, cfg);
    let result = via_from_config.start("p", Some(5));
    assert!(
        result
            .as_ref()
            .is_err_and(|e| e.contains("Workspace Trust")),
        "from_config must honour the untrusted policy: {result:?}"
    );
    assert!(
        !ws.path().join("ran.txt").exists(),
        "no process may run while untrusted"
    );
}
