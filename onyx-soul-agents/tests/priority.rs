//! El runtime ordena decisiones por id; ToolSystem consume esas llamadas en
//! el siguiente tick. Las reglas de sobrescribir o reservar son de la app de
//! prueba, no un árbitro de recursos incorporado en el soul.
use std::cell::RefCell;
use std::rc::Rc;

use onyx_soul_agents::{
    AgentBus, AgentConfig, AgentGraph, AgentRuntime, AgentSpawner, AgentSpec, AgentView, Attr,
    AttrOnly, AttrRegistry, ConceptId, Ctx, Decision, DetRng, Graph, GraphView, HeadlessDriver,
    MemStore, OnyxError, OnyxEvent, OnyxSystem, Policy, ScriptedPolicy, SystemState, ToolCall,
    ToolCtx, ToolExecutor, ToolSystem, Value, Vocab, spawn_event, tick,
};

struct UseResource;
impl ScriptedPolicy for UseResource {
    fn decide(&mut self, view: &AgentView, _: &mut DetRng) -> Decision {
        match view.memory.last() {
            None => Decision::Call {
                tool: "resource".into(),
                args: String::new(),
            },
            Some((_, outcome)) => Decision::Done(outcome.clone()),
        }
    }
}

struct Resource {
    id: ConceptId,
    owner: Attr,
    exclusive: bool,
}
impl ToolExecutor for Resource {
    fn name(&self) -> &str {
        "resource"
    }
    fn call(&mut self, call: &ToolCall, ctx: &mut ToolCtx) -> Result<String, String> {
        if self.exclusive && ctx.graph.get(self.id, self.owner).is_some() {
            return Err("occupied".into());
        }
        ctx.graph
            .set(self.id, self.owner, Value::Concept(call.agent));
        Ok("owned".into())
    }
}

type Frames = Rc<RefCell<Vec<Vec<OnyxEvent>>>>;
struct Observe(Frames);
impl OnyxSystem for Observe {
    fn name(&self) -> &str {
        "observe"
    }
    fn init(&mut self, _: &mut dyn AttrRegistry) -> Result<(), OnyxError> {
        Ok(())
    }
    fn run(&mut self, ctx: &mut Ctx) {
        self.0.borrow_mut().push(ctx.outbox.clone());
    }
}

fn compete(exclusive: bool, names: [&str; 2]) {
    let cfg = AgentConfig::default();
    let mut g = AgentGraph::new();
    let v = Vocab::resolve_with(&mut g, &cfg.vocab_prefix);
    let owner = g.attr("app.owner");
    let resource = g.create();
    let frames = Frames::default();
    let mut systems: Vec<Box<dyn OnyxSystem>> = vec![
        Box::new(AgentSpawner::new(cfg.clone())),
        Box::new(AgentRuntime::new(cfg.clone()).with_scripted(Box::new(UseResource))),
        Box::new(ToolSystem::new(cfg).with_tool(Box::new(Resource {
            id: resource,
            owner,
            exclusive,
        }))),
        Box::new(Observe(Rc::clone(&frames))),
    ];
    for system in &mut systems {
        system.init(&mut AttrOnly(&mut g)).unwrap();
    }
    let mut bus = AgentBus::new();
    use onyx_soul_agents::EventBus;
    for name in names {
        let spec = AgentSpec::new(name, "share")
            .with_policy(Policy::Scripted)
            .with_tools(["resource"]);
        bus.push(spawn_event(&v, &spec, None));
    }
    let mut store = MemStore::new();
    let mut driver = HeadlessDriver::new();
    let mut states = vec![SystemState::Active; systems.len()];
    tick(
        0,
        7,
        &mut driver,
        &mut g,
        &mut store,
        &mut bus,
        &mut systems,
        &mut states,
        &[],
    );
    let agents = g.query(&[v.state]);
    assert_eq!(agents.len(), 2);
    let [low, high] = [agents[0], agents[1]];
    assert!(low.0 < high.0);
    assert_eq!(g.get(low, v.name), Some(Value::Text(names[0].into())));
    assert_eq!(g.get(high, v.name), Some(Value::Text(names[1].into())));
    // La petición salió, pero ToolSystem aún no la ha recibido.
    assert_eq!(g.get(resource, owner), None);
    let subjects = |events: &[OnyxEvent], kind| -> Vec<ConceptId> {
        events
            .iter()
            .filter_map(|event| match event {
                OnyxEvent::Domain(d) if d.kind == kind => d.subject,
                _ => None,
            })
            .collect()
    };
    assert_eq!(
        subjects(&frames.borrow()[0], v.ev_tool_call),
        vec![low, high]
    );
    tick(
        1,
        7,
        &mut driver,
        &mut g,
        &mut store,
        &mut bus,
        &mut systems,
        &mut states,
        &[],
    );
    assert_eq!(
        g.get(resource, owner),
        Some(Value::Concept(if exclusive { low } else { high }))
    );
    let outcomes: Vec<_> = frames.borrow()[1]
        .iter()
        .filter_map(|event| match event {
            OnyxEvent::Domain(d) if d.kind == v.ev_tool_result => {
                let ok = d
                    .data
                    .iter()
                    .find(|(attr, _)| *attr == v.ok)
                    .unwrap()
                    .1
                    .clone();
                let text = d
                    .data
                    .iter()
                    .find(|(attr, _)| *attr == v.text)
                    .unwrap()
                    .1
                    .clone();
                Some((d.subject, ok, text))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        outcomes,
        vec![
            (Some(low), Value::Bool(true), Value::Text("owned".into())),
            (
                Some(high),
                Value::Bool(!exclusive),
                Value::Text(if exclusive { "occupied" } else { "owned" }.into())
            ),
        ]
    );
    tick(
        2,
        7,
        &mut driver,
        &mut g,
        &mut store,
        &mut bus,
        &mut systems,
        &mut states,
        &[],
    );
    assert_eq!(
        subjects(&frames.borrow()[2], v.ev_finished),
        vec![low, high]
    );
    assert_eq!(g.get(low, v.result), Some(Value::Text("owned".into())));
    assert_eq!(
        g.get(high, v.result),
        Some(Value::Text(
            if exclusive { "occupied" } else { "owned" }.into()
        ))
    );
}

#[test]
fn higher_agent_id_wins_overwriting_the_same_attribute() {
    for names in [["z", "a"], ["a", "z"]] {
        compete(false, names);
    }
}

#[test]
fn lower_agent_id_claims_an_exclusive_resource_first() {
    for names in [["z", "a"], ["a", "z"]] {
        compete(true, names);
    }
}
