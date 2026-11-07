//! Pruebas del runtime con puertos de prueba, no una implementación de soul.
use std::cell::{Cell, RefCell};
use std::rc::Rc;

use onyx_core::{
    Attr, AttrOnly, AttrRegistry, ConceptId, Ctx, EventBus, Graph, GraphView, OnyxError, OnyxEvent,
    OnyxSystem, PlatformDriver, ReadOnly, SnapshotStore, SystemEvent, SystemState, Value, tick,
};

type Log = Rc<RefCell<Vec<String>>>;

#[derive(Default)]
struct TestBus {
    events: Vec<OnyxEvent>,
    drains: usize,
}

impl From<Vec<OnyxEvent>> for TestBus {
    fn from(events: Vec<OnyxEvent>) -> Self {
        Self { events, drains: 0 }
    }
}

impl EventBus for TestBus {
    fn push(&mut self, event: OnyxEvent) {
        self.events.push(event);
    }
    fn drain(&mut self) -> Vec<OnyxEvent> {
        self.drains += 1;
        std::mem::take(&mut self.events)
    }
    fn is_empty(&self) -> bool {
        self.events.is_empty()
    }
}

struct TestGraph {
    value: Value,
    reject_import: bool,
    attrs: Vec<String>,
}

impl TestGraph {
    fn new() -> Self {
        Self {
            value: Value::Int(0),
            reject_import: false,
            attrs: vec!["value".into()],
        }
    }
}

impl AttrRegistry for TestGraph {
    fn attr(&mut self, name: &str) -> Attr {
        let index = match self.attrs.iter().position(|existing| existing == name) {
            Some(index) => index,
            None => {
                self.attrs.push(name.into());
                self.attrs.len() - 1
            }
        };
        Attr::from_raw(u32::try_from(index).expect("registro de prueba pequeño"))
    }
}

impl GraphView for TestGraph {
    fn exists(&self, id: ConceptId) -> bool {
        id == ConceptId(1)
    }
    fn get(&self, id: ConceptId, attr: Attr) -> Option<Value> {
        (self.exists(id) && attr.raw() == 0).then(|| self.value.clone())
    }
    fn related(&self, _: ConceptId, _: Attr) -> Vec<ConceptId> {
        Vec::new()
    }
    fn query(&self, with: &[Attr]) -> Vec<ConceptId> {
        if with.iter().all(|attr| attr.raw() == 0) {
            vec![ConceptId(1)]
        } else {
            Vec::new()
        }
    }
    fn props(&self, id: ConceptId) -> Vec<(String, Value)> {
        if self.exists(id) {
            vec![("value".into(), self.value.clone())]
        } else {
            Vec::new()
        }
    }
    fn links(&self, _: ConceptId) -> Vec<(String, ConceptId)> {
        Vec::new()
    }
}

impl Graph for TestGraph {
    fn create(&mut self) -> ConceptId {
        panic!("no se usa en estas pruebas")
    }
    fn destroy(&mut self, _: ConceptId) {
        panic!("no se usa en estas pruebas")
    }
    fn set(&mut self, id: ConceptId, attr: Attr, value: Value) {
        if self.exists(id) && attr.raw() == 0 {
            self.value = value;
        }
    }
    fn relate(&mut self, _: ConceptId, _: Attr, _: ConceptId) {
        panic!("no se usa en estas pruebas")
    }
    fn unrelate(&mut self, _: ConceptId, _: Attr, _: ConceptId) {
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
    fn poll_into(&mut self, _: &mut dyn EventBus) {}
    fn present(&mut self, graph: &dyn GraphView) {
        self.0.borrow_mut().push(format!(
            "present:{:?}",
            graph.get(ConceptId(1), Attr::from_raw(0))
        ));
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
    fn init(&mut self, attrs: &mut dyn AttrRegistry) -> Result<(), OnyxError> {
        attrs.attr("value");
        self.log.borrow_mut().push(format!("init:{}", self.name));
        Ok(())
    }
    fn on_restore(&mut self, graph: &dyn GraphView) {
        self.log.borrow_mut().push(format!(
            "restore:{}:{:?}",
            self.name,
            graph.get(ConceptId(1), Attr::from_raw(0))
        ));
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
            graph.get(ConceptId(1), Attr::from_raw(0))
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
    let mut bus = TestBus::from(vec![restore()]);

    tick(
        0,
        7,
        &mut driver,
        &mut graph,
        &mut store,
        &mut bus,
        &mut systems,
        &mut states,
    );

    assert_eq!(store.loads.get(), 1);
    assert!(matches!(graph.value, Value::Int(42)));
    assert_eq!(
        *log.borrow(),
        [
            "run:first",
            "run:second",
            "init:first",
            "restore:first:Some(Int(42))",
            "init:second",
            "restore:second:Some(Int(42))",
            "present:Some(Int(42))",
        ]
    );
    assert!(
        matches!(bus.events.as_slice(), [OnyxEvent::System(SystemEvent::SnapshotRestored(key))] if key == "saved")
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
    let mut bus = TestBus::from(vec![restore()]);

    tick(
        0,
        7,
        &mut driver,
        &mut graph,
        &mut store,
        &mut bus,
        &mut systems,
        &mut states,
    );

    assert!(matches!(graph.value, Value::Int(0)));
    assert_eq!(*log.borrow(), ["run:worker", "present:Some(Int(0))"]);
    assert!(matches!(
        bus.events.as_slice(),
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
    let mut bus = TestBus::default();

    tick(
        0,
        7,
        &mut driver,
        &mut graph,
        &mut store,
        &mut bus,
        &mut systems,
        &mut states,
    );
    assert_eq!(store.saves, 1);
    tick(
        1,
        7,
        &mut driver,
        &mut graph,
        &mut store,
        &mut bus,
        &mut systems,
        &mut states,
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
    let mut bus = TestBus::from(vec![restore()]);

    tick(
        0,
        7,
        &mut driver,
        &mut graph,
        &mut store,
        &mut bus,
        &mut systems,
        &mut states,
    );
    tick(
        1,
        7,
        &mut driver,
        &mut graph,
        &mut store,
        &mut bus,
        &mut systems,
        &mut states,
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
    let mut bus = TestBus::from(vec![OnyxEvent::System(SystemEvent::Stop("worker".into()))]);

    tick(
        0,
        7,
        &mut driver,
        &mut graph,
        &mut store,
        &mut bus,
        &mut systems,
        &mut states,
    );
    tick(
        1,
        7,
        &mut driver,
        &mut graph,
        &mut store,
        &mut bus,
        &mut systems,
        &mut states,
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
    fn init(&mut self, _: &mut dyn AttrRegistry) -> Result<(), OnyxError> {
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
    let mut bus = TestBus::default();

    tick(
        2,
        1,
        &mut driver,
        &mut graph,
        &mut store,
        &mut bus,
        &mut systems,
        &mut states,
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
    fn init(&mut self, _: &mut dyn AttrRegistry) -> Result<(), OnyxError> {
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
    let first_kind = graph.attr("domain.first");
    let second_kind = graph.attr("domain.second");
    let domain = |kind| {
        OnyxEvent::Domain(DomainEvent {
            kind,
            subject: None,
            data: Vec::new(),
        })
    };
    let mut systems: [Box<dyn OnyxSystem>; 1] = [Box::new(EventSystem {
        seen: Rc::clone(&seen),
        emit: vec![
            domain(first_kind),
            OnyxEvent::System(SystemEvent::SnapshotRequested("saved".into())),
            OnyxEvent::System(SystemEvent::StageLoaded("stage".into())),
            domain(second_kind),
        ],
    })];
    let mut states = [SystemState::Active];
    let mut bus = TestBus::default();

    tick(
        0,
        7,
        &mut driver,
        &mut graph,
        &mut store,
        &mut bus,
        &mut systems,
        &mut states,
    );
    assert_eq!(store.saves, 1);
    assert_eq!(bus.events.len(), 3);
    tick(
        1,
        7,
        &mut driver,
        &mut graph,
        &mut store,
        &mut bus,
        &mut systems,
        &mut states,
    );
    tick(
        2,
        7,
        &mut driver,
        &mut graph,
        &mut store,
        &mut bus,
        &mut systems,
        &mut states,
    );

    let seen = seen.borrow();
    assert!(matches!(seen[1].as_slice(), [
        OnyxEvent::Domain(DomainEvent { kind: first, .. }),
        OnyxEvent::System(SystemEvent::StageLoaded(stage)),
        OnyxEvent::Domain(DomainEvent { kind: second, .. }),
        OnyxEvent::System(SystemEvent::Tick { n: 1 }),
    ] if stage == "stage" && *first == first_kind && *second == second_kind));
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
    let mut bus = TestBus::from(vec![
        restore(),
        OnyxEvent::System(SystemEvent::Start("worker".into())),
    ]);

    tick(
        0,
        7,
        &mut driver,
        &mut graph,
        &mut store,
        &mut bus,
        &mut systems,
        &mut states,
    );

    assert_eq!(states, [SystemState::Active]);
    assert_eq!(
        *log.borrow(),
        [
            "init:worker",
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
        &mut bus,
        &mut systems,
        &mut states,
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
    let mut bus = TestBus::from(vec![
        OnyxEvent::System(SystemEvent::Stop("worker".into())),
        OnyxEvent::System(SystemEvent::Stop("worker".into())),
    ]);
    tick(
        0,
        7,
        &mut driver,
        &mut graph,
        &mut store,
        &mut bus,
        &mut systems,
        &mut states,
    );
    assert_eq!(states, [SystemState::Stopped]);
    bus.events.extend([
        OnyxEvent::System(SystemEvent::Start("worker".into())),
        OnyxEvent::System(SystemEvent::Start("worker".into())),
    ]);
    tick(
        1,
        7,
        &mut driver,
        &mut graph,
        &mut store,
        &mut bus,
        &mut systems,
        &mut states,
    );
    assert_eq!(states, [SystemState::Active]);
    tick(
        2,
        7,
        &mut driver,
        &mut graph,
        &mut store,
        &mut bus,
        &mut systems,
        &mut states,
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
    fn init(&mut self, _: &mut dyn AttrRegistry) -> Result<(), OnyxError> {
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
    fn on_restore(&mut self, _: &dyn GraphView) {
        self.log.borrow_mut().push("restore:fallible".into());
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
        let mut bus = TestBus::from(vec![restore()]);
        tick(
            0,
            7,
            &mut driver,
            &mut graph,
            &mut store,
            &mut bus,
            &mut systems,
            &mut states,
        );
        assert_eq!(states, [SystemState::Blocked]);
        assert!(
            bus.events
                .iter()
                .any(|event| matches!(event, OnyxEvent::System(SystemEvent::Fault(_))))
        );
        let log = log.borrow();
        assert!(!log.iter().any(|entry| entry.starts_with("start:")
            || entry.starts_with("stop:")
            || entry.starts_with("restore:")));
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
    let mut bus = TestBus::from(vec![restore()]);
    bus.events.extend(commands());
    tick(
        0,
        7,
        &mut driver,
        &mut graph,
        &mut store,
        &mut bus,
        &mut systems,
        &mut states,
    );
    drop(bus.drain());
    bus.events.extend(commands());
    tick(
        1,
        7,
        &mut driver,
        &mut graph,
        &mut store,
        &mut bus,
        &mut systems,
        &mut states,
    );

    assert_eq!(states, [SystemState::Blocked]);
    assert!(matches!(
        bus.events.as_slice(),
        [OnyxEvent::System(SystemEvent::Fault(_))]
    ));
    let log = log.borrow();
    assert_eq!(
        log.iter()
            .filter(|entry| entry.as_str() == "run:fallible")
            .count(),
        1
    );
    assert!(!log.iter().any(|entry| entry.starts_with("start:")
        || entry.starts_with("stop:")
        || entry.starts_with("restore:")));
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
    let mut bus = TestBus::from(vec![restore()]);
    tick(
        0,
        7,
        &mut driver,
        &mut graph,
        &mut store,
        &mut bus,
        &mut systems,
        &mut states,
    );
    assert_eq!(states, [SystemState::Stopped]);
    assert_eq!(*log.borrow(), ["init:ok", "present:Some(Int(42))"]);
    tick(
        1,
        7,
        &mut driver,
        &mut graph,
        &mut store,
        &mut bus,
        &mut systems,
        &mut states,
    );
    assert!(
        !log.borrow()
            .iter()
            .any(|entry| entry.as_str() == "run:fallible")
    );
    bus.push(OnyxEvent::System(SystemEvent::Start("fallible".into())));
    tick(
        2,
        7,
        &mut driver,
        &mut graph,
        &mut store,
        &mut bus,
        &mut systems,
        &mut states,
    );
    assert_eq!(states, [SystemState::Active]);
    tick(
        3,
        7,
        &mut driver,
        &mut graph,
        &mut store,
        &mut bus,
        &mut systems,
        &mut states,
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

struct IngressDriver {
    input: Option<OnyxEvent>,
}

impl PlatformDriver for IngressDriver {
    fn poll_into(&mut self, bus: &mut dyn EventBus) {
        if let Some(event) = self.input.take() {
            bus.push(event);
        }
    }
    fn present(&mut self, _: &dyn GraphView) {}
}

#[test]
fn tick_uses_fifo_bus_for_driver_input_and_next_tick_output() {
    use onyx_core::IngressEvent;
    let seen = Rc::new(RefCell::new(Vec::new()));
    let mut graph = TestGraph::new();
    let mut store = TestStore::default();
    let mut driver = IngressDriver {
        input: Some(OnyxEvent::Ingress(IngressEvent::Command("input".into()))),
    };
    let mut systems: [Box<dyn OnyxSystem>; 1] = [Box::new(EventSystem {
        seen: Rc::clone(&seen),
        emit: vec![OnyxEvent::System(SystemEvent::StageLoaded("next".into()))],
    })];
    let mut states = [SystemState::Active];
    let mut bus = TestBus::from(vec![OnyxEvent::System(SystemEvent::StageLoaded(
        "queued".into(),
    ))]);
    tick(
        10,
        7,
        &mut driver,
        &mut graph,
        &mut store,
        &mut bus,
        &mut systems,
        &mut states,
    );
    assert_eq!(bus.drains, 1);
    assert!(!bus.is_empty());
    assert!(matches!(seen.borrow()[0].as_slice(), [
        OnyxEvent::System(SystemEvent::StageLoaded(queued)),
        OnyxEvent::System(SystemEvent::Tick { n: 10 }),
        OnyxEvent::Ingress(IngressEvent::Command(input)),
    ] if queued == "queued" && input == "input"));
    tick(
        11,
        7,
        &mut driver,
        &mut graph,
        &mut store,
        &mut bus,
        &mut systems,
        &mut states,
    );
    assert_eq!(bus.drains, 2);
    assert!(bus.is_empty());
    assert!(matches!(seen.borrow()[1].as_slice(), [
        OnyxEvent::System(SystemEvent::StageLoaded(next)),
        OnyxEvent::System(SystemEvent::Tick { n: 11 }),
    ] if next == "next"));
}

#[test]
fn restore_hook_only_reaches_active_systems_after_successful_init() {
    for previous in [
        SystemState::Active,
        SystemState::Stopped,
        SystemState::Blocked,
    ] {
        for fail in [false, true] {
            let log = Log::default();
            let mut graph = TestGraph::new();
            let mut store = TestStore::default();
            let mut driver = TestDriver(Rc::clone(&log));
            let mut systems: [Box<dyn OnyxSystem>; 1] = [Box::new(FallibleSystem {
                log: Rc::clone(&log),
                fail_init: Rc::new(Cell::new(fail)),
            })];
            let mut states = [previous];
            let mut bus = TestBus::from(vec![restore()]);
            tick(
                0,
                7,
                &mut driver,
                &mut graph,
                &mut store,
                &mut bus,
                &mut systems,
                &mut states,
            );
            let entries = log.borrow();
            let expected = match (previous, fail) {
                (SystemState::Active, false) => vec![
                    "run:fallible",
                    "init:ok",
                    "restore:fallible",
                    "present:Some(Int(42))",
                ],
                (SystemState::Active, true) => {
                    vec!["run:fallible", "init:failed", "present:Some(Int(42))"]
                }
                (_, false) => vec!["init:ok", "present:Some(Int(42))"],
                (_, true) => vec!["init:failed", "present:Some(Int(42))"],
            };
            assert_eq!(*entries, expected, "previous={previous:?}, fail={fail}");
            let expected_state = if fail {
                SystemState::Blocked
            } else if previous == SystemState::Blocked {
                SystemState::Stopped
            } else {
                previous
            };
            assert_eq!(states, [expected_state]);
            assert_eq!(
                bus.events
                    .iter()
                    .filter(|event| matches!(event, OnyxEvent::System(SystemEvent::Fault(_))))
                    .count(),
                usize::from(fail)
            );
            assert_eq!(
                bus.events
                    .iter()
                    .filter(|event| matches!(
                        event,
                        OnyxEvent::System(SystemEvent::SnapshotRestored(_))
                    ))
                    .count(),
                1
            );
        }
    }
}

#[test]
fn each_successful_restore_rebuilds_active_caches_without_starting_again() {
    let log = Log::default();
    let mut graph = TestGraph::new();
    let mut store = TestStore::default();
    let mut driver = TestDriver(Rc::clone(&log));
    let mut systems = [system("worker", &log)];
    let mut states = [SystemState::Active];
    let mut bus = TestBus::default();
    for n in 0..2 {
        bus.push(restore());
        tick(
            n,
            7,
            &mut driver,
            &mut graph,
            &mut store,
            &mut bus,
            &mut systems,
            &mut states,
        );
    }
    let entries = log.borrow();
    assert_eq!(
        entries
            .iter()
            .filter(|entry| entry.as_str() == "restore:worker:Some(Int(42))")
            .count(),
        2
    );
    assert_eq!(
        entries
            .iter()
            .filter(|entry| entry.as_str() == "init:worker")
            .count(),
        2
    );
    assert!(!entries.iter().any(|entry| entry.starts_with("start:")));
    assert_eq!(states, [SystemState::Active]);
}

// Sistema consumidor de prueba: el core no aporta sistemas de negocio.
// La fábrica permite comparar instancias independientes, no la misma caché consigo misma.
#[derive(Default)]
struct CachedIncrement {
    value: Option<Attr>,
}

impl OnyxSystem for CachedIncrement {
    fn name(&self) -> &str {
        "increment"
    }
    fn init(&mut self, attrs: &mut dyn AttrRegistry) -> Result<(), OnyxError> {
        self.value = Some(attrs.attr("value"));
        Ok(())
    }
    fn run(&mut self, ctx: &mut Ctx) {
        let attr = self.value.expect("init antes de run");
        let Some(Value::Int(value)) = ctx.graph.get(ConceptId(1), attr) else {
            panic!("valor entero de prueba")
        };
        let value = Value::Int(value + 1);
        ctx.graph.set(ConceptId(1), attr, value.clone());
        ctx.outbox.push(OnyxEvent::Domain(onyx_core::DomainEvent {
            kind: attr,
            subject: Some(ConceptId(1)),
            data: vec![(attr, value)],
        }));
    }
}

#[test]
fn repeated_init_matches_a_fresh_system_from_the_same_factory() {
    fn observe(
        graph_factory: impl Fn() -> TestGraph,
        system_factory: impl Fn() -> Box<dyn OnyxSystem>,
        repetitions: usize,
    ) -> (Vec<(String, Value)>, Vec<Value>) {
        let mut graph = graph_factory();
        let mut system = system_factory();
        let before = graph.props(ConceptId(1));
        for _ in 0..repetitions {
            system.init(&mut AttrOnly(&mut graph)).unwrap();
            assert_eq!(graph.props(ConceptId(1)), before);
        }
        let mut outbox = Vec::new();
        let mut rng = onyx_core::DetRng::from_seed_and_tick(7, 0);
        system.on_start(&mut graph);
        system.run(&mut Ctx {
            graph: &mut graph,
            inbox: &[],
            outbox: &mut outbox,
            rng: &mut rng,
        });
        let values = outbox
            .into_iter()
            .map(|event| match event {
                OnyxEvent::Domain(event) => {
                    assert_eq!(event.subject, Some(ConceptId(1)));
                    assert_eq!(event.kind, graph.attr("value"));
                    assert_eq!(event.data.len(), 1);
                    assert_eq!(event.data[0].0, graph.attr("value"));
                    event.data.into_iter().next().unwrap().1
                }
                _ => panic!("solo se espera el evento de dominio del incremento"),
            })
            .collect();
        (ReadOnly(&graph).props(ConceptId(1)), values)
    }
    let graph_factory = || {
        let mut graph = TestGraph::new();
        graph.value = Value::Int(41);
        graph
    };
    let system_factory = || Box::new(CachedIncrement::default()) as Box<dyn OnyxSystem>;
    let once = observe(graph_factory, system_factory, 1);
    assert_eq!(
        once,
        (vec![("value".into(), Value::Int(42))], vec![Value::Int(42)])
    );
    assert_eq!(observe(graph_factory, system_factory, 3), once);
}

#[test]
fn start_before_restore_rebuilds_cache_for_next_run_without_second_start() {
    struct CacheReader {
        attr: Option<Attr>,
        cached: Option<Value>,
        log: Log,
    }

    impl OnyxSystem for CacheReader {
        fn name(&self) -> &str {
            "reader"
        }
        fn init(&mut self, attrs: &mut dyn AttrRegistry) -> Result<(), OnyxError> {
            self.attr = Some(attrs.attr("value"));
            Ok(())
        }
        fn on_start(&mut self, graph: &mut dyn Graph) {
            self.cached = graph.get(ConceptId(1), self.attr.unwrap());
            self.log
                .borrow_mut()
                .push(format!("start:{:?}", self.cached));
        }
        fn on_restore(&mut self, graph: &dyn GraphView) {
            self.cached = graph.get(ConceptId(1), self.attr.unwrap());
            self.log
                .borrow_mut()
                .push(format!("restore:{:?}", self.cached));
        }
        fn run(&mut self, _: &mut Ctx) {
            // Consume la caché, no vuelve a consultar el grafo.
            self.log.borrow_mut().push(format!("run:{:?}", self.cached));
        }
    }

    let log = Log::default();
    let mut graph = TestGraph::new();
    let mut store = TestStore::default();
    let mut driver = TestDriver(Rc::clone(&log));
    let mut systems: [Box<dyn OnyxSystem>; 1] = [Box::new(CacheReader {
        attr: None,
        cached: None,
        log: Rc::clone(&log),
    })];
    systems[0].init(&mut AttrOnly(&mut graph)).unwrap();
    let mut states = [SystemState::Stopped];
    let mut bus = TestBus::from(vec![
        OnyxEvent::System(SystemEvent::Start("reader".into())),
        restore(),
    ]);

    tick(
        0,
        7,
        &mut driver,
        &mut graph,
        &mut store,
        &mut bus,
        &mut systems,
        &mut states,
    );
    assert_eq!(states, [SystemState::Active]);
    assert_eq!(
        *log.borrow(),
        [
            "start:Some(Int(0))",
            "restore:Some(Int(42))",
            "present:Some(Int(42))",
        ]
    );
    tick(
        1,
        7,
        &mut driver,
        &mut graph,
        &mut store,
        &mut bus,
        &mut systems,
        &mut states,
    );
    let entries = log.borrow();
    assert_eq!(
        *entries,
        [
            "start:Some(Int(0))",
            "restore:Some(Int(42))",
            "present:Some(Int(42))",
            "run:Some(Int(42))",
            "present:Some(Int(42))",
        ]
    );
    assert_eq!(
        entries
            .iter()
            .filter(|entry| entry.starts_with("start:"))
            .count(),
        1
    );
    assert_eq!(store.loads.get(), 1);
    assert_eq!(states, [SystemState::Active]);
}
