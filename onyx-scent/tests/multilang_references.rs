//! `SDD-05` TEST-05.1: Multi-language AST indexing (`.ts`, `.tsx`, `.js`, `.mjs`, `.go`, `.cpp`, `.c`)
//! generating valid `@TOON v1` outlines and resolving `impact_toon` and `references_toon`.

use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use onyx_scent::{
    DistillerConfig, EntityKind, SupportedLang, TOON_HEADER, ToonPageOptions, WorkspaceIndex,
    decode_toon,
};

struct TempWorkspace(PathBuf);

impl TempWorkspace {
    fn new(prefix: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "onyx_scent_sdd05_{prefix}_{}_{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for TempWorkspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn supported_lang_extension_mapping_covers_sdd05_languages() {
    assert_eq!(
        SupportedLang::from_extension("ts"),
        Some(SupportedLang::TypeScript)
    );
    assert_eq!(
        SupportedLang::from_extension("tsx"),
        Some(SupportedLang::TypeScript)
    );
    assert_eq!(
        SupportedLang::from_extension("mts"),
        Some(SupportedLang::TypeScript)
    );
    assert_eq!(
        SupportedLang::from_extension("cts"),
        Some(SupportedLang::TypeScript)
    );
    assert_eq!(
        SupportedLang::from_extension("js"),
        Some(SupportedLang::JavaScript)
    );
    assert_eq!(
        SupportedLang::from_extension("jsx"),
        Some(SupportedLang::JavaScript)
    );
    assert_eq!(
        SupportedLang::from_extension("mjs"),
        Some(SupportedLang::JavaScript)
    );
    assert_eq!(
        SupportedLang::from_extension("cjs"),
        Some(SupportedLang::JavaScript)
    );
    assert_eq!(SupportedLang::from_extension("go"), Some(SupportedLang::Go));
    assert_eq!(SupportedLang::from_extension("c"), Some(SupportedLang::Cpp));
    assert_eq!(SupportedLang::from_extension("h"), Some(SupportedLang::Cpp));
    assert_eq!(
        SupportedLang::from_extension("cpp"),
        Some(SupportedLang::Cpp)
    );
    assert_eq!(
        SupportedLang::from_extension("hpp"),
        Some(SupportedLang::Cpp)
    );
    assert_eq!(
        SupportedLang::from_extension("cc"),
        Some(SupportedLang::Cpp)
    );
    assert_eq!(
        SupportedLang::from_extension("cxx"),
        Some(SupportedLang::Cpp)
    );
    assert_eq!(
        SupportedLang::from_extension("hh"),
        Some(SupportedLang::Cpp)
    );
}

#[test]
fn ignored_dirs_exclude_onyx_agents_and_tooling_directories() {
    let ws = TempWorkspace::new("ignored_dirs");
    fs::create_dir_all(ws.0.join("src")).unwrap();
    fs::create_dir_all(ws.0.join(".onyx/worktrees/wt-1")).unwrap();
    fs::create_dir_all(ws.0.join(".agents/skills/demo")).unwrap();
    fs::create_dir_all(ws.0.join(".venv/lib/site")).unwrap();
    fs::create_dir_all(ws.0.join(".next/cache")).unwrap();
    fs::write(ws.0.join("src/lib.rs"), "pub fn visible() -> i32 { 1 }\n").unwrap();
    // Copias que NUNCA deben indexarse (worktrees de subagentes, guías, deps, build).
    fs::write(
        ws.0.join(".onyx/worktrees/wt-1/lib.rs"),
        "pub fn shadow() -> i32 { 2 }\n",
    )
    .unwrap();
    fs::write(
        ws.0.join(".agents/skills/demo/skill.rs"),
        "pub fn guide() -> i32 { 3 }\n",
    )
    .unwrap();
    fs::write(
        ws.0.join(".venv/lib/site/dep.rs"),
        "pub fn dep() -> i32 { 4 }\n",
    )
    .unwrap();
    fs::write(ws.0.join(".next/cache/page.js"), "export const page = 5;\n").unwrap();

    let mut idx = WorkspaceIndex::new();
    let stats = idx.index_workspace(&ws.0).unwrap();
    assert_eq!(stats.total_files, 1, "only src/lib.rs may be indexed");

    let outline = idx.outline_toon(ToonPageOptions::default());
    assert!(outline.contains("src/lib.rs::visible"), "{outline}");
    for forbidden in ["shadow", "guide", "dep", "page"] {
        assert!(!outline.contains(forbidden), "{outline}");
    }
}

#[test]
fn go_test_prefix_is_not_test_outside_test_files() {
    let ws = TempWorkspace::new("go_test_prefix");
    fs::create_dir_all(ws.0.join("health")).unwrap();
    fs::write(
        ws.0.join("health/health.go"),
        r#"
package health

// TestConnection es un health check de producción, no un test.
func TestConnection() error {
    return ping()
}

func ping() error {
    return nil
}
"#,
    )
    .unwrap();
    fs::write(
        ws.0.join("health/ping_test.go"),
        r#"
package health

func helperForTest() int {
    return pingCount
}

var pingCount = 1
"#,
    )
    .unwrap();

    let mut idx = WorkspaceIndex::new();
    idx.index_workspace(&ws.0).unwrap();
    let outline = idx.outline_toon(ToonPageOptions::default());
    let conn_row = outline
        .lines()
        .find(|l| l.starts_with("health/health.go::TestConnection |"))
        .unwrap_or("");
    assert!(
        conn_row.contains("| fn | func TestConnection() error |") && conn_row.contains("scope:src"),
        "{outline}"
    );
    let test_row = outline
        .lines()
        .find(|l| l.starts_with("health/ping_test.go::helperForTest"))
        .unwrap_or("");
    assert!(test_row.contains("scope:test"), "{outline}");
}

#[test]
fn ts_js_multi_declarator_signatures_are_per_declarator() {
    let ws = TempWorkspace::new("multi_declarator");
    fs::create_dir_all(ws.0.join("src")).unwrap();
    fs::write(
        ws.0.join("src/multi.ts"),
        r#"
export const alpha = (x: number): number => x,
  beta = (y: number): number => y + 1;
"#,
    )
    .unwrap();

    let mut idx = WorkspaceIndex::new();
    idx.index_workspace(&ws.0).unwrap();
    let outline = idx.outline_toon(ToonPageOptions::default());
    let beta_row = outline
        .lines()
        .find(|l| l.starts_with("src/multi.ts::beta |"))
        .unwrap_or("");
    assert!(beta_row.contains("beta = (y: number): number"), "{outline}");
    assert!(
        !beta_row.contains("alpha"),
        "beta's signature must not leak alpha's declarator: {outline}"
    );
    let alpha_row = outline
        .lines()
        .find(|l| l.starts_with("src/multi.ts::alpha |"))
        .unwrap_or("");
    assert!(
        alpha_row.contains("alpha = (x: number): number"),
        "{outline}"
    );
}

#[test]
fn typescript_and_tsx_outline_impact_and_references() {
    let ws = TempWorkspace::new("ts_tsx");
    fs::create_dir_all(ws.0.join("src")).unwrap();

    fs::write(
        ws.0.join("src/service.ts"),
        r#"
export interface Store {
    get(key: string): string;
}

export type UserId = string;

export class MemoryStore implements Store {
    public get(key: string): string {
        return normalizeKey(key);
    }
}

export function normalizeKey(raw: string): string {
    return raw.trim();
}

export const loadUser = (store: Store, id: UserId): string => {
    return store.get(id);
};
"#,
    )
    .unwrap();

    fs::write(
        ws.0.join("src/App.tsx"),
        r#"
export function Badge(props: { label: string }) {
    return <span>{props.label}</span>;
}

export function Dashboard() {
    return (
        <div>
            <Badge label="ok" />
        </div>
    );
}
"#,
    )
    .unwrap();

    let mut idx = WorkspaceIndex::new();
    let stats = idx.index_workspace(&ws.0).unwrap();
    assert_eq!(stats.total_files, 2);

    let outline = idx.outline_toon(ToonPageOptions::default());
    assert!(outline.starts_with(TOON_HEADER), "{outline}");
    let decoded = decode_toon(&outline).expect("outline must be valid @TOON v1");
    assert!(
        decoded
            .iter()
            .any(|r| r.subject == "src/service.ts::Store" && r.predicate == "interface"),
        "{outline}"
    );
    assert!(
        decoded
            .iter()
            .any(|r| r.subject == "src/service.ts::UserId" && r.predicate == "type_alias"),
        "{outline}"
    );
    assert!(
        decoded
            .iter()
            .any(|r| r.subject == "src/service.ts::MemoryStore" && r.predicate == "class"),
        "{outline}"
    );
    assert!(
        decoded
            .iter()
            .any(|r| r.subject == "src/App.tsx::Badge" && r.predicate == "component"),
        "{outline}"
    );
    assert!(
        idx.arena
            .nodes
            .iter()
            .any(|n| n.short_name == "Badge" && n.kind == EntityKind::Component)
    );

    // Impact & references for normalizeKey (called by MemoryStore::get).
    let impact = idx
        .impact_toon(
            "normalizeKey",
            &DistillerConfig::default(),
            ToonPageOptions::default(),
        )
        .unwrap();
    assert!(
        impact.contains("src/service.ts::MemoryStore::get"),
        "{impact}"
    );
    decode_toon(&impact).expect("impact must be valid @TOON v1");

    let refs = idx
        .references_toon("normalizeKey", ToonPageOptions::default())
        .unwrap();
    let ref_rows = decode_toon(&refs).expect("references must be valid @TOON v1");
    assert!(
        ref_rows
            .iter()
            .any(|r| r.predicate == "defined_at" && r.subject == "src/service.ts::normalizeKey"),
        "{refs}"
    );
    assert!(
        ref_rows
            .iter()
            .any(|r| r.predicate == "called_by" && r.object == "src/service.ts::MemoryStore::get"),
        "{refs}"
    );

    // References for interface Store must report MemoryStore as implementor.
    let store_refs = idx
        .references_toon("Store", ToonPageOptions::default())
        .unwrap();
    let store_rows = decode_toon(&store_refs).unwrap();
    assert!(
        store_rows
            .iter()
            .any(|r| r.predicate == "implemented_by" && r.object == "src/service.ts::MemoryStore"),
        "{store_refs}"
    );

    // JSX component reference: Dashboard renders <Badge />.
    let badge_refs = idx
        .references_toon("Badge", ToonPageOptions::default())
        .unwrap();
    assert!(
        badge_refs.contains("src/App.tsx::Dashboard"),
        "{badge_refs}"
    );
}

#[test]
fn javascript_go_and_cpp_outline_impact_and_references() {
    let ws = TempWorkspace::new("js_go_cpp");
    fs::create_dir_all(ws.0.join("pkg")).unwrap();

    fs::write(
        ws.0.join("pkg/calc.mjs"),
        r#"
export function doubleValue(n) {
    return n * 2;
}

export class Calculator {
    compute(x) {
        return doubleValue(x) + 1;
    }
}
"#,
    )
    .unwrap();

    fs::write(
        ws.0.join("pkg/server.go"),
        r#"
package pkg

type Config struct {
    Port int
}

type Handler interface {
    Serve() error
}

type Server struct {
    Cfg Config
}

func (s *Server) Start() error {
    return s.listen()
}

func (s *Server) listen() error {
    return boot(s.Cfg)
}

func boot(cfg Config) error {
    return nil
}
"#,
    )
    .unwrap();

    fs::write(
        ws.0.join("pkg/engine.cpp"),
        r#"
namespace core {

struct Options {
    int retries;
};

using RetryCount = int;

class BaseEngine {
public:
    int ping() {
        return 1;
    }
};

class FastEngine : public BaseEngine {
public:
    int run(Options opts) {
        return helper(opts.retries);
    }
};

int helper(int n) {
    return n + 1;
}

}
"#,
    )
    .unwrap();

    let mut idx = WorkspaceIndex::new();
    let stats = idx.index_workspace(&ws.0).unwrap();
    assert_eq!(stats.total_files, 3);

    let outline = idx.outline_toon(ToonPageOptions::default());
    let rows = decode_toon(&outline).expect("multi-lang outline must be valid @TOON v1");

    // JavaScript
    assert!(
        rows.iter()
            .any(|r| r.subject == "pkg/calc.mjs::doubleValue" && r.meta.contains("lang:javascript")),
        "{outline}"
    );
    let js_refs = idx
        .references_toon("doubleValue", ToonPageOptions::default())
        .unwrap();
    assert!(
        js_refs.contains("pkg/calc.mjs::Calculator::compute"),
        "{js_refs}"
    );

    // Go
    assert!(
        rows.iter()
            .any(|r| r.subject == "pkg/server.go::Server::Start" && r.meta.contains("lang:go")),
        "{outline}"
    );
    assert!(
        rows.iter()
            .any(|r| r.subject == "pkg/server.go::Handler" && r.predicate == "interface"),
        "{outline}"
    );
    let go_impact = idx
        .impact_toon(
            "boot",
            &DistillerConfig::default(),
            ToonPageOptions::default(),
        )
        .unwrap();
    assert!(
        go_impact.contains("pkg/server.go::Server::listen"),
        "{go_impact}"
    );
    assert!(
        go_impact.contains("pkg/server.go::Server::Start"),
        "{go_impact}"
    );

    let go_cfg_refs = idx
        .references_toon("Config", ToonPageOptions::default())
        .unwrap();
    assert!(
        go_cfg_refs.contains("pkg/server.go::Server"),
        "{go_cfg_refs}"
    );

    // C++
    assert!(
        rows.iter()
            .any(|r| r.subject == "pkg/engine.cpp::core::FastEngine" && r.meta.contains("lang:cpp")),
        "{outline}"
    );
    assert!(
        rows.iter()
            .any(|r| r.subject == "pkg/engine.cpp::core::RetryCount" && r.predicate == "type_alias"),
        "{outline}"
    );
    let cpp_refs = idx
        .references_toon("helper", ToonPageOptions::default())
        .unwrap();
    assert!(
        cpp_refs.contains("pkg/engine.cpp::core::FastEngine::run"),
        "{cpp_refs}"
    );
    let base_refs = idx
        .references_toon("BaseEngine", ToonPageOptions::default())
        .unwrap();
    assert!(
        base_refs.contains("implemented_by")
            && base_refs.contains("pkg/engine.cpp::core::FastEngine"),
        "{base_refs}"
    );
}
