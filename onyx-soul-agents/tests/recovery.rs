//! Record/replay recovery through the public host, with graph-owned tool state.
// External tool results belong to the separate TR2 acceptance tests.
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use onyx_soul_agents::{
    AgentConfig, AgentHost, AgentSpec, AgentState, AgentView, Attr, AttrRegistry, Ctx, EventBus,
    Fingerprint, GraphView, HeadlessDriver, IngressEvent, OnyxError, OnyxEvent, OnyxSystem,
    PlatformDriver, RecordingBridge, ReplayBridge, Reply, SnapshotStore, SurfaceBinding,
    SystemEvent, ToolCall, ToolCtx, ToolExecutor, Transcript, Value, Vocab,
};

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

struct ScriptRng(u64);
impl ScriptRng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1);
        self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Frame {
    graph: Fingerprint,
    inbox: Vec<String>,
    outbox: Vec<String>,
    presented: Fingerprint,
    tool_calls: bool,
}
#[derive(Default)]
struct Log {
    inbox: Vec<String>,
    outbox: Vec<OnyxEvent>,
    presented: Fingerprint,
    restored: usize,
}

// present sees the same graph as the facade, without gaining write access.
struct Presentation(Rc<RefCell<Log>>);
impl PlatformDriver for Presentation {
    fn poll(&mut self, _: &mut dyn EventBus) {}
    fn present(&mut self, graph: &dyn GraphView, _: &[SurfaceBinding]) {
        self.0.borrow_mut().presented = graph
            .query(&[])
            .into_iter()
            .map(|id| (id.0, graph.props(id), graph.links(id)))
            .collect();
    }
}

#[derive(Default)]
struct Counter {
    attrs: Option<[Attr; 4]>,
}
impl ToolExecutor for Counter {
    fn name(&self) -> &str {
        "count"
    }
    fn init(&mut self, r: &mut dyn AttrRegistry) {
        self.attrs =
            Some(["app.count", "app.last_request", "app.owner", "app.backlink"].map(|s| r.attr(s)));
    }
    fn call(&mut self, call: &ToolCall, ctx: &mut ToolCtx) -> Result<String, String> {
        let [count, last_request, owner, backlink] = self.attrs.unwrap();
        let old = match ctx.graph.get(call.agent, count) {
            Some(Value::Int(n)) => n,
            _ => 0,
        };
        // The application owns this idempotency key in the graph, not in executor memory.
        if ctx.graph.get(call.agent, last_request) == Some(Value::Int(call.request)) {
            return Ok(old.to_string());
        }
        ctx.graph.set(call.agent, count, Value::Int(old + 1));
        ctx.graph
            .set(call.agent, last_request, Value::Int(call.request));
        let witness = ctx.graph.create();
        ctx.graph.set(witness, owner, Value::Concept(call.agent));
        ctx.graph.relate(witness, backlink, call.agent);
        Ok((old + 1).to_string())
    }
}

struct Probe {
    vocab: Option<Vocab>,
    salt: Option<Attr>,
    log: Rc<RefCell<Log>>,
}
impl OnyxSystem for Probe {
    fn name(&self) -> &str {
        "recovery_probe"
    }
    fn init(&mut self, r: &mut dyn AttrRegistry) -> Result<(), OnyxError> {
        self.vocab = Some(Vocab::resolve(r));
        self.salt = Some(r.attr("app.salt"));
        Ok(())
    }
    fn run(&mut self, ctx: &mut Ctx) {
        // A new host's restore step has no agents and is not part of the suffix.
        let v = self.vocab.unwrap();
        let n = ctx
            .inbox
            .iter()
            .find_map(|e| match e {
                OnyxEvent::System(SystemEvent::Tick { n }) => Some(*n),
                _ => None,
            })
            .unwrap();
        let agents = ctx.graph.query(&[v.state]);
        for id in &agents {
            ctx.graph.set(
                *id,
                self.salt.unwrap(),
                Value::Int(i64::from_le_bytes(ctx.rng.next_u64().to_le_bytes())),
            );
        }
        if n == 24 {
            if let Some(id) = agents.first() {
                ctx.graph.destroy(*id);
            }
        }
        let mut log = self.log.borrow_mut();
        // Only recovery bookkeeping is filtered. Faults and all domain/ingress
        // events stay in the comparison, with their original order and payload.
        log.inbox = ctx
            .inbox
            .iter()
            .filter(|e| {
                !matches!(
                    e,
                    OnyxEvent::System(
                        SystemEvent::SnapshotRequested(_)
                            | SystemEvent::RestoreRequested(_)
                            | SystemEvent::SnapshotRestored(_)
                    )
                )
            })
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

fn host(seed: u64, keep: usize, store: Store, log: Rc<RefCell<Log>>) -> AgentHost {
    AgentHost::builder(seed)
        .config(AgentConfig {
            memory_keep_last: keep,
            ..AgentConfig::default()
        })
        .store(Box::new(store))
        .tool(Box::new(Counter::default()))
        .system(Box::new(Probe {
            vocab: None,
            salt: None,
            log,
        }))
        .build()
        .unwrap()
}

fn respond(req: &AgentView) -> Option<Reply> {
    Some(match req.request {
        1 | 3 => Reply::say(
            req.agent,
            req.request,
            format!("{}:{}", req.goal, req.request),
        ),
        2 | 4 => Reply::call(req.agent, req.request, "count next"),
        _ => Reply::done(req.agent, req.request, "complete"),
    })
}

fn frame(host: &AgentHost, log: &Rc<RefCell<Log>>) -> Frame {
    let log = log.borrow();
    Frame {
        graph: host.fingerprint(),
        inbox: log.inbox.clone(),
        outbox: log.outbox.iter().map(|e| format!("{e:?}")).collect(),
        presented: log.presented.clone(),
        tool_calls: log.outbox.iter().any(|event| {
            matches!(event,
            OnyxEvent::Domain(d) if d.kind == host.vocab().ev_tool_call)
        }),
    }
}

fn input(host: &mut AgentHost, rng: &mut ScriptRng) {
    host.push(OnyxEvent::Ingress(IngressEvent::Command(
        rng.next().to_string(),
    )));
    host.request_snapshot("cut");
}

fn record(seed: u64, count: usize, keep: usize) -> (Vec<Frame>, Transcript) {
    record_ticks(seed, count, keep, 30)
}

fn record_ticks(seed: u64, count: usize, keep: usize, ticks: usize) -> (Vec<Frame>, Transcript) {
    let log = Rc::new(RefCell::new(Log::default()));
    let mut host = host(seed, keep, Store::default(), log.clone());
    let mut rng = ScriptRng(seed);
    for i in 0..count {
        host.spawn(
            &AgentSpec::new(format!("agent-{i}"), rng.next().to_string())
                .with_tools(["count"])
                .with_max_steps(10)
                .with_timeout_ticks(12),
        );
    }
    let mut bridge =
        RecordingBridge::new(host.vocab(), respond).wrapping(Presentation(log.clone()));
    let mut frames = Vec::new();
    for n in 0..ticks {
        input(&mut host, &mut rng);
        host.step(&mut bridge);
        if n == 23 {
            assert_eq!(host.agents().len(), count);
            let counter = host.attr("app.count");
            for id in host.agents() {
                assert_eq!(host.agent_state(id), Some(AgentState::Done));
                assert_eq!(host.graph().get(id, counter), Some(Value::Int(2)));
                assert_eq!(host.view(id).memory.len(), if keep == 0 { 6 } else { keep });
                assert_eq!(host.result(id).as_deref(), Some("complete"));
            }
        }
        frames.push(frame(&host, &log));
    }
    (frames, bridge.transcript().clone())
}

struct Recovery<'a> {
    cuts: &'a [u64],
    reinsert: bool,
    align: bool,
    seed: u64,
}

fn replay(
    seed: u64,
    count: usize,
    keep: usize,
    transcript: Transcript,
    recovery: Recovery<'_>,
) -> Vec<Frame> {
    replay_ticks(seed, count, keep, transcript, recovery, 30)
}

fn replay_ticks(
    seed: u64,
    count: usize,
    keep: usize,
    transcript: Transcript,
    recovery: Recovery<'_>,
    ticks: usize,
) -> Vec<Frame> {
    let Recovery {
        cuts,
        reinsert,
        align,
        seed: resume_seed,
    } = recovery;
    let store = Store::default();
    let mut log = Rc::new(RefCell::new(Log::default()));
    let mut host = host(seed, keep, store.clone(), log.clone());
    let mut rng = ScriptRng(seed);
    for i in 0..count {
        host.spawn(
            &AgentSpec::new(format!("agent-{i}"), rng.next().to_string())
                .with_tools(["count"])
                .with_max_steps(10)
                .with_timeout_ticks(12),
        );
    }
    let mut bridge = ReplayBridge::new(transcript.clone()).wrapping(Presentation(log.clone()));
    let mut frames = Vec::new();
    let mut restored_last_step = false;
    for n in 0..u64::try_from(ticks).unwrap() {
        input(&mut host, &mut rng);
        host.step(&mut bridge);
        if restored_last_step {
            assert_eq!(log.borrow().restored, 1);
        }
        frames.push(frame(&host, &log));
        restored_last_step = cuts.contains(&n);
        if cuts.contains(&n) {
            let pending = log.borrow().outbox.clone();
            log = Rc::new(RefCell::new(Log::default()));
            host = self::host(resume_seed, keep, store.clone(), log.clone());
            host.request_restore("cut");
            host.step(&mut HeadlessDriver::new());
            assert_eq!(
                host.fingerprint(),
                frames.last().unwrap().graph,
                "restored boundary, seed={seed}, agents={count}, keep={keep}, cut={n}"
            );
            if align {
                host.set_tick(n + 1);
            }
            if reinsert {
                for event in pending {
                    host.push(event);
                }
            }
            bridge = ReplayBridge::new(transcript.clone())
                .starting_at(n + 1)
                .wrapping(Presentation(log.clone()));
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

fn case_fails(seed: u64, count: usize, keep: usize, cuts: &[u64], ticks: usize) -> bool {
    std::panic::catch_unwind(|| {
        let (baseline, transcript) = record_ticks(seed, count, keep, ticks);
        replay_ticks(
            seed,
            count,
            keep,
            transcript,
            Recovery {
                cuts,
                reinsert: true,
                align: true,
                seed,
            },
            ticks,
        ) != baseline
    })
    .unwrap_or(true)
}

fn check_cut(
    seed: u64,
    count: usize,
    keep: usize,
    cuts: &[u64],
    baseline: &[Frame],
    transcript: &Transcript,
) {
    let result = std::panic::catch_unwind(|| {
        replay(
            seed,
            count,
            keep,
            transcript.clone(),
            Recovery {
                cuts,
                reinsert: true,
                align: true,
                seed,
            },
        )
    });
    if result
        .as_ref()
        .is_ok_and(|frames| frames.as_slice() == baseline)
    {
        return;
    }
    let smallest_count = (1..count)
        .find(|n| case_fails(seed, *n, keep, cuts, 30))
        .unwrap_or(count);
    let minimum_ticks = usize::try_from(cuts.iter().max().copied().unwrap_or(0) + 1).unwrap();
    let smallest_ticks = (minimum_ticks..=30)
        .find(|ticks| case_fails(seed, smallest_count, keep, cuts, *ticks))
        .unwrap();
    if let Ok(actual) = result {
        let case = format!(
            "seed={seed}, original_count={count}, reduced_count={smallest_count}, reduced_ticks={smallest_ticks}, cuts={cuts:?}"
        );
        assert_frames(&actual, baseline, &case);
    }
    panic!(
        "recovery failed: seed={seed}, agents={smallest_count}, keep={keep}, ticks={smallest_ticks}, cuts={cuts:?}"
    );
}

#[test]
fn every_cut_replays_graph_owned_tools_memory_events_and_present() {
    for seed in [0, 1, u64::MAX] {
        for count in [1, 3, 16] {
            for keep in [0, 2] {
                let (baseline, transcript) = record(seed, count, keep);
                assert_frames(
                    &replay(
                        seed,
                        count,
                        keep,
                        transcript.clone(),
                        Recovery {
                            cuts: &[],
                            reinsert: true,
                            align: true,
                            seed,
                        },
                    ),
                    &baseline,
                    &format!("replay without restore, seed={seed}, agents={count}, keep={keep}"),
                );
                for cut in 0..29 {
                    check_cut(seed, count, keep, &[cut], &baseline, &transcript);
                }
                for cuts in [[0, 1], [2, 14], [10, 23]] {
                    check_cut(seed, count, keep, &cuts, &baseline, &transcript);
                }
            }
        }
    }
}

#[test]
fn restoring_without_pending_tool_calls_loses_work_but_quiet_cuts_do_not() {
    let (baseline, transcript) = record(7, 3, 2);
    // Derive the witness cut from a REAL emitted tool call, not a guessed tick.
    let call_cut = baseline.iter().position(|f| f.tool_calls).unwrap();
    let quiet_cut = baseline.iter().position(|f| f.outbox.is_empty()).unwrap();
    assert_ne!(
        replay(
            7,
            3,
            2,
            transcript.clone(),
            Recovery {
                cuts: &[u64::try_from(call_cut).unwrap()],
                reinsert: false,
                align: true,
                seed: 7
            }
        ),
        baseline
    );
    assert_frames(
        &replay(
            7,
            3,
            2,
            transcript,
            Recovery {
                cuts: &[u64::try_from(quiet_cut).unwrap()],
                reinsert: false,
                align: true,
                seed: 7,
            },
        ),
        &baseline,
        "quiet cut without pending events",
    );
}

#[test]
fn restoring_with_the_wrong_tick_or_seed_is_observable() {
    let (baseline, transcript) = record(7, 3, 2);
    assert_ne!(
        replay(
            7,
            3,
            2,
            transcript.clone(),
            Recovery {
                cuts: &[4],
                reinsert: true,
                align: false,
                seed: 7
            }
        ),
        baseline
    );
    assert_ne!(
        replay(
            7,
            3,
            2,
            transcript,
            Recovery {
                cuts: &[4],
                reinsert: true,
                align: true,
                seed: 8
            }
        ),
        baseline
    );
}

#[test]
fn restored_notifications_do_not_change_the_soul_graph_or_emitted_events() {
    let (baseline, transcript) = record(7, 3, 2);
    let log = Rc::new(RefCell::new(Log::default()));
    let mut host = host(7, 2, Store::default(), log.clone());
    let mut rng = ScriptRng(7);
    for i in 0..3 {
        host.spawn(
            &AgentSpec::new(format!("agent-{i}"), rng.next().to_string())
                .with_tools(["count"])
                .with_max_steps(10)
                .with_timeout_ticks(12),
        );
    }
    let mut bridge = ReplayBridge::new(transcript).wrapping(Presentation(log.clone()));
    for (n, expected) in baseline.iter().enumerate() {
        input(&mut host, &mut rng);
        host.push(OnyxEvent::System(SystemEvent::SnapshotRestored(
            "notice-only".into(),
        )));
        host.step(&mut bridge);
        assert_eq!(&frame(&host, &log), expected, "notice changed tick {n}");
        assert_eq!(log.borrow().restored, n + 1);
    }
}
