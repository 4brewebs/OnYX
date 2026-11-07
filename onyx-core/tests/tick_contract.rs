//! Pruebas del runtime con puertos de prueba, no una implementación de soul.
use std::cell::{Cell, RefCell};
use std::rc::Rc;

use onyx_core::{
    Attr, ConceptId, Ctx, Graph, OnyxError, OnyxEvent, OnyxSystem, PlatformDriver, SnapshotStore,
    SystemEvent, SystemState, Value, tick,
};

type Log = Rc<RefCell<Vec<String>>>;

struct TestGraph {
    value: Value,
    reject_import: bool,
}

impl TestGraph {
    fn new() -> Self {
        Self {
            value: Value::Int(0),
            reject_import: false,
        }
    }
}

impl Graph for TestGraph {
    fn attr(&mut self, _: &str) -> Attr {
        Attr(0)
    }
    fn create(&mut self) -> ConceptId {
        panic!("no se usa en estas pruebas")
    }
    fn destroy(&mut self, _: ConceptId) {
        panic!("no se usa en estas pruebas")
    }
    fn get(&self, _: ConceptId, _: Attr) -> Option<&Value> {
        Some(&self.value)
    }
    fn set(&mut self, _: ConceptId, _: Attr, value: Value) {
        self.value = value;
    }
    fn relate(&mut self, _: ConceptId, _: Attr, _: ConceptId) {
        panic!("no se usa en estas pruebas")
    }
    fn unrelate(&mut self, _: ConceptId, _: Attr, _: ConceptId) {
        panic!("no se usa en estas pruebas")
    }
    fn related(&self, _: ConceptId, _: Attr) -> Vec<ConceptId> {
        panic!("no se usa en estas pruebas")
    }
    fn query(&self, _: &[Attr]) -> Vec<ConceptId> {
        panic!("no se usa en estas pruebas")
    }
    fn export(&self) -> Vec<u8> {
        vec![42]
    }
    fn import(&mut self, bytes: &[u8]) -> Result<(), OnyxError> {
        if self.reject_import || bytes != [42] {
            return Err(OnyxError::Format(
                "snapshot rechazado por el puerto de prueba".into(),
            ));
        }
        self.value = Value::Int(42);
        Ok(())
    }
}

#[derive(Default)]
struct TestStore {
    loads: Cell<usize>,
    saves: usize,
}

impl SnapshotStore for TestStore {
    fn save(&mut self, _: &str, bytes: &[u8]) -> Result<(), OnyxError> {
        assert_eq!(bytes, [42]);
        self.saves += 1;
        Ok(())
    }
    fn load(&self, _: &str) -> Result<Vec<u8>, OnyxError> {
        self.loads.set(self.loads.get() + 1);
        Ok(vec![42])
    }
}

struct TestDriver(Log);

impl PlatformDriver for TestDriver {
    fn poll(&mut self, _: &mut Vec<OnyxEvent>) {}
    fn present(&mut self, graph: &dyn Graph) {
        self.0
            .borrow_mut()
            .push(format!("present:{:?}", graph.get(ConceptId(1), Attr(0))));
    }
}

struct TestSystem {
    name: &'static str,
    log: Log,
    request_save: bool,
}

impl OnyxSystem for TestSystem {
    fn name(&self) -> &str {
        self.name
    }
    fn init(&mut self, graph: &mut dyn Graph) -> Result<(), OnyxError> {
        let attr = graph.attr("value");
        self.log.borrow_mut().push(format!(
            "init:{}:{:?}",
            self.name,
            graph.get(ConceptId(1), attr)
        ));
        Ok(())
    }
    fn run(&mut self, ctx: &mut Ctx) {
        self.log.borrow_mut().push(format!("run:{}", self.name));
        if self.request_save {
            ctx.outbox
                .push(OnyxEvent::System(SystemEvent::SnapshotRequested(
                    "saved".into(),
                )));
            self.request_save = false;
        }
    }
    fn on_start(&mut self, graph: &mut dyn Graph) {
        self.log.borrow_mut().push(format!(
            "start:{}:{:?}",
            self.name,
            graph.get(ConceptId(1), Attr(0))
        ));
    }
    fn on_stop(&mut self, _: &mut dyn Graph) {
        self.log.borrow_mut().push(format!("stop:{}", self.name));
    }
}

fn system(name: &'static str, log: &Log) -> Box<dyn OnyxSystem> {
    Box::new(TestSystem {
        name,
        log: Rc::clone(log),
        request_save: false,
    })
}

fn restore() -> OnyxEvent {
    OnyxEvent::System(SystemEvent::RestoreRequested("saved".into()))
}

#[test]
fn restore_reinitializes_every_system_before_presenting_restored_state() {
    let log = Log::default();
    let mut graph = TestGraph::new();
    let mut store = TestStore::default();
    let mut driver = TestDriver(Rc::clone(&log));
    let mut systems = [system("first", &log), system("second", &log)];
    let mut states = vec![SystemState::Active; systems.len()];
    let mut pending = vec![restore()];

    tick(
        0,
        7,
        &mut driver,
        &mut graph,
        &mut store,
        &mut systems,
        &mut states,
        &mut pending,
    );

    assert_eq!(store.loads.get(), 1);
    assert!(matches!(graph.value, Value::Int(42)));
    assert_eq!(
        *log.borrow(),
        [
            "run:first",
            "run:second",
            "init:first:Some(Int(42))",
            "init:second:Some(Int(42))",
            "present:Some(Int(42))",
        ]
    );
    assert!(
        matches!(pending.as_slice(), [OnyxEvent::System(SystemEvent::SnapshotRestored(key))] if key == "saved")
    );
}

#[test]
fn rejected_import_emits_fault_without_reinitializing_systems() {
    let log = Log::default();
    let mut graph = TestGraph::new();
    graph.reject_import = true;
    let mut store = TestStore::default();
    let mut driver = TestDriver(Rc::clone(&log));
    let mut systems = [system("worker", &log)];
    let mut states = vec![SystemState::Active; systems.len()];
    let mut pending = vec![restore()];

    tick(
        0,
        7,
        &mut driver,
        &mut graph,
        &mut store,
        &mut systems,
        &mut states,
        &mut pending,
    );

    assert!(matches!(graph.value, Value::Int(0)));
    assert_eq!(*log.borrow(), ["run:worker", "present:Some(Int(0))"]);
    assert!(matches!(
        pending.as_slice(),
        [OnyxEvent::System(SystemEvent::Fault(_))]
    ));
}

#[test]
fn snapshot_request_from_outbox_is_processed_in_the_same_tick() {
    let log = Log::default();
    let mut graph = TestGraph::new();
    let mut store = TestStore::default();
    let mut driver = TestDriver(Rc::clone(&log));
    let mut systems: [Box<dyn OnyxSystem>; 1] = [Box::new(TestSystem {
        name: "worker",
        log: Rc::clone(&log),
        request_save: true,
    })];
    let mut states = vec![SystemState::Active; systems.len()];
    let mut pending = Vec::new();

    tick(
        0,
        7,
        &mut driver,
        &mut graph,
        &mut store,
        &mut systems,
        &mut states,
        &mut pending,
    );
    assert_eq!(store.saves, 1);
    tick(
        1,
        7,
        &mut driver,
        &mut graph,
        &mut store,
        &mut systems,
        &mut states,
        &mut pending,
    );
    assert_eq!(store.saves, 1);
}

#[test]
fn restore_notification_does_not_restore_the_snapshot_again() {
    let log = Log::default();
    let mut graph = TestGraph::new();
    let mut store = TestStore::default();
    let mut driver = TestDriver(Rc::clone(&log));
    let mut systems = [system("worker", &log)];
    let mut states = vec![SystemState::Active; systems.len()];
    let mut pending = vec![restore()];

    tick(
        0,
        7,
        &mut driver,
        &mut graph,
        &mut store,
        &mut systems,
        &mut states,
        &mut pending,
    );
    tick(
        1,
        7,
        &mut driver,
        &mut graph,
        &mut store,
        &mut systems,
        &mut states,
        &mut pending,
    );

    assert_eq!(
        store.loads.get(),
        1,
        "la notificación no debe repetir el restore"
    );
}

#[test]
fn stopped_system_does_not_run_again_without_start() {
    let log = Log::default();
    let mut graph = TestGraph::new();
    let mut store = TestStore::default();
    let mut driver = TestDriver(Rc::clone(&log));
    let mut systems = [system("worker", &log)];
    let mut states = vec![SystemState::Active; systems.len()];
    let mut pending = vec![OnyxEvent::System(SystemEvent::Stop("worker".into()))];

    tick(
        0,
        7,
        &mut driver,
        &mut graph,
        &mut store,
        &mut systems,
        &mut states,
        &mut pending,
    );
    tick(
        1,
        7,
        &mut driver,
        &mut graph,
        &mut store,
        &mut systems,
        &mut states,
        &mut pending,
    );

    let log = log.borrow();
    assert_eq!(
        log.iter()
            .filter(|entry| entry.as_str() == "stop:worker")
            .count(),
        1
    );
    assert_eq!(
        log.iter()
            .filter(|entry| entry.as_str() == "run:worker")
            .count(),
        1,
        "Stop al final del primer tick debe impedir run en el siguiente"
    );
}

struct RngSystem {
    name: &'static str,
    log: Log,
}

impl OnyxSystem for RngSystem {
    fn name(&self) -> &str {
        self.name
    }
    fn init(&mut self, _: &mut dyn Graph) -> Result<(), OnyxError> {
        Ok(())
    }
    fn run(&mut self, ctx: &mut Ctx) {
        self.log
            .borrow_mut()
            .push(format!("{}:{}", self.name, ctx.rng.next_u64()));
    }
}

#[test]
fn tick_shares_the_seeded_rng_in_system_order() {
    let log = Log::default();
    let mut graph = TestGraph::new();
    let mut store = TestStore::default();
    let mut driver = TestDriver(Rc::clone(&log));
    let mut systems: [Box<dyn OnyxSystem>; 2] = [
        Box::new(RngSystem {
            name: "first",
            log: Rc::clone(&log),
        }),
        Box::new(RngSystem {
            name: "second",
            log: Rc::clone(&log),
        }),
    ];
    let mut states = vec![SystemState::Active; systems.len()];
    let mut pending = Vec::new();

    tick(
        2,
        1,
        &mut driver,
        &mut graph,
        &mut store,
        &mut systems,
        &mut states,
        &mut pending,
    );

    // Primeras dos salidas del vector de referencia seed=1, tick=2.
    assert_eq!(
        *log.borrow(),
        [
            "first:3999644361938179224",
            "second:2243154290913709655",
            "present:Some(Int(0))",
        ]
    );
}

struct EventSystem {
    seen: Rc<RefCell<Vec<Vec<OnyxEvent>>>>,
    emit: Vec<OnyxEvent>,
}

impl OnyxSystem for EventSystem {
    fn name(&self) -> &str {
        "events"
    }
    fn init(&mut self, _: &mut dyn Graph) -> Result<(), OnyxError> {
        Ok(())
    }
    fn run(&mut self, ctx: &mut Ctx) {
        self.seen.borrow_mut().push(ctx.inbox.to_vec());
        ctx.outbox.append(&mut self.emit);
    }
}

#[test]
fn normal_outbox_events_are_delivered_once_in_order_without_echoing_inbox() {
    use onyx_core::DomainEvent;
    let seen = Rc::new(RefCell::new(Vec::new()));
    let log = Log::default();
    let mut graph = TestGraph::new();
    let mut store = TestStore::default();
    let mut driver = TestDriver(log);
    let domain = |kind| {
        OnyxEvent::Domain(DomainEvent {
            kind: Attr(kind),
            subject: None,
            data: Vec::new(),
        })
    };
    let mut systems: [Box<dyn OnyxSystem>; 1] = [Box::new(EventSystem {
        seen: Rc::clone(&seen),
        emit: vec![
            domain(1),
            OnyxEvent::System(SystemEvent::SnapshotRequested("saved".into())),
            OnyxEvent::System(SystemEvent::StageLoaded("stage".into())),
            domain(2),
        ],
    })];
    let mut states = [SystemState::Active];
    let mut pending = Vec::new();

    tick(
        0,
        7,
        &mut driver,
        &mut graph,
        &mut store,
        &mut systems,
        &mut states,
        &mut pending,
    );
    assert_eq!(store.saves, 1);
    assert_eq!(pending.len(), 3);
    tick(
        1,
        7,
        &mut driver,
        &mut graph,
        &mut store,
        &mut systems,
        &mut states,
        &mut pending,
    );
    tick(
        2,
        7,
        &mut driver,
        &mut graph,
        &mut store,
        &mut systems,
        &mut states,
        &mut pending,
    );

    let seen = seen.borrow();
    assert!(matches!(seen[1].as_slice(), [
        OnyxEvent::Domain(DomainEvent { kind: Attr(1), .. }),
        OnyxEvent::System(SystemEvent::StageLoaded(stage)),
        OnyxEvent::Domain(DomainEvent { kind: Attr(2), .. }),
        OnyxEvent::System(SystemEvent::Tick { n: 1 }),
    ] if stage == "stage"));
    assert!(matches!(
        seen[2].as_slice(),
        [OnyxEvent::System(SystemEvent::Tick { n: 2 })]
    ));
    assert_eq!(store.saves, 1, "la petición atendida tampoco se reencola");
}

#[test]
fn inactive_system_reinitializes_before_a_start_hook_after_restore() {
    let log = Log::default();
    let mut graph = TestGraph::new();
    let mut store = TestStore::default();
    let mut driver = TestDriver(Rc::clone(&log));
    let mut systems = [system("worker", &log)];
    let mut states = [SystemState::Stopped];
    let mut pending = vec![
        restore(),
        OnyxEvent::System(SystemEvent::Start("worker".into())),
    ];

    tick(
        0,
        7,
        &mut driver,
        &mut graph,
        &mut store,
        &mut systems,
        &mut states,
        &mut pending,
    );

    assert_eq!(states, [SystemState::Active]);
    assert_eq!(
        *log.borrow(),
        [
            "init:worker:Some(Int(42))",
            "start:worker:Some(Int(42))",
            "present:Some(Int(42))",
        ]
    );
    tick(
        1,
        7,
        &mut driver,
        &mut graph,
        &mut store,
        &mut systems,
        &mut states,
        &mut pending,
    );
    assert_eq!(store.loads.get(), 1);
    assert_eq!(
        log.borrow()
            .iter()
            .filter(|entry| entry.as_str() == "run:worker")
            .count(),
        1
    );
}

#[test]
fn repeated_start_and_stop_only_call_hooks_on_state_transitions() {
    let log = Log::default();
    let mut graph = TestGraph::new();
    let mut store = TestStore::default();
    let mut driver = TestDriver(Rc::clone(&log));
    let mut systems = [system("worker", &log)];
    let mut states = [SystemState::Active];
    let mut pending = vec![
        OnyxEvent::System(SystemEvent::Stop("worker".into())),
        OnyxEvent::System(SystemEvent::Stop("worker".into())),
    ];
    tick(
        0,
        7,
        &mut driver,
        &mut graph,
        &mut store,
        &mut systems,
        &mut states,
        &mut pending,
    );
    assert_eq!(states, [SystemState::Stopped]);
    pending.extend([
        OnyxEvent::System(SystemEvent::Start("worker".into())),
        OnyxEvent::System(SystemEvent::Start("worker".into())),
    ]);
    tick(
        1,
        7,
        &mut driver,
        &mut graph,
        &mut store,
        &mut systems,
        &mut states,
        &mut pending,
    );
    assert_eq!(states, [SystemState::Active]);
    tick(
        2,
        7,
        &mut driver,
        &mut graph,
        &mut store,
        &mut systems,
        &mut states,
        &mut pending,
    );
    let log = log.borrow();
    assert_eq!(
        log.iter()
            .filter(|entry| entry.as_str() == "stop:worker")
            .count(),
        1
    );
    assert_eq!(
        log.iter()
            .filter(|entry| entry.starts_with("start:worker:"))
            .count(),
        1
    );
    assert_eq!(
        log.iter()
            .filter(|entry| entry.as_str() == "run:worker")
            .count(),
        2
    );
}

struct FallibleSystem {
    log: Log,
    fail_init: Rc<Cell<bool>>,
}

impl OnyxSystem for FallibleSystem {
    fn name(&self) -> &str {
        "fallible"
    }
    fn init(&mut self, _: &mut dyn Graph) -> Result<(), OnyxError> {
        if self.fail_init.get() {
            self.log.borrow_mut().push("init:failed".into());
            Err(OnyxError::Missing)
        } else {
            self.log.borrow_mut().push("init:ok".into());
            Ok(())
        }
    }
    fn run(&mut self, _: &mut Ctx) {
        self.log.borrow_mut().push("run:fallible".into());
    }
    fn on_start(&mut self, _: &mut dyn Graph) {
        self.log.borrow_mut().push("start:fallible".into());
    }
    fn on_stop(&mut self, _: &mut dyn Graph) {
        self.log.borrow_mut().push("stop:fallible".into());
    }
}

#[test]
fn failed_reinit_blocks_every_previous_state_without_running_hooks() {
    for previous in [
        SystemState::Active,
        SystemState::Stopped,
        SystemState::Blocked,
    ] {
        let log = Log::default();
        let mut graph = TestGraph::new();
        let mut store = TestStore::default();
        let mut driver = TestDriver(Rc::clone(&log));
        let mut systems: [Box<dyn OnyxSystem>; 1] = [Box::new(FallibleSystem {
            log: Rc::clone(&log),
            fail_init: Rc::new(Cell::new(true)),
        })];
        let mut states = [previous];
        let mut pending = vec![restore()];
        tick(
            0,
            7,
            &mut driver,
            &mut graph,
            &mut store,
            &mut systems,
            &mut states,
            &mut pending,
        );
        assert_eq!(states, [SystemState::Blocked]);
        assert!(
            pending
                .iter()
                .any(|event| matches!(event, OnyxEvent::System(SystemEvent::Fault(_))))
        );
        let log = log.borrow();
        assert!(
            !log.iter()
                .any(|entry| entry.starts_with("start:") || entry.starts_with("stop:"))
        );
    }
}

#[test]
fn blocked_system_rejects_start_and_stop_without_run_or_hooks_across_ticks() {
    let log = Log::default();
    let mut graph = TestGraph::new();
    let mut store = TestStore::default();
    let mut driver = TestDriver(Rc::clone(&log));
    let mut systems: [Box<dyn OnyxSystem>; 1] = [Box::new(FallibleSystem {
        log: Rc::clone(&log),
        fail_init: Rc::new(Cell::new(true)),
    })];
    let mut states = [SystemState::Active];
    let commands = || {
        vec![
            OnyxEvent::System(SystemEvent::Start("fallible".into())),
            OnyxEvent::System(SystemEvent::Stop("fallible".into())),
        ]
    };
    let mut pending = vec![restore()];
    pending.extend(commands());
    tick(
        0,
        7,
        &mut driver,
        &mut graph,
        &mut store,
        &mut systems,
        &mut states,
        &mut pending,
    );
    pending.clear();
    pending.extend(commands());
    tick(
        1,
        7,
        &mut driver,
        &mut graph,
        &mut store,
        &mut systems,
        &mut states,
        &mut pending,
    );

    assert_eq!(states, [SystemState::Blocked]);
    assert!(matches!(
        pending.as_slice(),
        [OnyxEvent::System(SystemEvent::Fault(_))]
    ));
    let log = log.borrow();
    assert_eq!(
        log.iter()
            .filter(|entry| entry.as_str() == "run:fallible")
            .count(),
        1
    );
    assert!(
        !log.iter()
            .any(|entry| entry.starts_with("start:") || entry.starts_with("stop:"))
    );
}

#[test]
fn successful_reinit_unblocks_to_stopped_until_an_explicit_start() {
    let log = Log::default();
    let mut graph = TestGraph::new();
    let mut store = TestStore::default();
    let mut driver = TestDriver(Rc::clone(&log));
    let mut systems: [Box<dyn OnyxSystem>; 1] = [Box::new(FallibleSystem {
        log: Rc::clone(&log),
        fail_init: Rc::new(Cell::new(false)),
    })];
    let mut states = [SystemState::Blocked];
    let mut pending = vec![restore()];
    tick(
        0,
        7,
        &mut driver,
        &mut graph,
        &mut store,
        &mut systems,
        &mut states,
        &mut pending,
    );
    assert_eq!(states, [SystemState::Stopped]);
    assert_eq!(*log.borrow(), ["init:ok", "present:Some(Int(42))"]);
    tick(
        1,
        7,
        &mut driver,
        &mut graph,
        &mut store,
        &mut systems,
        &mut states,
        &mut pending,
    );
    assert!(
        !log.borrow()
            .iter()
            .any(|entry| entry.as_str() == "run:fallible")
    );
    pending.push(OnyxEvent::System(SystemEvent::Start("fallible".into())));
    tick(
        2,
        7,
        &mut driver,
        &mut graph,
        &mut store,
        &mut systems,
        &mut states,
        &mut pending,
    );
    assert_eq!(states, [SystemState::Active]);
    tick(
        3,
        7,
        &mut driver,
        &mut graph,
        &mut store,
        &mut systems,
        &mut states,
        &mut pending,
    );
    let log = log.borrow();
    assert_eq!(
        log.iter()
            .filter(|entry| entry.as_str() == "start:fallible")
            .count(),
        1
    );
    assert_eq!(
        log.iter()
            .filter(|entry| entry.as_str() == "run:fallible")
            .count(),
        1
    );
}
