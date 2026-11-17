pub mod distiller;
pub mod index;
pub mod lang;
pub mod ontology;
pub mod toon;

pub use distiller::{DistillerConfig, ImpactHit, MAX_TEMPERATURE_BP, distill_heat};
pub use index::{IndexStats, WorkspaceIndex};
pub use ontology::{Edge, Entity, EntityKind, RelationKind, ScentArena, ScentError, SupportedLang};
pub use toon::{
    DEFAULT_MAX_BYTES, DEFAULT_MAX_ROWS, TOON_HEADER, ToonPageOptions, ToonRow, decode_toon,
    encode_row, render_toon,
};

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(prefix: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let dir = std::env::temp_dir().join(format!("onyx_scent_{prefix}_{nanos}"));
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn multi_language_index_qualifies_symbols_and_traces_impact() {
        let dir = TempDir::new("multilang");
        fs::create_dir_all(dir.0.join("src")).unwrap();
        fs::write(
            dir.0.join("src/engine.rs"),
            r#"
pub struct Engine {
    count: usize,
}

impl Engine {
    pub fn tick(&mut self) -> usize {
        self.count += 1;
        helper(self.count)
    }
}

fn helper(v: usize) -> usize {
    v + 1
}

pub fn run_all(e: &mut Engine) -> usize {
    e.tick()
}
"#,
        )
        .unwrap();
        fs::write(
            dir.0.join("script.py"),
            r#"
class Worker:
    def run(self):
        return compute(1)

def compute(x):
    return x * 2
"#,
        )
        .unwrap();
        fs::write(
            dir.0.join("App.java"),
            r#"
public class App {
    public int start() {
        return step();
    }
    private int step() {
        return 42;
    }
}
"#,
        )
        .unwrap();

        let mut index = WorkspaceIndex::new();
        let stats = index.index_workspace(&dir.0).unwrap();
        assert_eq!(stats.total_files, 3);
        assert_eq!(stats.reparsed_files, 3);
        assert_eq!(stats.cache_hits, 0);

        let outline = index.outline_toon(ToonPageOptions::default());
        assert!(outline.starts_with(TOON_HEADER), "{outline}");
        assert!(outline.contains("src/engine.rs::Engine::tick"), "{outline}");
        assert!(outline.contains("script.py::Worker::run"), "{outline}");
        assert!(outline.contains("App.java::App::start"), "{outline}");

        let impact = index
            .impact_toon(
                "Engine::tick",
                &DistillerConfig::default(),
                ToonPageOptions::default(),
            )
            .unwrap();
        assert!(impact.contains("epicenter"), "{impact}");
        assert!(impact.contains("src/engine.rs::run_all"), "{impact}");
        assert!(impact.contains("src/engine.rs::helper"), "{impact}");
    }

    #[test]
    fn blake3_cache_skips_unchanged_files_and_output_is_deterministic() {
        let dir = TempDir::new("cache");
        fs::write(
            dir.0.join("lib.rs"),
            "pub fn alpha() -> i32 { beta() }\npub fn beta() -> i32 { 7 }\n",
        )
        .unwrap();
        fs::write(dir.0.join("util.py"), "def ping():\n    return 1\n").unwrap();

        let mut index = WorkspaceIndex::new();
        let first_stats = index.index_workspace(&dir.0).unwrap();
        assert_eq!(first_stats.total_files, 2);
        assert_eq!(first_stats.reparsed_files, 2);
        assert_eq!(first_stats.cache_hits, 0);

        let first_outline = index.outline_toon(ToonPageOptions::default());
        let first_impact = index
            .impact_toon(
                "beta",
                &DistillerConfig::default(),
                ToonPageOptions::default(),
            )
            .unwrap();

        // Second pass with no changes: 100% cache hit, bit-exact outputs.
        let second_stats = index.index_workspace(&dir.0).unwrap();
        assert_eq!(second_stats.cache_hits, 2);
        assert_eq!(second_stats.reparsed_files, 0);
        assert_eq!(
            first_outline,
            index.outline_toon(ToonPageOptions::default())
        );
        assert_eq!(
            first_impact,
            index
                .impact_toon(
                    "beta",
                    &DistillerConfig::default(),
                    ToonPageOptions::default()
                )
                .unwrap()
        );

        // Touch only util.py: only 1 file reparsed.
        fs::write(dir.0.join("util.py"), "def ping():\n    return 2\n").unwrap();
        let third_stats = index.index_workspace(&dir.0).unwrap();
        assert_eq!(third_stats.cache_hits, 1);
        assert_eq!(third_stats.reparsed_files, 1);
    }

    #[test]
    fn toon_round_trip_escapes_delimiters_and_paginates() {
        let rows = vec![
            ToonRow {
                subject: "a|b".to_string(),
                predicate: "fn".to_string(),
                object: "pub fn f() -> A | B\nwhere T: C".to_string(),
                meta: "lines:1-3".to_string(),
            },
            ToonRow {
                subject: "c\\d".to_string(),
                predicate: "method".to_string(),
                object: "fn m(&self)".to_string(),
                meta: "lines:5-8".to_string(),
            },
        ];

        let page1 = render_toon(
            &rows,
            ToonPageOptions {
                cursor: 0,
                max_rows: 1,
                max_bytes: DEFAULT_MAX_BYTES,
                public_only: false,
                exclude_tests: false,
            },
        );
        assert!(page1.contains("# page: next_cursor=1 total=2"), "{page1}");
        let decoded1 = decode_toon(&page1).unwrap();
        assert_eq!(decoded1, vec![rows[0].clone()]);

        let page2 = render_toon(
            &rows,
            ToonPageOptions {
                cursor: 1,
                max_rows: 10,
                max_bytes: DEFAULT_MAX_BYTES,
                public_only: false,
                exclude_tests: false,
            },
        );
        assert!(page2.contains("# total: 2"), "{page2}");
        let decoded2 = decode_toon(&page2).unwrap();
        assert_eq!(decoded2, vec![rows[1].clone()]);
    }

    #[test]
    fn pub_trait_methods_inherit_public_visibility_and_trait_impact_works() {
        let dir = TempDir::new("trait_impact");
        fs::create_dir_all(dir.0.join("src")).unwrap();
        fs::write(
            dir.0.join("src/lib.rs"),
            r#"
pub trait EventBus {
    fn push(&mut self, val: i32);
    fn is_empty(&self) -> bool;
}

pub struct MemBus {
    items: Vec<i32>,
}

pub enum BusMode {
    Fifo,
    Strict,
}

impl EventBus for MemBus {
    fn push(&mut self, val: i32) {
        self.items.push(val);
    }
    fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

pub fn drive(bus: &mut dyn EventBus) -> bool {
    bus.push(1);
    bus.is_empty()
}
"#,
        )
        .unwrap();

        let mut index = WorkspaceIndex::new();
        index.index_workspace(&dir.0).unwrap();

        let pub_outline = index.outline_toon(ToonPageOptions {
            public_only: true,
            ..ToonPageOptions::default()
        });
        assert!(
            pub_outline.contains("src/lib.rs::EventBus::push"),
            "{pub_outline}"
        );
        assert!(
            pub_outline.contains("pub struct MemBus { items }"),
            "{pub_outline}"
        );
        assert!(
            pub_outline.contains("pub enum BusMode { Fifo, Strict }"),
            "{pub_outline}"
        );

        let trait_impact = index
            .impact_toon(
                "EventBus",
                &DistillerConfig::default(),
                ToonPageOptions::default(),
            )
            .unwrap();
        assert!(trait_impact.contains("used_by"), "{trait_impact}");
        assert!(trait_impact.contains("src/lib.rs::drive"), "{trait_impact}");
        assert!(
            trait_impact.contains("src/lib.rs::MemBus::push"),
            "{trait_impact}"
        );
    }

    #[test]
    fn nested_items_not_promoted_and_method_calls_avoid_cross_file_noise() {
        let dir = TempDir::new("no_noise");
        fs::create_dir_all(dir.0.join("src")).unwrap();
        fs::write(
            dir.0.join("src/a.rs"),
            r#"
pub fn caller() {
    struct LocalEntry {
        x: i32,
    }
    fn inner_helper() {}
    let mut items = Vec::new();
    items.push(LocalEntry { x: 1 });
    inner_helper();
}
"#,
        )
        .unwrap();
        fs::write(
            dir.0.join("src/b.rs"),
            r#"
pub struct OtherBus;
impl OtherBus {
    pub fn push(&mut self) {}
}
"#,
        )
        .unwrap();

        let mut index = WorkspaceIndex::new();
        index.index_workspace(&dir.0).unwrap();
        let outline = index.outline_toon(ToonPageOptions::default());
        assert!(!outline.contains("LocalEntry"), "{outline}");
        assert!(!outline.contains("inner_helper"), "{outline}");

        let impact = index
            .impact_toon(
                "caller",
                &DistillerConfig::default(),
                ToonPageOptions::default(),
            )
            .unwrap();
        assert!(!impact.contains("OtherBus::push"), "{impact}");
    }

    #[test]
    fn scope_tags_java_annotations_and_cycle_saturation_work() {
        let dir = TempDir::new("cycles_java");
        fs::write(
            dir.0.join("mod.rs"),
            r#"
pub struct Attr(u32);
pub fn ping() { pong(); }
pub fn pong() { ping(); }

#[cfg(test)]
mod tests {
    #[test]
    fn check_ping() {
        super::ping();
    }
}
"#,
        )
        .unwrap();
        fs::write(
            dir.0.join("Runner.java"),
            r#"
public class Runner {
    @Override
    @SuppressWarnings("unchecked")
    public void run() {}
}
"#,
        )
        .unwrap();

        let mut index = WorkspaceIndex::new();
        index.index_workspace(&dir.0).unwrap();

        let src_only = index.outline_toon(ToonPageOptions {
            public_only: true,
            exclude_tests: true,
            ..ToonPageOptions::default()
        });
        assert!(src_only.contains("pub struct Attr(u32)"), "{src_only}");
        assert!(src_only.contains("Runner.java::Runner::run"), "{src_only}");
        assert!(!src_only.contains("check_ping"), "{src_only}");

        let all_outline = index.outline_toon(ToonPageOptions::default());
        assert!(all_outline.contains("check_ping"), "{all_outline}");
        assert!(all_outline.contains("scope:test"), "{all_outline}");

        let impact = index
            .impact_toon(
                "ping",
                &DistillerConfig::default(),
                ToonPageOptions::default(),
            )
            .unwrap();
        assert!(impact.contains("mod.rs::pong"), "{impact}");
        assert!(impact.contains("temp_bp:10000"), "{impact}");
    }
}
