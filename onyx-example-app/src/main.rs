use onyx_soul::{
    event::{IngressEvent, OnyxEvent},
    graph::{Attr, ConceptId, Value, AttrRegistry, OnyxError},
    system::{Ctx, OnyxSystem, StageBlueprint, SystemSpec},
    tick,
    FileSnapshotStore, HeadlessDriver, InMemoryBus,
    InMemorySystemRegistry, RustLoader, Supervisor, WorldGraph,
};

struct BootSystem {
    name_attr: Attr,
    clearance_attr: Attr,
    player: Option<ConceptId>,
}

impl BootSystem {
    fn new() -> Self {
        Self {
            name_attr: Attr::from_raw(0),
            clearance_attr: Attr::from_raw(0),
            player: None,
        }
    }
}

impl OnyxSystem for BootSystem {
    fn name(&self) -> &str {
        "boot"
    }

    fn init(&mut self, attrs: &mut dyn onyx_core::graph::AttrRegistry) -> Result<(), onyx_core::OnyxError> {
        self.name_attr = attrs.attr("name");
        self.clearance_attr = attrs.attr("clearance_level");
        Ok(())
    }

    fn run(&mut self, ctx: &mut Ctx) {
        // Crear jugador la primera vez
        if self.player.is_none() {
            let id = ctx.graph.create();
            ctx.graph.set(id, self.name_attr, Value::Text("engineer_2233".into()));
            ctx.graph.set(id, self.clearance_attr, Value::Int(0));
            self.player = Some(id);
            println!("[BOOT] Esperando 'agree'...");
        }

        for event in ctx.inbox {
            if let OnyxEvent::Ingress(IngressEvent::Command(cmd)) = event {
                if cmd.trim().eq_ignore_ascii_case("agree") {
                    if let Some(id) = self.player {
                        ctx.graph.set(id, self.clearance_attr, Value::Int(1));
                        println!("[BOOT] NDA aceptado. Clearance = 1");
                        // Aquí podrías emitir un evento para cambiar de stage
                    }
                } else {
                    println!("[BOOT] Comando no reconocido: {}", cmd);
                }
            }
        }
    }
}

fn make_boot(_: &SystemSpec) -> Result<Box<dyn OnyxSystem>, onyx_core::OnyxError> {
    Ok(Box::new(BootSystem::new()))
}

fn main() {
    let mut graph = WorldGraph::new();
    let mut bus = InMemoryBus::new();
    let mut driver = HeadlessDriver::new();
    let mut store = FileSnapshotStore::new("./snapshots").unwrap();
    let mut registry = InMemorySystemRegistry::new();
    let mut loader = RustLoader::new();

    registry.register("boot", make_boot);

    loader.register(StageBlueprint {
        name: "boot".into(),
        systems: vec![SystemSpec {
            kind: "boot".into(),
            params: vec![],
            persistent: true,
        }],
        entities: vec![],
    });

    let bp = loader.load("boot").unwrap();
    let mut systems: Vec<Box<dyn OnyxSystem>> = bp
        .systems
        .iter()
        .map(|spec| registry.build(spec).unwrap())
        .collect();

    let mut supervisor = Supervisor::new(systems.len());
    supervisor.init_all(&mut systems, &mut graph);
    supervisor.start(0); // activa el boot

    // Simulamos entrada
    driver.push_event(OnyxEvent::Ingress(IngressEvent::Command("agree".into())));

    let seed = 42u64;
    tick(
        0,
        seed,
        &mut driver,
        &mut graph,
        &mut store,
        &mut bus,
        &mut systems,
        supervisor.states_mut(),
        &[], // surfaces vacías por ahora
    );

    println!("Tick ejecutado. Revisa el grafo o añade más lógica.");
}
