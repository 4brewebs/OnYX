//! Tests de comportamiento de `onyx-sprout` (`onyx-workspace/src/sprout.rs`) usando solo la
//! API pública del crate.
//!
//! - Parser de `sprout.toml`, validadores de nombres (`container`, `exec_user`) y DAG.
//! - Propiedades sobre DAGs aleatorios con un generador `xorshift64` propio y semillas fijas.
//! - `SproutSupervisor` con procesos reales triviales (`sh -c`) y una CLI de contenedores falsa
//!   (script `sh` en un directorio temporal) vía `with_container_cli_override`: sin Docker.
//!
//! Los tests que lanzan procesos dependen de `sh` y van marcados `#[cfg(unix)]`.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use onyx_workspace::{
    ContainerCli, ServiceKind, ServiceStatus, SproutConfig, SproutServiceSpec, WorkspacePolicy,
    audit_sprout_workspace, detect_container_cli, format_services_panel_from_toon,
    validate_container_name, validate_exec_user,
};

// ---------------------------------------------------------------------------
// Utilidades
// ---------------------------------------------------------------------------

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Directorio temporal único (sin crates): `temp_dir()/onyx_sprout_<prefix>_<pid>_<n>_<nanos>`.
struct TempWs(PathBuf);

impl TempWs {
    fn new(prefix: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "onyx_sprout_{prefix}_{}_{n}_{nanos}",
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

fn parse(toml: &str, root: &Path) -> Result<SproutConfig, String> {
    SproutConfig::parse_toml(toml, root, None)
}

fn parse_ok(toml: &str) -> SproutConfig {
    let ws = TempWs::new("parse");
    parse(toml, ws.path()).unwrap_or_else(|e| panic!("expected valid sprout.toml: {e}\n{toml}"))
}

fn parse_err(toml: &str) -> String {
    let ws = TempWs::new("parse_err");
    match parse(toml, ws.path()) {
        Ok(cfg) => panic!("expected parse error, got {cfg:#?}\n{toml}"),
        Err(e) => e,
    }
}

fn spec(id: &str, deps: &[&str]) -> SproutServiceSpec {
    SproutServiceSpec {
        id: id.to_string(),
        name: id.to_string(),
        kind: ServiceKind::Process,
        cwd: PathBuf::from("."),
        raw_cwd: ".".to_string(),
        cmd: "true".to_string(),
        stop_cmd: None,
        logs_cmd: None,
        container: None,
        exec_user: None,
        debug_port: None,
        ready_port: None,
        ready_log: None,
        ready_cmd: None,
        optional: false,
        enabled: true,
        depends_on: deps.iter().map(|d| d.to_string()).collect(),
    }
}

/// Construye un `SproutConfig` directamente (campos públicos), sin pasar por el parser.
fn config_of(edges: &[(&str, &[&str])]) -> SproutConfig {
    let services = edges
        .iter()
        .map(|(id, deps)| (id.to_string(), spec(id, deps)))
        .collect();
    SproutConfig {
        container_cli: None,
        env_wrapper: None,
        services,
    }
}

fn pos(order: &[String], id: &str) -> usize {
    order
        .iter()
        .position(|x| x == id)
        .unwrap_or_else(|| panic!("`{id}` missing from {order:?}"))
}

// ---------------------------------------------------------------------------
// ServiceKind / ServiceStatus
// ---------------------------------------------------------------------------

#[test]
fn service_kind_parse_accepts_documented_aliases_case_and_whitespace_insensitive() {
    for raw in ["process", "proc", "PROCESS", "  Proc\t"] {
        assert_eq!(
            ServiceKind::parse(raw),
            Some(ServiceKind::Process),
            "{raw:?}"
        );
    }
    for raw in ["container", "docker", "oci", "Docker", " OCI "] {
        assert_eq!(
            ServiceKind::parse(raw),
            Some(ServiceKind::Container),
            "{raw:?}"
        );
    }
}

#[test]
fn service_kind_round_trips_through_as_str() {
    for kind in [ServiceKind::Process, ServiceKind::Container] {
        assert_eq!(ServiceKind::parse(kind.as_str()), Some(kind));
    }
    assert_eq!(ServiceKind::Process.as_str(), "process");
    assert_eq!(ServiceKind::Container.as_str(), "container");
}

#[test]
fn service_kind_rejects_unknown_or_near_miss_values() {
    for raw in [
        "",
        "   ",
        "processes",
        "pro",
        "dock er",
        "podman",
        "container;rm",
        "pr\u{043e}cess", // `о` cirílica
    ] {
        assert_eq!(ServiceKind::parse(raw), None, "{raw:?}");
    }
}

#[test]
fn service_status_badges_and_toon_cells() {
    assert_eq!(ServiceStatus::Off.as_badge(), "OFF");
    assert_eq!(ServiceStatus::Pending.as_badge(), "PENDING");
    assert_eq!(ServiceStatus::Starting.as_badge(), "STARTING");
    assert_eq!(ServiceStatus::Ready.as_badge(), "READY");
    assert_eq!(ServiceStatus::Failed("x".into()).as_badge(), "FAILED");
    assert_eq!(ServiceStatus::Ready.as_toon_cell(), "READY");
    // Motivo vacío: solo el badge.
    assert_eq!(
        ServiceStatus::Failed(String::new()).as_toon_cell(),
        "FAILED"
    );
    // `|` y saltos de línea no pueden romper la tabla TOON.
    assert_eq!(
        ServiceStatus::Failed("a|b\nc".into()).as_toon_cell(),
        "FAILED(a/b c)"
    );
}

#[test]
fn service_status_toon_cell_truncates_long_reasons_at_48_bytes_on_char_boundary() {
    let exactly_48 = "x".repeat(48);
    assert_eq!(
        ServiceStatus::Failed(exactly_48.clone()).as_toon_cell(),
        format!("FAILED({exactly_48})")
    );
    let reason_49 = "y".repeat(49);
    assert_eq!(
        ServiceStatus::Failed(reason_49).as_toon_cell(),
        format!("FAILED({}...)", "y".repeat(48))
    );
    // 47 ASCII + `é` (2 bytes): el corte en 48 cae dentro de `é` y debe retroceder a 47.
    let mut tricky = "z".repeat(47);
    tricky.push('é');
    tricky.push_str("tail");
    assert_eq!(
        ServiceStatus::Failed(tricky).as_toon_cell(),
        format!("FAILED({}...)", "z".repeat(47))
    );
}

// ---------------------------------------------------------------------------
// validate_container_name
// ---------------------------------------------------------------------------

#[test]
fn container_name_accepts_valid_names_and_trims() {
    for (raw, expected) in [
        ("a", "a"),
        ("app-postgres", "app-postgres"),
        ("App_DB.1-x", "App_DB.1-x"),
        ("9lives", "9lives"),
        ("a..b", "a..b"),
        ("  app-redis \t", "app-redis"),
    ] {
        assert_eq!(
            validate_container_name(raw).as_deref(),
            Ok(expected),
            "{raw:?}"
        );
    }
}

#[test]
fn container_name_length_limit_is_128_bytes() {
    let ok = format!("a{}", "b".repeat(127));
    assert_eq!(ok.len(), 128);
    assert_eq!(validate_container_name(&ok), Ok(ok.clone()));
    let too_long = format!("{ok}c");
    let err = validate_container_name(&too_long).unwrap_err();
    assert!(err.contains("128"), "{err}");
    // El límite se mide tras recortar espacios.
    assert!(validate_container_name(&format!("  {ok}  ")).is_ok());
}

#[test]
fn container_name_rejects_empty_flags_and_shell_injection() {
    let hostile = [
        "",
        "   ",
        "-u root",
        "--privileged",
        "-x",
        ".hidden",
        "..",
        "../etc",
        "_under",
        "app;rm -rf /",
        "app$(id)",
        "app`id`",
        "app name",
        "app\tname",
        "app\nname",
        "app/sub",
        "app\\sub",
        "app|cat",
        "app&",
        "app>out",
        "app'q",
        "app\"q",
        "app:tag",
        "app@sha",
        "café",
        "\u{ff41}pp", // `ａ` de ancho completo
        "app\0x",
        "\0",
    ];
    for raw in hostile {
        assert!(
            validate_container_name(raw).is_err(),
            "must reject container name {raw:?}"
        );
    }
}

#[test]
fn container_name_errors_explain_the_rule() {
    let err = validate_container_name("--privileged").unwrap_err();
    assert!(err.contains("must start with"), "{err}");
    let err = validate_container_name("a;b").unwrap_err();
    assert!(err.contains("[A-Za-z0-9_.-]"), "{err}");
    let err = validate_container_name("  ").unwrap_err();
    assert!(err.contains("empty"), "{err}");
}

// ---------------------------------------------------------------------------
// validate_exec_user
// ---------------------------------------------------------------------------

#[test]
fn exec_user_accepts_documented_forms() {
    for (raw, expected) in [
        ("nobody", "nobody"),
        ("1000", "1000"),
        ("1000:1000", "1000:1000"),
        ("_apt", "_apt"),
        ("app-user:app.group", "app-user:app.group"),
        ("  www-data  ", "www-data"),
    ] {
        assert_eq!(validate_exec_user(raw).as_deref(), Ok(expected), "{raw:?}");
    }
}

#[test]
fn exec_user_segment_and_total_length_limits() {
    let seg64 = format!("u{}", "x".repeat(63));
    let seg65 = format!("{seg64}y");
    assert!(validate_exec_user(&seg64).is_ok());
    assert!(validate_exec_user(&seg65).is_err());
    // 64 + ':' + 64 = 129: máximo aceptado.
    let max = format!("{seg64}:{seg64}");
    assert_eq!(max.len(), 129);
    assert_eq!(validate_exec_user(&max), Ok(max.clone()));
    assert!(validate_exec_user(&format!("{seg64}:{seg65}")).is_err());
}

#[test]
fn exec_user_rejects_flags_separators_and_injection() {
    let hostile = [
        "",
        "  ",
        ":",
        "user:",
        ":group",
        "a:b:c",
        "-u",
        "--user=root",
        "root;id",
        "root$(id)",
        "root`id`",
        "root id",
        "root\tid",
        "root\nid",
        "a: b",
        "a/b",
        "../root",
        ".root",
        "-1000",
        "root|x",
        "r\u{f6}\u{f6}t",
        "root\0",
        "1000:-g",
    ];
    for raw in hostile {
        assert!(
            validate_exec_user(raw).is_err(),
            "must reject exec_user {raw:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// SproutConfig::parse_toml / load_from_file
// ---------------------------------------------------------------------------

#[test]
fn parse_minimal_service_applies_documented_defaults() {
    let ws = TempWs::new("defaults");
    let cfg = parse("[services.api]\ncmd = \"echo hi\"\n", ws.path()).unwrap();
    assert_eq!(cfg.container_cli, None);
    assert_eq!(cfg.env_wrapper, None);
    let api = &cfg.services["api"];
    assert_eq!(api.id, "api");
    assert_eq!(api.name, "api", "name defaults to id");
    assert_eq!(api.kind, ServiceKind::Process);
    assert_eq!(api.cmd, "echo hi");
    assert_eq!(api.raw_cwd, ".");
    assert_eq!(api.cwd, ws.path());
    assert!(api.enabled, "enabled defaults to true");
    assert!(!api.optional);
    assert!(api.depends_on.is_empty());
    assert_eq!(api.stop_cmd, None);
    assert_eq!(api.container, None);
}

#[test]
fn parse_empty_input_is_an_empty_valid_config() {
    let cfg = parse_ok("");
    assert!(cfg.services.is_empty());
    assert_eq!(cfg.topo_order(), Ok(vec![]));
    let cfg = parse_ok("# solo comentarios\n\n[runtime]\n");
    assert!(cfg.services.is_empty());
}

#[test]
fn parse_strips_bom_and_comments_but_not_hash_inside_strings() {
    let toml = "\u{feff}# cabecera\n[services.api] # comentario\ncmd = \"echo '#no-es-comentario'\" # si\nname = 'a # b'\n";
    let cfg = parse_ok(toml);
    assert_eq!(cfg.services["api"].cmd, "echo '#no-es-comentario'");
    assert_eq!(cfg.services["api"].name, "a # b");
}

#[test]
fn parse_basic_string_escapes_and_literal_strings() {
    let toml = r#"
[services.a]
cmd = "echo \"q\" \\ tab\there"
name = 'C:\path\no\escape'
ready_log = "line\nnext"
"#;
    let cfg = parse_ok(toml);
    let a = &cfg.services["a"];
    assert_eq!(a.cmd, "echo \"q\" \\ tab\there");
    assert_eq!(a.name, r"C:\path\no\escape");
    assert_eq!(a.ready_log.as_deref(), Some("line\nnext"));
}

#[test]
fn parse_rejects_unquoted_or_non_string_values_with_line_number() {
    let err = parse_err("[services.a]\ncmd = echo hi\n");
    assert!(err.contains("line 2"), "{err}");
    assert!(err.contains("expected quoted string"), "{err}");

    let err = parse_err("[services.a]\ncmd = \"x\"\nname = 42\n");
    assert!(err.contains("line 3"), "{err}");

    let err = parse_err("[services.a]\ncmd = \"unterminated\n");
    assert!(err.contains("line 2"), "{err}");
}

#[test]
fn parse_rejects_line_without_equals_sign() {
    let err = parse_err("[services.a]\ncmd = \"x\"\njust some words\n");
    assert!(err.contains("line 3"), "{err}");
    assert!(err.contains("key = value"), "{err}");
}

#[test]
fn parse_rejects_empty_service_id_in_header() {
    for header in ["[services.\"\"]", "[services.'']", "[services.  ]"] {
        let err = parse_err(&format!("{header}\ncmd = \"x\"\n"));
        assert!(err.contains("empty service id"), "{header}: {err}");
    }
}

#[test]
fn parse_quoted_service_id_in_header_is_unquoted() {
    let cfg = parse_ok("[services.\"api-v2\"]\ncmd = \"x\"\n");
    assert!(cfg.services.contains_key("api-v2"));
}

#[test]
fn parse_requires_cmd_or_up() {
    let err = parse_err("[services.api]\nname = \"API\"\n");
    assert!(err.contains("service `api` must specify `cmd`"), "{err}");
    let err = parse_err("[services.api]\ncmd = \"   \"\n");
    assert!(err.contains("must specify `cmd`"), "{err}");
    // `up` basta (servicios container).
    let cfg = parse_ok("[services.db]\nup = \"compose up -d db\"\n");
    assert_eq!(cfg.services["db"].cmd, "compose up -d db");
}

#[test]
fn parse_cmd_takes_precedence_over_up_and_stop_cmd_over_down() {
    let cfg = parse_ok(
        "[services.x]\nup = \"UP\"\ncmd = \"CMD\"\ndown = \"DOWN\"\nstop_cmd = \"STOP\"\n",
    );
    assert_eq!(cfg.services["x"].cmd, "CMD");
    assert_eq!(cfg.services["x"].stop_cmd.as_deref(), Some("STOP"));
    let cfg = parse_ok("[services.x]\nup = \"UP\"\ndown = \"DOWN\"\n");
    assert_eq!(cfg.services["x"].stop_cmd.as_deref(), Some("DOWN"));
}

#[test]
fn parse_infers_container_kind_only_without_cmd() {
    let cases = [
        ("up = \"u\"", ServiceKind::Container),
        ("cmd = \"c\"\ndown = \"d\"", ServiceKind::Process),
        ("cmd = \"c\"\ncontainer = \"box\"", ServiceKind::Process),
        ("up = \"u\"\nkind = \"process\"", ServiceKind::Process),
        ("cmd = \"c\"\nkind = \"docker\"", ServiceKind::Container),
        ("cmd = \"c\"", ServiceKind::Process),
    ];
    for (body, expected) in cases {
        let cfg = parse_ok(&format!("[services.s]\n{body}\n"));
        assert_eq!(cfg.services["s"].kind, expected, "{body}");
    }
    let cfg = parse_ok("[services.s]\nup = \"u\"\nlogs = \"l\"\n");
    assert_eq!(cfg.services["s"].kind, ServiceKind::Container);
    assert_eq!(cfg.services["s"].logs_cmd.as_deref(), Some("l"));
}

#[test]
fn parse_rejects_invalid_kind() {
    let err = parse_err("[services.s]\ncmd = \"c\"\nkind = \"vm\"\n");
    assert!(err.contains("invalid kind `vm`"), "{err}");
    assert!(err.contains("line 3"), "{err}");
}

#[test]
fn parse_ports_must_fit_in_u16() {
    let cfg = parse_ok("[services.s]\ncmd = \"c\"\nready_port = 65535\ndebug_port = 0\n");
    assert_eq!(cfg.services["s"].ready_port, Some(65535));
    assert_eq!(cfg.services["s"].debug_port, Some(0));
    for (key, val) in [
        ("ready_port", "65536"),
        ("ready_port", "-1"),
        ("ready_port", "\"8080\""),
        ("ready_port", "80.5"),
        ("debug_port", "99999"),
        ("debug_port", "abc"),
    ] {
        let err = parse_err(&format!("[services.s]\ncmd = \"c\"\n{key} = {val}\n"));
        assert!(
            err.contains(&format!("`{key}` must be a u16")),
            "{key}={val}: {err}"
        );
        assert!(err.contains("line 3"), "{err}");
    }
}

#[test]
fn parse_booleans_are_strict() {
    let cfg = parse_ok("[services.s]\ncmd = \"c\"\noptional = true\nenabled = false\n");
    assert!(cfg.services["s"].optional);
    assert!(!cfg.services["s"].enabled);
    for val in ["yes", "1", "\"true\"", "True", "on"] {
        let err = parse_err(&format!("[services.s]\ncmd = \"c\"\nenabled = {val}\n"));
        assert!(err.contains("expected boolean"), "{val}: {err}");
    }
}

#[test]
fn parse_depends_on_multiline_array_with_comments() {
    let toml = r#"
[services.db]
cmd = "db"
[services.cache]
cmd = "cache"
[services.api]
cmd = "api"
depends_on = [
  "db",   # base de datos
  "cache",
]
"#;
    let cfg = parse_ok(toml);
    assert_eq!(cfg.services["api"].depends_on, vec!["db", "cache"]);
}

#[test]
fn parse_unterminated_depends_on_array_reports_origin_line() {
    let err = parse_err("[services.api]\ncmd = \"api\"\ndepends_on = [\"db\",\n\"x\"\n");
    assert!(err.contains("unterminated array"), "{err}");
    assert!(err.contains("line 3"), "{err}");
}

#[test]
fn parse_depends_on_must_be_string_array() {
    let err = parse_err("[services.api]\ncmd = \"api\"\ndepends_on = \"db\"\n");
    assert!(err.contains("depends_on"), "{err}");
}

#[test]
fn parse_runtime_container_cli_and_env_wrapper() {
    for (raw, cli) in [
        ("docker", ContainerCli::Docker),
        ("Podman", ContainerCli::Podman),
        ("nerdctl", ContainerCli::Nerdctl),
    ] {
        let cfg = parse_ok(&format!("[runtime]\ncontainer_cli = \"{raw}\"\n"));
        assert_eq!(cfg.container_cli, Some(cli));
    }
    // `[global]` es alias de `[runtime]`.
    let cfg = parse_ok("[global]\nenv_wrapper = \"call env.bat &&\"\ncontainer_cli = \"\"\n");
    assert_eq!(cfg.env_wrapper.as_deref(), Some("call env.bat &&"));
    assert_eq!(
        cfg.container_cli, None,
        "empty container_cli means auto-detect"
    );

    // `container_cli` nunca acepta rutas ni flags (el override solo existe desde Rust).
    let err = parse_err("[runtime]\ncontainer_cli = \"/tmp/evil-docker\"\n");
    assert!(err.contains("unsupported runtime"), "{err}");
    let err = parse_err("[runtime]\ncontainer_cli = \"docker --privileged\"\n");
    assert!(err.contains("unsupported runtime"), "{err}");
}

#[test]
fn parse_ignores_unknown_keys_and_unknown_sections() {
    let toml = r#"
top_level = "ignored"
[runtime]
unknown = "x"
[other]
cmd = "not a service"
[services.a]
cmd = "a"
future_field = "ok"
[[plugins]]
cmd = "ignored too"
"#;
    let cfg = parse_ok(toml);
    assert_eq!(cfg.services.keys().collect::<Vec<_>>(), vec!["a"]);
}

#[test]
fn parse_rejects_invalid_container_and_exec_user_at_load_time() {
    let err = parse_err("[services.db]\nup = \"u\"\ncontainer = \"--privileged\"\n");
    assert!(err.contains("service `db` invalid `container`"), "{err}");
    let err = parse_err("[services.db]\nup = \"u\"\ncontainer = \"db\"\nexec_user = \"root;id\"\n");
    assert!(err.contains("service `db` invalid `exec_user`"), "{err}");
    // Valores vacíos equivalen a ausencia.
    let cfg = parse_ok("[services.db]\nup = \"u\"\ncontainer = \"  \"\nexec_user = \"\"\n");
    assert_eq!(cfg.services["db"].container, None);
    assert_eq!(cfg.services["db"].exec_user, None);
    // Y los válidos se normalizan (trim).
    let cfg = parse_ok(
        "[services.db]\nup = \"u\"\ncontainer = \" app-db \"\nexec_user = \"1000:1000\"\n",
    );
    assert_eq!(cfg.services["db"].container.as_deref(), Some("app-db"));
    assert_eq!(cfg.services["db"].exec_user.as_deref(), Some("1000:1000"));
}

#[test]
fn parse_legacy_array_services_derive_ids() {
    let toml = r#"
[[service]]
name = "My API Server"
cmd = "a"

[[service]]
cmd = "b"

[[service]]
id = ""
cmd = "c"

[[services]]
id = "explicit"
name = "Whatever Name"
cmd = "d"
"#;
    let cfg = parse_ok(toml);
    let ids: Vec<&str> = cfg.services.keys().map(String::as_str).collect();
    // `name` normalizado; sin id ni name => `service-<índice 1-based>`; id vacío => fallback.
    assert_eq!(
        ids,
        vec!["explicit", "my-api-server", "service-2", "service-3"]
    );
    assert_eq!(cfg.services["my-api-server"].name, "My API Server");
    assert_eq!(cfg.services["explicit"].name, "Whatever Name");
}

#[test]
fn parse_rejects_duplicate_ids_across_array_services() {
    let err =
        parse_err("[[service]]\nid = \"db\"\ncmd = \"a\"\n[[service]]\nid = \"db\"\ncmd = \"b\"\n");
    assert!(err.contains("duplicate service id `db`"), "{err}");
    // Colisión entre `[services.db]` y `[[service]] id = "db"`.
    let err = parse_err("[services.db]\ncmd = \"a\"\n[[service]]\nid = \"db\"\ncmd = \"b\"\n");
    assert!(err.contains("duplicate service id `db`"), "{err}");
    // Colisión por nombre normalizado.
    let err = parse_err(
        "[[service]]\nname = \"Web App\"\ncmd = \"a\"\n[[service]]\nname = \"web app\"\ncmd = \"b\"\n",
    );
    assert!(err.contains("duplicate service id `web-app`"), "{err}");
}

#[test]
fn parse_cwd_without_policy_rejects_absolute_and_parent_segments() {
    let ws = TempWs::new("cwd_nopolicy");
    for bad in ["/etc", "../x", "a/../../x", "a/..", "a\\..\\..\\x"] {
        let err = parse(
            &format!("[services.s]\ncmd = \"c\"\ncwd = '{bad}'\n"),
            ws.path(),
        )
        .unwrap_err();
        assert!(err.contains("escapes workspace root"), "{bad}: {err}");
    }
    let cfg = parse("[services.s]\ncmd = \"c\"\ndir = \"svc/api\"\n", ws.path()).unwrap();
    assert_eq!(cfg.services["s"].cwd, ws.path().join("svc/api"));
    assert_eq!(cfg.services["s"].raw_cwd, "svc/api");
    let cfg = parse("[services.s]\ncmd = \"c\"\ncwd = \"\"\n", ws.path()).unwrap();
    assert_eq!(cfg.services["s"].cwd, ws.path());
}

#[test]
fn parse_cwd_with_policy_confines_to_workspace_including_symlinks() {
    let ws = TempWs::new("cwd_policy");
    let outside = TempWs::new("cwd_outside");
    fs::create_dir_all(ws.path().join("svc/api")).unwrap();
    let policy = WorkspacePolicy::new(ws.path()).unwrap();

    let ok = SproutConfig::parse_toml(
        "[services.s]\ncmd = \"c\"\ncwd = \"svc/api\"\n[services.t]\ncmd = \"c\"\ncwd = \"not/yet/created\"\n",
        ws.path(),
        Some(&policy),
    )
    .unwrap();
    assert_eq!(ok.services["s"].cwd, policy.root.join("svc/api"));
    assert_eq!(ok.services["t"].cwd, policy.root.join("not/yet/created"));

    let abs_outside = outside.path().to_string_lossy().to_string();
    for bad in ["../x", abs_outside.as_str()] {
        let err = SproutConfig::parse_toml(
            &format!("[services.s]\ncmd = \"c\"\ncwd = '{bad}'\n"),
            ws.path(),
            Some(&policy),
        )
        .unwrap_err();
        assert!(err.contains("service `s` invalid cwd"), "{bad}: {err}");
    }

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(outside.path(), ws.path().join("escape")).unwrap();
        let err = SproutConfig::parse_toml(
            "[services.s]\ncmd = \"c\"\ncwd = \"escape\"\n",
            ws.path(),
            Some(&policy),
        )
        .unwrap_err();
        assert!(
            err.contains("invalid cwd"),
            "symlink escape must be rejected: {err}"
        );
    }
}

#[test]
fn parse_runs_dag_validation() {
    let err = parse_err("[services.a]\ncmd = \"a\"\ndepends_on = [\"ghost\"]\n");
    assert!(err.contains("unknown service `ghost`"), "{err}");
    let err = parse_err("[services.a]\ncmd = \"a\"\ndepends_on = [\"a\"]\n");
    assert!(err.contains("cannot depend on itself"), "{err}");
}

#[test]
fn parse_is_independent_of_section_order() {
    let a =
        parse_ok("[services.b]\ncmd = \"b\"\ndepends_on = [\"a\"]\n[services.a]\ncmd = \"a\"\n");
    let b =
        parse_ok("[services.a]\ncmd = \"a\"\n[services.b]\ncmd = \"b\"\ndepends_on = [\"a\"]\n");
    assert_eq!(a.topo_order(), b.topo_order());
    assert_eq!(a.topo_order().unwrap(), vec!["a", "b"]);
}

#[test]
fn load_from_file_reads_and_reports_missing_file() {
    let ws = TempWs::new("load");
    let path = ws.path().join("sprout.toml");
    let err = SproutConfig::load_from_file(&path, ws.path(), None).unwrap_err();
    assert!(err.contains("failed to read"), "{err}");
    fs::write(&path, "[services.x]\ncmd = \"echo x\"\n").unwrap();
    let cfg = SproutConfig::load_from_file(&path, ws.path(), None).unwrap();
    assert_eq!(cfg.services["x"].cmd, "echo x");
    fs::write(&path, "[services.x]\nnope\n").unwrap();
    assert!(SproutConfig::load_from_file(&path, ws.path(), None).is_err());
}

// ---------------------------------------------------------------------------
// DAG: validate_dag / topo_order / transitive_start_order
// ---------------------------------------------------------------------------

#[test]
fn topo_order_puts_dependencies_first_and_breaks_ties_by_id() {
    // Diamante: api -> {db, cache} -> base ; `zeta` y `alpha` independientes.
    let cfg = config_of(&[
        ("zeta", &[]),
        ("api", &["db", "cache"]),
        ("db", &["base"]),
        ("cache", &["base"]),
        ("base", &[]),
        ("alpha", &[]),
    ]);
    let order = cfg.topo_order().unwrap();
    // Observado: Kahn con cola ordenada por id (BTreeSet), es decir, el orden
    // topológico lexicográficamente mínimo.
    assert_eq!(order, vec!["alpha", "base", "cache", "db", "api", "zeta"]);
}

#[test]
fn topo_order_ignores_duplicate_dependencies() {
    let cfg = config_of(&[("a", &[]), ("b", &["a", "a", "a"]), ("c", &["b", "a", "b"])]);
    assert_eq!(cfg.topo_order().unwrap(), vec!["a", "b", "c"]);
}

#[test]
fn validate_dag_reports_unknown_dependency_and_self_loop() {
    let err = config_of(&[("a", &["missing"])])
        .validate_dag()
        .unwrap_err();
    assert_eq!(err, "service `a` depends on unknown service `missing`");
    let err = config_of(&[("a", &["a"])]).validate_dag().unwrap_err();
    assert_eq!(err, "service `a` cannot depend on itself");
    // Las dependencias se comparan exactamente (sin trim ni mayúsculas).
    assert!(
        config_of(&[("a", &[]), ("b", &["A"])])
            .validate_dag()
            .is_err()
    );
    assert!(
        config_of(&[("a", &[]), ("b", &[" a"])])
            .validate_dag()
            .is_err()
    );
}

#[test]
fn validate_dag_detects_cycles_and_names_the_stuck_services() {
    let err = config_of(&[("a", &["b"]), ("b", &["a"]), ("free", &[])])
        .validate_dag()
        .unwrap_err();
    assert_eq!(
        err,
        "dependency cycle detected in sprout.toml involving: a, b"
    );

    let err = config_of(&[("x", &["z"]), ("y", &["x"]), ("z", &["y"])])
        .validate_dag()
        .unwrap_err();
    assert_eq!(
        err,
        "dependency cycle detected in sprout.toml involving: x, y, z"
    );
}

#[test]
fn transitive_start_order_includes_only_target_and_its_ancestors() {
    let cfg = config_of(&[
        ("base", &[]),
        ("db", &["base"]),
        ("cache", &[]),
        ("api", &["db"]),
        ("web", &["api", "cache"]),
        ("worker", &["db"]),
    ]);
    assert_eq!(
        cfg.transitive_start_order("api").unwrap(),
        vec!["base", "db", "api"]
    );
    assert_eq!(
        cfg.transitive_start_order("web").unwrap(),
        vec!["base", "cache", "db", "api", "web"]
    );
    assert_eq!(cfg.transitive_start_order("base").unwrap(), vec!["base"]);
    assert_eq!(cfg.transitive_start_order("cache").unwrap(), vec!["cache"]);
}

#[test]
fn transitive_start_order_rejects_unknown_target_and_cyclic_graphs() {
    let cfg = config_of(&[("a", &[])]);
    assert_eq!(
        cfg.transitive_start_order("b").unwrap_err(),
        "unknown service `b` in sprout.toml"
    );
    // `all` no es un id especial aquí (lo gestiona `SproutSupervisor::start`).
    assert!(cfg.transitive_start_order("all").is_err());
    // Un ciclo en otra parte del grafo invalida también este cálculo.
    let cyclic = config_of(&[("ok", &[]), ("p", &["q"]), ("q", &["p"])]);
    assert!(
        cyclic
            .transitive_start_order("ok")
            .unwrap_err()
            .contains("cycle")
    );
}

#[test]
fn has_container_services_and_effective_container_cli() {
    let mut cfg = config_of(&[("a", &[]), ("b", &[])]);
    assert!(!cfg.has_container_services());
    cfg.services.get_mut("b").unwrap().kind = ServiceKind::Container;
    assert!(cfg.has_container_services());

    // Sin `container_cli` explícito se delega en la detección del host.
    assert_eq!(
        cfg.effective_container_cli().as_deref(),
        detect_container_cli()
    );
    // Con `container_cli` explícito gana siempre, esté o no instalado.
    cfg.container_cli = Some(ContainerCli::Podman);
    assert_eq!(cfg.effective_container_cli().as_deref(), Some("podman"));
    cfg.container_cli = Some(ContainerCli::Nerdctl);
    assert_eq!(cfg.effective_container_cli().as_deref(), Some("nerdctl"));
}

// ---------------------------------------------------------------------------
// Propiedades sobre DAGs aleatorios (xorshift64, semillas fijas)
// ---------------------------------------------------------------------------

struct XorShift(u64);

impl XorShift {
    fn new(seed: u64) -> Self {
        Self(seed.max(1))
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn shuffle<T>(&mut self, v: &mut [T]) {
        for i in (1..v.len()).rev() {
            let j = self.below(i as u64 + 1) as usize;
            v.swap(i, j);
        }
    }
}

const SEEDS: u64 = 300;

type Graph = BTreeMap<String, Vec<String>>;

/// DAG aleatorio: el nodo de rango topológico `i` solo depende de rangos `< i`; los ids se
/// barajan para que el orden lexicográfico no coincida con el topológico. Incluye dependencias
/// duplicadas a propósito.
fn random_dag(rng: &mut XorShift) -> Graph {
    let n = 1 + rng.below(14) as usize;
    let mut labels: Vec<usize> = (0..n).collect();
    rng.shuffle(&mut labels);
    let ids: Vec<String> = labels.iter().map(|l| format!("s{l:02}")).collect();
    let mut graph = BTreeMap::new();
    for i in 0..n {
        let mut deps = Vec::new();
        for id in ids.iter().take(i) {
            if rng.below(100) < 30 {
                deps.push(id.clone());
                if rng.below(10) == 0 {
                    deps.push(id.clone());
                }
            }
        }
        rng.shuffle(&mut deps);
        graph.insert(ids[i].clone(), deps);
    }
    graph
}

fn config_from_graph(graph: &Graph) -> SproutConfig {
    let services = graph
        .iter()
        .map(|(id, deps)| {
            let deps: Vec<&str> = deps.iter().map(String::as_str).collect();
            (id.clone(), spec(id, &deps))
        })
        .collect();
    SproutConfig {
        container_cli: None,
        env_wrapper: None,
        services,
    }
}

fn ancestors_and_self(graph: &Graph, target: &str) -> BTreeSet<String> {
    let mut seen = BTreeSet::new();
    let mut stack = vec![target.to_string()];
    while let Some(cur) = stack.pop() {
        if seen.insert(cur.clone()) {
            stack.extend(graph[&cur].iter().cloned());
        }
    }
    seen
}

#[test]
fn property_topo_order_is_a_permutation_respecting_every_edge() {
    for seed in 1..=SEEDS {
        let mut rng = XorShift::new(seed);
        let graph = random_dag(&mut rng);
        let cfg = config_from_graph(&graph);
        let order = cfg
            .topo_order()
            .unwrap_or_else(|e| panic!("seed {seed}: acyclic graph rejected: {e}"));
        let as_set: BTreeSet<&String> = order.iter().collect();
        assert_eq!(order.len(), graph.len(), "seed {seed}: {order:?}");
        assert_eq!(
            as_set.len(),
            graph.len(),
            "seed {seed}: duplicates in {order:?}"
        );
        for (id, deps) in &graph {
            for dep in deps {
                assert!(
                    pos(&order, dep) < pos(&order, id),
                    "seed {seed}: `{dep}` must precede `{id}` in {order:?}"
                );
            }
        }
        assert!(cfg.validate_dag().is_ok());
    }
}

#[test]
fn property_topo_order_is_deterministic_and_independent_of_dependency_listing_order() {
    for seed in 1..=SEEDS {
        let mut rng = XorShift::new(seed);
        let graph = random_dag(&mut rng);
        let first = config_from_graph(&graph).topo_order().unwrap();
        assert_eq!(
            config_from_graph(&graph).topo_order().unwrap(),
            first,
            "seed {seed}"
        );

        // Reordenar y deduplicar `depends_on` no cambia el resultado.
        let mut permuted = graph.clone();
        for deps in permuted.values_mut() {
            rng.shuffle(deps);
        }
        assert_eq!(
            config_from_graph(&permuted).topo_order().unwrap(),
            first,
            "seed {seed}"
        );
        let mut dedup = graph.clone();
        for deps in dedup.values_mut() {
            deps.sort();
            deps.dedup();
        }
        assert_eq!(
            config_from_graph(&dedup).topo_order().unwrap(),
            first,
            "seed {seed}"
        );
    }
}

#[test]
fn property_topo_order_is_the_lexicographically_smallest_valid_order() {
    // Documenta lo observado: en cada paso se elige el id disponible más pequeño.
    for seed in 1..=SEEDS {
        let mut rng = XorShift::new(seed);
        let graph = random_dag(&mut rng);
        let order = config_from_graph(&graph).topo_order().unwrap();
        let mut placed = BTreeSet::new();
        for id in &order {
            let smallest_available = graph
                .iter()
                .filter(|(cand, deps)| {
                    !placed.contains(*cand) && deps.iter().all(|d| placed.contains(d))
                })
                .map(|(cand, _)| cand.clone())
                .next()
                .unwrap();
            assert_eq!(id, &smallest_available, "seed {seed}: {order:?}");
            placed.insert(id.clone());
        }
    }
}

#[test]
fn property_cycles_are_always_rejected_and_named() {
    for seed in 1..=SEEDS {
        let mut rng = XorShift::new(seed.wrapping_mul(7919));
        let mut graph = random_dag(&mut rng);
        if graph.len() < 2 {
            graph.insert("s98".into(), vec![]);
            graph.insert("s99".into(), vec![]);
        }
        // Anillo de longitud k >= 2 sobre nodos distintos elegidos al azar.
        let mut ids: Vec<String> = graph.keys().cloned().collect();
        rng.shuffle(&mut ids);
        let k = 2 + rng.below(ids.len() as u64 - 1) as usize;
        let ring = ids[..k].to_vec();
        for w in 0..k {
            let to = ring[(w + 1) % k].clone();
            graph.get_mut(&ring[w]).unwrap().push(to);
        }
        let cfg = config_from_graph(&graph);
        let err = cfg.topo_order().unwrap_err();
        assert!(err.contains("dependency cycle"), "seed {seed}: {err}");
        for id in &ring {
            assert!(
                err.contains(id.as_str()),
                "seed {seed}: `{id}` missing in {err}"
            );
        }
        assert!(cfg.validate_dag().is_err());
    }
}

#[test]
fn property_self_dependency_is_always_rejected() {
    for seed in 1..=SEEDS {
        let mut rng = XorShift::new(seed ^ 0xDEAD_BEEF);
        let mut graph = random_dag(&mut rng);
        let ids: Vec<String> = graph.keys().cloned().collect();
        let victim = ids[rng.below(ids.len() as u64) as usize].clone();
        graph.get_mut(&victim).unwrap().push(victim.clone());
        let err = config_from_graph(&graph).validate_dag().unwrap_err();
        assert!(
            err.contains("cannot depend on itself"),
            "seed {seed}: {err}"
        );
    }
}

#[test]
fn property_transitive_start_order_is_ancestor_closure_in_topo_order() {
    for seed in 1..=SEEDS {
        let mut rng = XorShift::new(seed + 10_000);
        let graph = random_dag(&mut rng);
        let cfg = config_from_graph(&graph);
        let full = cfg.topo_order().unwrap();
        for target in graph.keys() {
            let got = cfg.transitive_start_order(target).unwrap();
            let expected_set = ancestors_and_self(&graph, target);
            let expected: Vec<String> = full
                .iter()
                .filter(|id| expected_set.contains(*id))
                .cloned()
                .collect();
            assert_eq!(got, expected, "seed {seed}, target {target}");
            assert_eq!(got.last(), Some(target), "seed {seed}: target must be last");
        }
    }
}

// ---------------------------------------------------------------------------
// Formato TOON / panel / auditoría
// ---------------------------------------------------------------------------

#[test]
fn services_panel_parses_toon_rows_and_skips_noise() {
    let text = "started `api` (db: READY)\n\
                api | process | READY | 80 | 1 | -\n\
                @TOON v1\n\
                # cols: id | kind | status | port | pid | deps\n\
                db | container | READY | 5432 | - | -\n\
                api | process | FAILED(exit code 3) | - | - | db\n\
                \n\
                broken | row\n\
                # total: 2";
    assert_eq!(
        format_services_panel_from_toon(text),
        "[READY] db (container :5432)\n[FAILED] api (process)"
    );
    assert_eq!(format_services_panel_from_toon("no toon here"), "");
}

#[test]
fn audit_returns_none_without_sprout_toml() {
    let ws = TempWs::new("audit_none");
    assert!(audit_sprout_workspace(ws.path()).is_none());
}

#[test]
fn audit_reports_invalid_config_as_error() {
    let ws = TempWs::new("audit_bad");
    fs::write(
        ws.path().join("sprout.toml"),
        "[services.a]\ncmd = \"a\"\ndepends_on = [\"b\"]\n[services.b]\ncmd = \"b\"\ndepends_on = [\"a\"]\n",
    )
    .unwrap();
    let audit = audit_sprout_workspace(ws.path()).unwrap();
    assert!(audit.checks.is_empty(), "{audit:?}");
    assert_eq!(audit.errors.len(), 1, "{audit:?}");
    assert!(audit.errors[0].contains("dependency cycle"), "{audit:?}");
}

#[test]
fn audit_validates_and_warns_about_busy_ready_ports() {
    let ws = TempWs::new("audit_port");
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let busy = listener.local_addr().unwrap().port();
    fs::write(
        ws.path().join("sprout.toml"),
        format!(
            "[services.api]\ncmd = \"a\"\nready_port = {busy}\n[services.worker]\ncmd = \"w\"\n"
        ),
    )
    .unwrap();
    let audit = audit_sprout_workspace(ws.path()).unwrap();
    assert!(audit.errors.is_empty(), "{audit:?}");
    assert_eq!(audit.checks.len(), 1);
    assert!(audit.checks[0].contains("2 service(s)"), "{audit:?}");
    assert_eq!(audit.warnings.len(), 1, "{audit:?}");
    assert!(
        audit.warnings[0].contains(&format!("port {busy}")) && audit.warnings[0].contains("`api`"),
        "{audit:?}"
    );
    drop(listener);
}

// ---------------------------------------------------------------------------
// SproutSupervisor (procesos reales triviales + CLI de contenedores falsa)
// ---------------------------------------------------------------------------

#[cfg(unix)]
mod supervisor {
    use super::*;
    use std::time::{Duration, Instant};

    use onyx_workspace::{
        NetworkPolicy, SandboxMode, SproutSupervisor, WorkspaceTrustReport, WorkspaceTrustStatus,
    };

    const WAIT: Option<u64> = Some(15);

    fn write_toml(ws: &TempWs, toml: &str) {
        fs::write(ws.path().join("sprout.toml"), toml).unwrap();
    }

    fn sup(ws: &TempWs, toml: &str) -> SproutSupervisor {
        write_toml(ws, toml);
        SproutSupervisor::new(WorkspacePolicy::new(ws.path()).unwrap())
    }

    fn sup_cli(ws: &TempWs, toml: &str) -> SproutSupervisor {
        let cli = fake_cli(ws);
        sup(ws, toml).with_container_cli_override(cli.display().to_string())
    }

    /// CLI de contenedores falsa: registra cada invocación en `cli-calls.log` como
    /// `CALL <arg1> <arg2> ...` y, para `exec`, imprime `ARGS <...>` en stdout.
    /// - `info` falla si existe `daemon-down`.
    /// - `exec` con `SLEEP_FOREVER` duerme 30 s; con `BIG_OUTPUT` imprime ~16 KiB; con
    ///   `EXIT7` sale con código 7; con `LEAK` imprime un secreto.
    fn fake_cli(ws: &TempWs) -> PathBuf {
        let root = ws.path().display().to_string();
        let script = format!(
            r#"#!/bin/sh
LOG='{root}/cli-calls.log'
printf 'CALL' >> "$LOG"
for a in "$@"; do printf ' <%s>' "$a" >> "$LOG"; done
printf '\n' >> "$LOG"
case "$1" in
  info)
    if [ -f '{root}/daemon-down' ]; then echo 'Cannot connect to the Docker daemon' >&2; exit 1; fi
    exit 0 ;;
  exec)
    shift
    printf 'ARGS'
    for a in "$@"; do printf ' <%s>' "$a"; done
    printf '\n'
    case "$*" in
      *SLEEP_FOREVER*) sleep 30 ;;
      *BIG_OUTPUT*) i=0; while [ $i -lt 400 ]; do echo 0123456789012345678901234567890123456789; i=$((i+1)); done ;;
      *EXIT7*) echo 'to stderr' >&2; exit 7 ;;
      *LEAK*) echo 'Authorization: Bearer sk-live-abcdefghijklmnopqrstuvwxyz123456' ;;
    esac
    exit 0 ;;
esac
exit 0
"#
        );
        let path = ws.path().join("fake-docker.sh");
        fs::write(&path, script).unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn cli_calls(ws: &TempWs) -> Vec<String> {
        fs::read_to_string(ws.path().join("cli-calls.log"))
            .map(|s| s.lines().map(str::to_string).collect())
            .unwrap_or_default()
    }

    fn container_toml(extra: &str) -> String {
        format!(
            "[services.db]\nkind = \"container\"\nup = \"echo up-ran >> up.txt\"\ncontainer = \"app-db\"\n{extra}\n"
        )
    }

    fn read(ws: &TempWs, file: &str) -> String {
        fs::read_to_string(ws.path().join(file)).unwrap_or_default()
    }

    fn wait_until(timeout: Duration, mut cond: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if cond() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(30));
        }
        cond()
    }

    fn logs_eventually_contain(s: &SproutSupervisor, id: &str, needle: &str) -> bool {
        wait_until(Duration::from_secs(5), || {
            s.logs(id, Some(500), None, false)
                .map(|l| l.contains(needle))
                .unwrap_or(false)
        })
    }

    /// Fila TOON de `id` -> columnas.
    fn row(toon: &str, id: &str) -> Vec<String> {
        toon.lines()
            .map(|l| {
                l.split('|')
                    .map(|c| c.trim().to_string())
                    .collect::<Vec<_>>()
            })
            .find(|cols| cols.len() == 6 && cols[0] == id)
            .unwrap_or_else(|| panic!("no row for `{id}` in:\n{toon}"))
    }

    fn status_of(s: &SproutSupervisor, id: &str) -> String {
        row(&s.status_toon().unwrap(), id)[2].clone()
    }

    // --- carga y estado -----------------------------------------------------

    #[test]
    fn status_requires_sprout_toml_in_workspace() {
        let ws = TempWs::new("sup_missing");
        let s = SproutSupervisor::new(WorkspacePolicy::new(ws.path()).unwrap());
        let err = s.status_toon().unwrap_err();
        assert!(err.contains("no `sprout.toml` found"), "{err}");
        assert!(s.render_panel_text().is_none());
        assert!(s.start("x", WAIT).is_err());
    }

    #[test]
    fn invalid_sprout_toml_surfaces_parse_error_on_every_entry_point() {
        let ws = TempWs::new("sup_invalid");
        let s = sup(&ws, "[services.a]\ncmd = \"a\"\ndepends_on = [\"zz\"]\n");
        for err in [
            s.status_toon().unwrap_err(),
            s.start("a", WAIT).unwrap_err(),
            s.stop(None, true).unwrap_err(),
            s.logs("a", None, None, false).unwrap_err(),
            s.exec_in_container("a", "id", None).unwrap_err(),
        ] {
            assert!(err.contains("unknown service `zz`"), "{err}");
        }
    }

    #[test]
    fn status_toon_lists_services_in_topo_order_with_columns() {
        let ws = TempWs::new("sup_status");
        let s = sup(
            &ws,
            "[services.web]\ncmd = \"w\"\ndepends_on = [\"db\", \"cache\"]\nready_port = 8080\n\
             [services.db]\nkind = \"container\"\nup = \"u\"\n\
             [services.cache]\ncmd = \"c\"\n",
        );
        let toon = s.status_toon().unwrap();
        let lines: Vec<&str> = toon.lines().collect();
        assert_eq!(
            lines,
            vec![
                "@TOON v1",
                "# cols: id | kind | status | port | pid | deps",
                "cache | process | OFF | - | - | -",
                "db | container | OFF | - | - | -",
                "web | process | OFF | 8080 | - | db,cache",
                "# total: 3",
            ]
        );
        assert_eq!(
            s.render_panel_text().unwrap(),
            "[OFF] cache (process)\n[OFF] db (container)\n[OFF] web (process :8080)"
        );
    }

    #[test]
    fn empty_config_has_no_panel_and_nothing_to_start() {
        let ws = TempWs::new("sup_empty");
        let s = sup(&ws, "[runtime]\n");
        assert!(s.render_panel_text().is_none());
        let out = s.start("all", WAIT).unwrap();
        assert!(out.starts_with("no enabled services to start"), "{out}");
    }

    #[test]
    fn from_config_preloads_services_without_reading_sprout_toml() {
        let ws = TempWs::new("sup_from_config");
        let policy = WorkspacePolicy::new(ws.path()).unwrap();
        let s = SproutSupervisor::from_config(policy, config_of(&[("x", &[]), ("y", &["x"])]));
        assert!(!ws.path().join("sprout.toml").exists());
        let toon = s.status_toon().unwrap();
        assert!(toon.contains("x | process | OFF | - | - | -"), "{toon}");
        assert!(toon.contains("y | process | OFF | - | - | x"), "{toon}");
    }

    // --- confianza ----------------------------------------------------------

    #[test]
    fn untrusted_policy_blocks_start_stop_exec_but_allows_read_only_views() {
        let ws = TempWs::new("sup_untrusted");
        let cli = fake_cli(&ws);
        write_toml(
            &ws,
            &(container_toml("") + "[services.p]\ncmd = \"echo ran > ran.txt\"\n"),
        );
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
        let s =
            SproutSupervisor::new(policy).with_container_cli_override(cli.display().to_string());

        for err in [
            s.start("p", WAIT).unwrap_err(),
            s.start("all", WAIT).unwrap_err(),
            s.start_all(WAIT).unwrap_err(),
            s.stop(Some("p"), false).unwrap_err(),
            s.stop(None, true).unwrap_err(),
            s.exec_in_container("db", "id", None).unwrap_err(),
        ] {
            assert!(err.contains("blocked by Workspace Trust"), "{err}");
            assert!(err.contains("untrusted workspace"), "{err}");
        }
        assert!(cli_calls(&ws).is_empty(), "no CLI call while untrusted");
        assert!(
            !ws.path().join("ran.txt").exists(),
            "no process while untrusted"
        );
        assert!(!ws.path().join("up.txt").exists());

        let toon = s.status_toon().unwrap();
        assert!(toon.starts_with("# UNTRUSTED sprout.toml"), "{toon}");
        assert!(toon.contains("p | process | OFF"), "{toon}");
        let panel = s.render_panel_text().unwrap();
        assert!(panel.starts_with("[UNTRUSTED — run /trust]\n"), "{panel}");
        assert!(s.logs("p", None, None, false).unwrap().contains("no logs"));
    }

    #[test]
    fn set_trust_state_toggles_execution_and_explains_modified_sprout() {
        let ws = TempWs::new("sup_trust_toggle");
        let s = sup(&ws, "[services.p]\ncmd = \"echo ran >> ran.txt\"\n");
        s.set_trust_state(
            false,
            WorkspaceTrustStatus::Modified {
                config_changed: false,
                sprout_changed: true,
            },
        );
        let err = s.start("p", WAIT).unwrap_err();
        assert!(err.contains("changed since last approval"), "{err}");
        assert!(!ws.path().join("ran.txt").exists());

        // Modificado solo `config.toml`: mensaje genérico.
        s.set_trust_state(
            false,
            WorkspaceTrustStatus::Modified {
                config_changed: true,
                sprout_changed: false,
            },
        );
        let err = s.start("p", WAIT).unwrap_err();
        assert!(err.contains("untrusted workspace"), "{err}");

        s.set_trust_state(true, WorkspaceTrustStatus::Trusted);
        let out = s.start("p", WAIT).unwrap();
        assert!(out.contains("p: READY"), "{out}");
        assert_eq!(read(&ws, "ran.txt"), "ran\n");
        assert!(!s.status_toon().unwrap().contains("UNTRUSTED"));
    }

    // --- start / stop de procesos ------------------------------------------

    #[test]
    fn start_runs_dependencies_first_and_leaves_unrelated_services_off() {
        let ws = TempWs::new("sup_start_deps");
        let s = sup(
            &ws,
            "[services.web]\ncmd = \"echo web >> order.txt\"\ndepends_on = [\"api\"]\n\
             [services.api]\ncmd = \"echo api >> order.txt\"\ndepends_on = [\"db\"]\n\
             [services.db]\ncmd = \"echo db >> order.txt\"\n\
             [services.other]\ncmd = \"echo other >> order.txt\"\n",
        );
        let out = s.start("web", WAIT).unwrap();
        assert!(
            out.starts_with("started `web` (db: READY -> api: READY -> web: READY)"),
            "{out}"
        );
        assert_eq!(read(&ws, "order.txt"), "db\napi\nweb\n");
        assert_eq!(status_of(&s, "other"), "OFF");
        assert_eq!(status_of(&s, "web"), "READY");

        // Segundo arranque: no relanza nada y solo informa del foco.
        let again = s.start("web", WAIT).unwrap();
        assert!(
            again.starts_with("started `web` (web: already READY)"),
            "{again}"
        );
        assert_eq!(read(&ws, "order.txt"), "db\napi\nweb\n");
    }

    #[test]
    fn start_unknown_service_errors_without_side_effects() {
        let ws = TempWs::new("sup_start_unknown");
        let s = sup(&ws, "[services.a]\ncmd = \"echo a > a.txt\"\n");
        let err = s.start("ghost", WAIT).unwrap_err();
        assert!(err.contains("unknown service `ghost`"), "{err}");
        assert!(!ws.path().join("a.txt").exists());
    }

    #[test]
    fn start_all_respects_enabled_flag_and_topo_order() {
        let ws = TempWs::new("sup_start_all");
        let s = sup(
            &ws,
            "[services.b]\ncmd = \"echo b >> order.txt\"\ndepends_on = [\"a\"]\n\
             [services.a]\ncmd = \"echo a >> order.txt\"\n\
             [services.off]\ncmd = \"echo off >> order.txt\"\nenabled = false\n",
        );
        // `all` es case-insensitive y admite espacios.
        let out = s.start("  ALL ", WAIT).unwrap();
        assert!(
            out.starts_with("started `all` (a: READY -> b: READY)"),
            "{out}"
        );
        assert_eq!(read(&ws, "order.txt"), "a\nb\n");
        assert_eq!(status_of(&s, "off"), "OFF");

        let again = s.start_all(WAIT).unwrap();
        assert!(
            again.starts_with("started `all` (a: already READY -> b: already READY)"),
            "{again}"
        );
    }

    #[test]
    fn start_all_with_only_disabled_services_reports_nothing_to_start() {
        let ws = TempWs::new("sup_all_disabled");
        let s = sup(
            &ws,
            "[services.a]\ncmd = \"echo a > a.txt\"\nenabled = false\n",
        );
        let out = s.start_all(WAIT).unwrap();
        assert!(out.starts_with("no enabled services to start"), "{out}");
        assert!(!ws.path().join("a.txt").exists());
        // Pero se puede arrancar explícitamente por id.
        assert!(s.start("a", WAIT).unwrap().contains("a: READY"));
    }

    #[test]
    fn failing_dependency_aborts_the_plan_and_reports_exit_code() {
        let ws = TempWs::new("sup_fail");
        let s = sup(
            &ws,
            "[services.db]\ncmd = \"echo boom-message; exit 3\"\n\
             [services.api]\ncmd = \"echo api > api.txt\"\ndepends_on = [\"db\"]\n",
        );
        let err = s.start("api", WAIT).unwrap_err();
        assert!(
            err.contains("service `db` failed to start (exit code 3)"),
            "{err}"
        );
        assert!(
            !ws.path().join("api.txt").exists(),
            "dependent must not start"
        );
        assert_eq!(status_of(&s, "db"), "FAILED(exit code 3)");
        assert_eq!(status_of(&s, "api"), "OFF");
        assert!(logs_eventually_contain(&s, "db", "boom-message"));
        assert!(logs_eventually_contain(
            &s,
            "db",
            "[ERROR] Process exited unexpectedly with code 3"
        ));
    }

    #[test]
    fn ready_log_probe_marks_ready_and_stop_runs_stop_cmd_and_kills_process() {
        let ws = TempWs::new("sup_ready_log");
        let s = sup(
            &ws,
            "[services.srv]\ncmd = \"echo booting; echo 'Started Demo'; exec sleep 30\"\n\
             ready_log = \"Started Demo\"\nstop_cmd = \"echo stopped >> stopped.txt\"\n",
        );
        let out = s.start("srv", WAIT).unwrap();
        assert!(out.contains("srv: READY"), "{out}");
        let toon = s.status_toon().unwrap();
        let cols = row(&toon, "srv");
        assert_eq!(cols[2], "READY");
        let pid: u32 = cols[4]
            .parse()
            .unwrap_or_else(|_| panic!("pid column: {toon}"));

        let stopped = s.stop(Some("srv"), false).unwrap();
        assert!(stopped.starts_with("stopped: srv"), "{stopped}");
        assert_eq!(status_of(&s, "srv"), "OFF");
        assert_eq!(read(&ws, "stopped.txt"), "stopped\n");
        #[cfg(target_os = "linux")]
        assert!(
            wait_until(Duration::from_secs(5), || !Path::new(&format!(
                "/proc/{pid}"
            ))
            .exists()),
            "process {pid} must be terminated after stop"
        );
        let _ = pid;

        // Parar algo ya parado no ejecuta `stop_cmd` otra vez.
        let again = s.stop(Some("srv"), false).unwrap();
        assert!(again.starts_with("no running services to stop"), "{again}");
        assert_eq!(read(&ws, "stopped.txt"), "stopped\n");
    }

    #[test]
    fn ready_log_matches_on_stderr_too() {
        let ws = TempWs::new("sup_stderr");
        let s = sup(
            &ws,
            "[services.srv]\ncmd = \"echo LISTENING >&2; exec sleep 30\"\nready_log = \"LISTENING\"\n",
        );
        assert!(s.start("srv", WAIT).unwrap().contains("srv: READY"));
        assert!(logs_eventually_contain(&s, "srv", "[ERR] LISTENING"));
        s.stop(None, true).unwrap();
    }

    #[test]
    fn ready_port_probe_marks_ready_when_port_opens() {
        let ws = TempWs::new("sup_port");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let s = sup(
            &ws,
            &format!("[services.srv]\ncmd = \"exec sleep 30\"\nready_port = {port}\n"),
        );
        let out = s.start("srv", WAIT).unwrap();
        assert!(out.contains("srv: READY"), "{out}");
        assert!(logs_eventually_contain(
            &s,
            "srv",
            &format!("Port {port} is open")
        ));
        s.stop(None, true).unwrap();
        drop(listener);
    }

    #[test]
    fn ready_cmd_probe_marks_ready_when_check_succeeds() {
        let ws = TempWs::new("sup_ready_cmd");
        let s = sup(
            &ws,
            "[services.srv]\ncmd = \"touch flag; exec sleep 30\"\nready_cmd = \"test -f flag\"\n",
        );
        let out = s.start("srv", WAIT).unwrap();
        assert!(out.contains("srv: READY"), "{out}");
        assert!(logs_eventually_contain(
            &s,
            "srv",
            "Check 'test -f flag' succeeded"
        ));
        s.stop(None, true).unwrap();
    }

    #[test]
    fn all_probes_must_be_satisfied_not_just_one() {
        let ws = TempWs::new("sup_all_probes");
        // El log aparece, pero `ready_cmd` nunca tiene éxito: no puede quedar READY.
        let s = sup(
            &ws,
            "[services.srv]\ncmd = \"echo UP; exec sleep 30\"\nready_log = \"UP\"\nready_cmd = \"false\"\n",
        );
        let err = s.start("srv", Some(2)).unwrap_err();
        assert!(err.contains("did not reach READY within 2s"), "{err}");
        assert!(err.contains("current: STARTING"), "{err}");
        s.stop(None, true).unwrap();
    }

    #[test]
    fn wait_ready_secs_zero_is_clamped_to_one_second() {
        let ws = TempWs::new("sup_clamp");
        let s = sup(
            &ws,
            "[services.srv]\ncmd = \"exec sleep 30\"\nready_log = \"never-printed\"\n",
        );
        let t0 = Instant::now();
        let err = s.start("srv", Some(0)).unwrap_err();
        let elapsed = t0.elapsed();
        assert!(err.contains("did not reach READY within 1s"), "{err}");
        assert!(
            elapsed >= Duration::from_millis(900),
            "waited only {elapsed:?}"
        );
        s.stop(None, true).unwrap();
    }

    #[test]
    fn stop_all_runs_stop_cmds_in_reverse_topological_order() {
        let ws = TempWs::new("sup_stop_order");
        let s = sup(
            &ws,
            "[services.db]\ncmd = \"echo UP; exec sleep 30\"\nready_log = \"UP\"\nstop_cmd = \"echo db >> stopped.txt\"\n\
             [services.api]\ncmd = \"echo UP; exec sleep 30\"\nready_log = \"UP\"\nstop_cmd = \"echo api >> stopped.txt\"\ndepends_on = [\"db\"]\n\
             [services.web]\ncmd = \"echo UP; exec sleep 30\"\nready_log = \"UP\"\nstop_cmd = \"echo web >> stopped.txt\"\ndepends_on = [\"api\"]\n\
             [services.idle]\ncmd = \"echo idle\"\nstop_cmd = \"echo idle >> stopped.txt\"\n",
        );
        s.start("web", WAIT).unwrap();
        // `stop(None, false)` equivale a parar todo.
        let out = s.stop(None, false).unwrap();
        assert!(out.starts_with("stopped: web, api, db\n"), "{out}");
        assert_eq!(
            read(&ws, "stopped.txt"),
            "web\napi\ndb\n",
            "idle never ran, so its stop_cmd must not run"
        );
    }

    #[test]
    fn stop_unknown_service_is_an_error() {
        let ws = TempWs::new("sup_stop_unknown");
        let s = sup(&ws, "[services.a]\ncmd = \"echo a\"\n");
        let err = s.stop(Some("nope"), false).unwrap_err();
        assert!(err.contains("unknown service `nope`"), "{err}");
        // `all = true` ignora el id.
        assert!(s.stop(Some("nope"), true).is_ok());
    }

    #[test]
    fn env_wrapper_and_debug_port_apply_only_to_process_services() {
        let ws = TempWs::new("sup_wrapper");
        let s = sup_cli(
            &ws,
            "[runtime]\nenv_wrapper = \"echo WRAPPED\"\n\
             [services.p]\ncmd = \"payload\"\ndebug_port = 5005\n\
             [services.c]\nkind = \"container\"\nup = \"echo container-up\"\ncontainer = \"app-c\"\ndebug_port = 6006\n",
        );
        s.start("all", WAIT).unwrap();
        assert!(logs_eventually_contain(&s, "p", "WRAPPED payload"));
        assert!(logs_eventually_contain(&s, "p", "address=*:5005"));
        assert!(logs_eventually_contain(&s, "c", "container-up"));
        let c_logs = s.logs("c", None, None, false).unwrap();
        assert!(!c_logs.contains("WRAPPED"), "{c_logs}");
        assert!(!c_logs.contains("6006"), "{c_logs}");
    }

    #[test]
    fn service_processes_do_not_inherit_secret_env_vars() {
        let ws = TempWs::new("sup_env");
        // SAFETY: variable con nombre único usada solo por este test.
        unsafe {
            std::env::set_var("FORJA_SPROUT_TEST_API_KEY", "leak-me-please-42");
        }
        let s = sup(
            &ws,
            "[services.p]\ncmd = 'echo \"key=[$FORJA_SPROUT_TEST_API_KEY]\"'\n",
        );
        s.start("p", WAIT).unwrap();
        assert!(logs_eventually_contain(&s, "p", "key=["));
        let logs = s.logs("p", None, Some("key="), false).unwrap();
        assert_eq!(logs, "key=[]");
    }

    // --- logs ---------------------------------------------------------------

    fn sup_with_logs(ws: &TempWs) -> SproutSupervisor {
        let s = sup(
            ws,
            "[services.p]\ncmd = \"for i in 1 2 3 4 5; do echo line-$i; done; echo abc; echo a.c; echo 'Authorization: Bearer sk-live-abcdefghijklmnopqrstuvwxyz123456'\"\n",
        );
        s.start("p", WAIT).unwrap();
        assert!(logs_eventually_contain(&s, "p", "Authorization"));
        s
    }

    #[test]
    fn logs_tail_is_clamped_and_returns_the_last_lines() {
        let ws = TempWs::new("sup_logs_tail");
        let s = sup_with_logs(&ws);
        assert_eq!(
            s.logs("p", Some(2), Some("line-"), false).unwrap(),
            "line-4\nline-5"
        );
        // `tail = 0` se acota a 1.
        assert_eq!(
            s.logs("p", Some(0), Some("line-"), false).unwrap(),
            "line-5"
        );
        assert_eq!(
            s.logs("p", Some(10_000), Some("line-"), false).unwrap(),
            "line-1\nline-2\nline-3\nline-4\nline-5"
        );
    }

    #[test]
    fn logs_filter_is_literal_unless_regex_requested() {
        let ws = TempWs::new("sup_logs_filter");
        let s = sup_with_logs(&ws);
        assert_eq!(s.logs("p", None, Some("a.c"), false).unwrap(), "a.c");
        assert_eq!(s.logs("p", None, Some("^a.c$"), true).unwrap(), "abc\na.c");
        // Sensible a mayúsculas.
        assert_eq!(
            s.logs("p", None, Some("LINE-"), false).unwrap(),
            "(no logs for `p`, status=READY)"
        );
        // Patrón vacío o de espacios = sin filtro.
        assert!(
            s.logs("p", None, Some("   "), false)
                .unwrap()
                .contains("line-1")
        );
        let err = s.logs("p", None, Some("(unclosed"), true).unwrap_err();
        assert!(err.contains("invalid regex pattern"), "{err}");
        // Como literal, el mismo texto es válido.
        assert!(s.logs("p", None, Some("(unclosed"), false).is_ok());
    }

    #[test]
    fn logs_scrub_secrets_and_reject_unknown_service() {
        let ws = TempWs::new("sup_logs_scrub");
        let s = sup_with_logs(&ws);
        let logs = s.logs("p", None, None, false).unwrap();
        assert!(
            !logs.contains("sk-live-abcdefghijklmnopqrstuvwxyz123456"),
            "{logs}"
        );
        assert!(logs.contains("[REDACTED]"), "{logs}");
        let err = s.logs("ghost", None, None, false).unwrap_err();
        assert!(err.contains("unknown service `ghost`"), "{err}");
    }

    #[test]
    fn logs_are_truncated_to_policy_max_output_bytes() {
        let ws = TempWs::new("sup_logs_trunc");
        write_toml(
            &ws,
            "[services.p]\ncmd = \"i=0; while [ $i -lt 50 ]; do echo 0123456789abcdef; i=$((i+1)); done; echo END\"\n",
        );
        let mut policy = WorkspacePolicy::new(ws.path()).unwrap();
        policy.max_output_bytes = 100;
        let s = SproutSupervisor::new(policy);
        s.start("p", WAIT).unwrap();
        assert!(wait_until(Duration::from_secs(5), || {
            s.logs("p", Some(1), Some("END"), false).unwrap() == "END"
        }));
        let out = s.logs("p", Some(500), Some("0123"), false).unwrap();
        assert!(out.ends_with("\n...[truncated]"), "{out}");
        assert_eq!(out.len(), 100 + "\n...[truncated]".len(), "{out}");
    }

    // --- contenedores con CLI falsa ----------------------------------------

    #[test]
    fn container_start_checks_daemon_with_info_then_runs_up_and_down() {
        let ws = TempWs::new("sup_ctr_start");
        let s = sup_cli(&ws, &container_toml("down = \"echo down >> down.txt\""));
        let out = s.start("db", WAIT).unwrap();
        assert!(out.contains("db: READY"), "{out}");
        assert_eq!(cli_calls(&ws), vec!["CALL <info>"]);
        assert_eq!(read(&ws, "up.txt"), "up-ran\n");
        s.stop(Some("db"), false).unwrap();
        assert_eq!(read(&ws, "down.txt"), "down\n");
    }

    #[test]
    fn container_start_fails_fast_when_daemon_is_down() {
        let ws = TempWs::new("sup_ctr_down");
        fs::write(ws.path().join("daemon-down"), "").unwrap();
        let s = sup_cli(&ws, &container_toml(""));
        let err = s.start("db", WAIT).unwrap_err();
        assert!(err.contains("info` failed (exit 1)"), "{err}");
        assert!(err.contains("Cannot connect to the Docker daemon"), "{err}");
        assert!(!ws.path().join("up.txt").exists(), "`up` must not run");
        assert_eq!(status_of(&s, "db"), "OFF");
    }

    #[test]
    fn exec_passes_command_as_single_sh_c_argument() {
        let ws = TempWs::new("sup_exec_args");
        let s = sup_cli(&ws, &container_toml(""));
        let cmd = "psql -c 'select 1'; echo $(id) `x` && ls | wc";
        let out = s.exec_in_container("db", cmd, None).unwrap();
        assert!(
            out.starts_with("service: db (container=app-db)\nexit_code: 0\nstdout:\n"),
            "{out}"
        );
        assert!(
            out.contains(&format!("ARGS <app-db> <sh> <-c> <{cmd}>")),
            "{out}"
        );
        assert_eq!(
            cli_calls(&ws),
            vec![format!("CALL <exec> <app-db> <sh> <-c> <{cmd}>")]
        );
    }

    #[test]
    fn exec_adds_user_flag_only_when_exec_user_is_set() {
        let ws = TempWs::new("sup_exec_user");
        let s = sup_cli(&ws, &container_toml("exec_user = \"1000:1000\""));
        s.exec_in_container("db", "whoami", None).unwrap();
        assert_eq!(
            cli_calls(&ws),
            vec!["CALL <exec> <--user> <1000:1000> <app-db> <sh> <-c> <whoami>"]
        );
    }

    #[test]
    fn exec_reports_exit_code_and_stderr() {
        let ws = TempWs::new("sup_exec_exit");
        let s = sup_cli(&ws, &container_toml(""));
        let out = s.exec_in_container("db", "EXIT7", None).unwrap();
        assert!(out.contains("\nexit_code: 7\n"), "{out}");
        assert!(out.ends_with("stderr:\nto stderr\n"), "{out}");
    }

    #[test]
    fn exec_scrubs_secrets_and_truncates_output() {
        let ws = TempWs::new("sup_exec_scrub");
        let cli = fake_cli(&ws);
        write_toml(&ws, &container_toml(""));
        let mut policy = WorkspacePolicy::new(ws.path()).unwrap();
        policy.max_output_bytes = 300;
        let s =
            SproutSupervisor::new(policy).with_container_cli_override(cli.display().to_string());
        let out = s.exec_in_container("db", "LEAK", None).unwrap();
        assert!(
            !out.contains("sk-live-abcdefghijklmnopqrstuvwxyz123456"),
            "{out}"
        );
        assert!(out.contains("[REDACTED]"), "{out}");
        let big = s.exec_in_container("db", "BIG_OUTPUT", None).unwrap();
        assert!(big.ends_with("\n...[truncated]"), "{big}");
        assert_eq!(big.len(), 300 + "\n...[truncated]".len());
    }

    #[test]
    fn exec_times_out_and_timeout_is_clamped_to_at_least_one_second() {
        let ws = TempWs::new("sup_exec_timeout");
        let s = sup_cli(&ws, &container_toml(""));
        let t0 = Instant::now();
        let err = s
            .exec_in_container("db", "SLEEP_FOREVER", Some(1))
            .unwrap_err();
        assert!(err.contains("service_exec(db) timed out after 1s"), "{err}");
        assert!(t0.elapsed() < Duration::from_secs(10), "{:?}", t0.elapsed());
        let t0 = Instant::now();
        let err = s
            .exec_in_container("db", "SLEEP_FOREVER", Some(0))
            .unwrap_err();
        assert!(err.contains("timed out after 1s"), "{err}");
        assert!(
            t0.elapsed() >= Duration::from_millis(900),
            "{:?}",
            t0.elapsed()
        );
    }

    #[test]
    fn exec_rejects_bad_targets_before_invoking_the_cli() {
        let ws = TempWs::new("sup_exec_bad");
        let s = sup_cli(
            &ws,
            "[services.proc]\ncmd = \"echo p\"\n\
             [services.nobox]\nkind = \"container\"\nup = \"echo u\"\n",
        );
        let err = s.exec_in_container("proc", "id", None).unwrap_err();
        assert!(err.contains("has kind `process`"), "{err}");
        let err = s.exec_in_container("nobox", "id", None).unwrap_err();
        assert!(err.contains("does not declare `container"), "{err}");
        let err = s.exec_in_container("ghost", "id", None).unwrap_err();
        assert!(err.contains("unknown service `ghost`"), "{err}");
        for empty in ["", "   ", "\n\t"] {
            let err = s.exec_in_container("nobox", empty, None).unwrap_err();
            assert_eq!(err, "command cannot be empty");
        }
        assert!(cli_calls(&ws).is_empty());
    }

    #[test]
    fn exec_revalidates_container_and_user_even_if_config_was_built_by_hand() {
        let ws = TempWs::new("sup_exec_reval");
        let cli = fake_cli(&ws);
        let policy = WorkspacePolicy::new(ws.path()).unwrap();
        let base = SproutConfig::parse_toml(&container_toml(""), ws.path(), Some(&policy)).unwrap();

        let hostile: [(Option<&str>, Option<&str>); 4] = [
            (Some("--privileged"), None),
            (Some("app-db; rm -rf /"), None),
            (Some("app-db"), Some("root --privileged")),
            (Some("app-db"), Some("-u")),
        ];
        for (container, user) in hostile {
            let mut cfg = base.clone();
            let svc = cfg.services.get_mut("db").unwrap();
            svc.container = container.map(str::to_string);
            svc.exec_user = user.map(str::to_string);
            let s = SproutSupervisor::from_config(policy.clone(), cfg)
                .with_container_cli_override(cli.display().to_string());
            assert!(
                s.exec_in_container("db", "id", None).is_err(),
                "{container:?} / {user:?}"
            );
        }
        assert!(
            cli_calls(&ws).is_empty(),
            "hostile specs must never reach the CLI"
        );
    }

    #[test]
    fn exec_applies_sandbox_command_checks_unless_sandbox_is_off() {
        let ws = TempWs::new("sup_exec_sandbox");
        let s = sup_cli(&ws, &container_toml(""));
        assert!(
            s.exec_in_container("db", "curl https://example.com", None)
                .is_err()
        );
        let err = s
            .exec_in_container("db", "echo x > .onyx/config.toml", None)
            .unwrap_err();
        assert!(err.contains("control-plane"), "{err}");
        assert!(cli_calls(&ws).is_empty());

        let cli = ws.path().join("fake-docker.sh");
        let mut off = WorkspacePolicy::new(ws.path()).unwrap();
        off.sandbox.mode = SandboxMode::Off;
        assert_eq!(off.sandbox.network, NetworkPolicy::Offline);
        let s_off =
            SproutSupervisor::new(off).with_container_cli_override(cli.display().to_string());
        let out = s_off
            .exec_in_container("db", "curl https://example.com", None)
            .unwrap();
        assert!(out.contains("exit_code: 0"), "{out}");
        assert_eq!(cli_calls(&ws).len(), 1);
    }

    /// Rustdoc de `SproutSupervisor`: "Al destruirse el último clon (`Drop` del `Arc` interior),
    /// ejecuta `stop_cmd`/`down` y suelta todos los `ProcessJob` garantizando cero procesos
    /// huérfanos".
    #[test]
    #[ignore = "BUG: los hilos internos (monitor child.wait y lectores de stdout/stderr) retienen Arc<SproutInner>; Drop no se ejecuta al soltar el último clon público mientras el servicio vive (sprout.rs:1448-1606)"]
    fn dropping_last_supervisor_clone_runs_stop_cmd_and_kills_service() {
        let ws = TempWs::new("sup_drop");
        let s = sup(
            &ws,
            "[services.srv]\ncmd = \"echo UP; exec sleep 5\"\nready_log = \"UP\"\nstop_cmd = \"echo bye >> stopped.txt\"\n",
        );
        s.start("srv", WAIT).unwrap();
        let pid: u32 = row(&s.status_toon().unwrap(), "srv")[4].parse().unwrap();
        let clone = s.clone();
        drop(s);
        assert!(
            !ws.path().join("stopped.txt").exists(),
            "a live clone keeps services running"
        );
        drop(clone);
        assert_eq!(
            read(&ws, "stopped.txt"),
            "bye\n",
            "dropping the last clone must run stop_cmd synchronously"
        );
        #[cfg(target_os = "linux")]
        assert!(
            wait_until(Duration::from_secs(2), || !Path::new(&format!(
                "/proc/{pid}"
            ))
            .exists()),
            "service process {pid} must not outlive the supervisor"
        );
        let _ = pid;
    }
}
