//! Recovery within one compatible configuration. Snapshots never include the bus.
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use onyx_soul::event::{DomainEvent, IngressEvent, OnyxEvent, SystemEvent};
use onyx_soul::system::{Ctx, OnyxSystem, SystemState};
use onyx_soul::{
    Attr, AttrRegistry, ConceptId, GraphView, HeadlessDriver, InMemoryBus, OnyxError,
    SnapshotStore, Value, WorldGraph, tick,
};

type Fingerprint = Vec<(u64, Vec<(String, Value)>, Vec<(String, ConceptId)>)>;
type Blobs = Rc<RefCell<BTreeMap<String, Vec<u8>>>>;

#[derive(Clone, Default)]
struct Store(Blobs);
impl SnapshotStore for Store {
    fn save(&mut self, key: &str, bytes: &[u8]) -> Result<(), OnyxError> {
        self.0.borrow_mut().insert(key.into(), bytes.to_vec());
        Ok(())
    }
    fn load(&self, key: &str) -> Result<Vec<u8>, OnyxError> {
        self.0.borrow().get(key).cloned().ok_or(OnyxError::Missing)
    }
}

// Independent generator for ingress; it does not share the runtime's PRNG.
struct ScriptRng(u64);
impl ScriptRng {
    fn next(&mut self) -> i64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1);
        i64::from_le_bytes(self.0.to_le_bytes())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Frame {
    graph: Fingerprint,
    inbox: Vec<String>,
    outbox: Vec<String>,
}

#[derive(Default)]
struct Log {
    inbox: Vec<String>,
    outbox: Vec<OnyxEvent>,
    restored: usize,
}

fn visible(event: &OnyxEvent) -> bool {
    !matches!(
        event,
        OnyxEvent::System(
            SystemEvent::SnapshotRequested(_)
                | SystemEvent::RestoreRequested(_)
                | SystemEvent::SnapshotRestored(_)
        )
    )
}

struct Workload {
    count: usize,
    attrs: Option<[Attr; 7]>,
    log: Rc<RefCell<Log>>,
}
impl OnyxSystem for Workload {
    fn name(&self) -> &str {
        "workload"
    }
    fn init(&mut self, r: &mut dyn AttrRegistry) -> Result<(), OnyxError> {
        self.attrs = Some(
            [
                "node",
                "value",
                "target",
                "backlink",
                "reference",
                "echo",
                "ingress",
            ]
            .map(|s| r.attr(s)),
        );
        Ok(())
    }
    fn run(&mut self, ctx: &mut Ctx) {
        // The restore step runs systems BEFORE import. It is not a continuation tick.
        if ctx
            .inbox
            .iter()
            .any(|e| matches!(e, OnyxEvent::System(SystemEvent::RestoreRequested(_))))
        {
            return;
        }
        let [node, value, target, backlink, reference, echo, ingress] = self.attrs.unwrap();
        let n = ctx
            .inbox
            .iter()
            .find_map(|e| match e {
                OnyxEvent::System(SystemEvent::Tick { n }) => Some(*n),
                _ => None,
            })
            .unwrap();
        if n == 0 {
            for _ in 0..self.count {
                let id = ctx.graph.create();
                ctx.graph.set(id, node, Value::Bool(true));
            }
        }
        for id in ctx.graph.query(&[node]) {
            ctx.graph.set(
                id,
                value,
                Value::Int(i64::from_le_bytes(ctx.rng.next_u64().to_le_bytes())),
            );
            for event in ctx.inbox {
                match event {
                    OnyxEvent::Domain(d) if d.kind == echo => {
                        ctx.graph.set(id, echo, d.data[0].1.clone());
                    }
                    OnyxEvent::Ingress(IngressEvent::Command(text)) => {
                        ctx.graph.set(id, ingress, Value::Text(text.clone()));
                    }
                    _ => {}
                }
            }
        }
        // Each next tick destroys a snapshotted target with TWO kinds of incoming reference.
        for old in ctx.graph.query(&[target]) {
            ctx.graph.destroy(old);
        }
        let to = ctx.graph.create();
        let from = ctx.graph.create();
        ctx.graph.set(to, target, Value::Bool(true));
        ctx.graph.set(from, reference, Value::Concept(to));
        ctx.graph.relate(from, backlink, to);
        if n % 4 != 3 {
            ctx.outbox.push(OnyxEvent::Domain(DomainEvent {
                kind: echo,
                subject: None,
                data: vec![(value, Value::Int(i64::try_from(n).unwrap()))],
            }));
        }
        let mut log = self.log.borrow_mut();
        log.inbox = ctx
            .inbox
            .iter()
            .filter(|e| visible(e))
            .map(|e| format!("{e:?}"))
            .collect();
        log.outbox = ctx.outbox.clone();
        log.restored += ctx
            .inbox
            .iter()
            .filter(|e| matches!(e, OnyxEvent::System(SystemEvent::SnapshotRestored(_))))
            .count();
    }
}

struct App {
    seed: u64,
    n: u64,
    graph: WorldGraph,
    bus: InMemoryBus,
    store: Store,
    systems: Vec<Box<dyn OnyxSystem>>,
    states: Vec<SystemState>,
    log: Rc<RefCell<Log>>,
}
impl App {
    fn new(seed: u64, count: usize, store: Store) -> Self {
        let log = Rc::new(RefCell::new(Log::default()));
        let mut graph = WorldGraph::new();
        let mut system = Workload {
            count,
            attrs: None,
            log: log.clone(),
        };
        system.init(&mut graph).unwrap();
        Self {
            seed,
            n: 0,
            graph,
            bus: InMemoryBus::new(),
            store,
            systems: vec![Box::new(system)],
            states: vec![SystemState::Active],
            log,
        }
    }
    fn fingerprint(&self) -> Fingerprint {
        self.graph
            .query(&[])
            .into_iter()
            .map(|id| (id.0, self.graph.props(id), self.graph.links(id)))
            .collect()
    }
    fn step(&mut self, driver: &mut HeadlessDriver) -> Frame {
        tick(
            self.n,
            self.seed,
            driver,
            &mut self.graph,
            &mut self.store,
            &mut self.bus,
            &mut self.systems,
            &mut self.states,
            &[],
        );
        self.n += 1;
        let log = self.log.borrow();
        Frame {
            graph: self.fingerprint(),
            inbox: log.inbox.clone(),
            outbox: log.outbox.iter().map(|e| format!("{e:?}")).collect(),
        }
    }
    fn restore(&self, count: usize, reinsert: bool, align: bool, seed: u64) -> Self {
        let mut restored = Self::new(seed, count, self.store.clone());
        // Seed the new bus via an outbox during the excluded restore step.
        // This retains FIFO order relative to the next Tick, without importing core.
        if reinsert {
            restored
                .systems
                .push(Box::new(Reinsert(Some(self.log.borrow().outbox.clone()))));
            restored.states.push(SystemState::Active);
        }
        let mut driver = HeadlessDriver::new();
        driver.push_event(OnyxEvent::System(SystemEvent::RestoreRequested(
            "cut".into(),
        )));
        restored.step(&mut driver);
        assert_eq!(
            restored.fingerprint(),
            self.fingerprint(),
            "restored boundary, seed={}, next_tick={}",
            self.seed,
            self.n
        );
        if reinsert {
            restored.systems.pop();
            restored.states.pop();
        }
        if align {
            restored.n = self.n;
        }
        restored
    }
}

struct Reinsert(Option<Vec<OnyxEvent>>);
impl OnyxSystem for Reinsert {
    fn name(&self) -> &str {
        "recovery_pending"
    }
    fn init(&mut self, _: &mut dyn AttrRegistry) -> Result<(), OnyxError> {
        Ok(())
    }
    fn run(&mut self, ctx: &mut Ctx) {
        ctx.outbox.extend(self.0.take().unwrap_or_default());
    }
}

fn run(
    seed: u64,
    count: usize,
    cuts: &[u64],
    reinsert: bool,
    align: bool,
    restored_seed: u64,
) -> Vec<Frame> {
    run_ticks(seed, count, cuts, reinsert, align, restored_seed, 30)
}

fn run_ticks(
    seed: u64,
    count: usize,
    cuts: &[u64],
    reinsert: bool,
    align: bool,
    restored_seed: u64,
    ticks: usize,
) -> Vec<Frame> {
    let mut app = App::new(seed, count, Store::default());
    let mut script = ScriptRng(seed);
    let mut frames = Vec::new();
    let mut restored_last_step = false;
    for n in 0..u64::try_from(ticks).unwrap() {
        let mut driver = HeadlessDriver::new();
        driver.push_event(OnyxEvent::Ingress(IngressEvent::Command(
            script.next().to_string(),
        )));
        driver.push_event(OnyxEvent::System(SystemEvent::SnapshotRequested(
            "cut".into(),
        )));
        frames.push(app.step(&mut driver));
        if restored_last_step {
            assert_eq!(app.log.borrow().restored, 1);
        }
        restored_last_step = cuts.contains(&n);
        if cuts.contains(&n) {
            app = app.restore(count, reinsert, align, restored_seed);
        }
    }
    frames
}

fn assert_frames(actual: &[Frame], expected: &[Frame], case: &str) {
    assert_eq!(
        actual.len(),
        expected.len(),
        "{case}: missing continuation ticks"
    );
    for (n, (a, b)) in actual.iter().zip(expected).enumerate() {
        assert_eq!(a, b, "{case}, first divergent tick={n}");
    }
}

// Shrinking is test-only and runs only on a failing generated case. A panic
// at the restore boundary also counts as failure, never as a passing case.
fn case_fails(seed: u64, count: usize, cuts: &[u64], ticks: usize) -> bool {
    std::panic::catch_unwind(|| {
        let baseline = run_ticks(seed, count, &[], true, true, seed, ticks);
        run_ticks(seed, count, cuts, true, true, seed, ticks) != baseline
    })
    .unwrap_or(true)
}

fn check_cut(seed: u64, count: usize, cuts: &[u64], baseline: &[Frame]) {
    let result = std::panic::catch_unwind(|| run(seed, count, cuts, true, true, seed));
    if result
        .as_ref()
        .is_ok_and(|frames| frames.as_slice() == baseline)
    {
        return;
    }
    let smallest_count = (1..count)
        .find(|n| case_fails(seed, *n, cuts, 30))
        .unwrap_or(count);
    let minimum_ticks = usize::try_from(cuts.iter().max().copied().unwrap_or(0) + 1).unwrap();
    let smallest_ticks = (minimum_ticks..=30)
        .find(|ticks| case_fails(seed, smallest_count, cuts, *ticks))
        .unwrap();
    if let Ok(actual) = result {
        let case = format!(
            "seed={seed}, original_count={count}, reduced_count={smallest_count}, reduced_ticks={smallest_ticks}, cuts={cuts:?}"
        );
        assert_frames(&actual, baseline, &case);
    }
    panic!(
        "recovery failed: seed={seed}, nodes={smallest_count}, ticks={smallest_ticks}, cuts={cuts:?}"
    );
}

#[test]
fn every_single_and_selected_double_cut_match_uninterrupted_execution() {
    for seed in [0, 1, u64::MAX] {
        for count in [1, 3, 16] {
            let baseline = run(seed, count, &[], true, true, seed);
            for cut in 0..29 {
                check_cut(seed, count, &[cut], &baseline);
            }
            for cuts in [[0, 1], [2, 14], [10, 28]] {
                check_cut(seed, count, &cuts, &baseline);
            }
        }
    }
}

#[test]
fn pending_events_are_not_in_the_snapshot_and_only_matter_when_emitted() {
    let baseline = run(7, 3, &[], true, true, 7);
    assert_ne!(run(7, 3, &[2], false, true, 7), baseline);
    assert_frames(&run(7, 3, &[3], false, true, 7), &baseline, "quiet cut");
}

#[test]
fn wrong_tick_or_seed_breaks_continuation() {
    let baseline = run(7, 3, &[], true, true, 7);
    assert_ne!(run(7, 3, &[4], true, false, 7), baseline);
    assert_ne!(run(7, 3, &[4], true, true, 8), baseline);
}

#[test]
fn restored_notification_does_not_replace_or_mutate_the_application_state() {
    let baseline = run(7, 3, &[], true, true, 7);
    let mut app = App::new(7, 3, Store::default());
    let mut rng = ScriptRng(7);
    for (n, expected) in baseline.iter().enumerate() {
        let mut driver = HeadlessDriver::new();
        driver.push_event(OnyxEvent::Ingress(IngressEvent::Command(
            rng.next().to_string(),
        )));
        driver.push_event(OnyxEvent::System(SystemEvent::SnapshotRequested(
            "cut".into(),
        )));
        driver.push_event(OnyxEvent::System(SystemEvent::SnapshotRestored(
            "notice-only".into(),
        )));
        assert_eq!(&app.step(&mut driver), expected, "notice changed tick {n}");
        assert_eq!(app.log.borrow().restored, n + 1);
    }
}
