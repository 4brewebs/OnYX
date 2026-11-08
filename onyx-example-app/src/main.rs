use std::io::{self, Write};
use onyx_soul::{
    event::{IngressEvent, OnyxEvent},
    system::{
        BlueprintLoader, Ctx, OnyxSystem, StageBlueprint, SystemRegistry, SystemSpec,
    },
    tick, Attr, AttrRegistry, ConceptId, HeadlessDriver, InMemoryBus,
    InMemorySystemRegistry, OnyxError, RustLoader, Supervisor, Value, WorldGraph,
    Graph, SnapshotStore // Importados limpiamente a través de soul
};
use onyx_soul::store::FileSnapshotStore;

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

    fn init(&mut self, attrs: &mut dyn AttrRegistry) -> Result<(), OnyxError> {
        self.name_attr = attrs.attr("name");
        self.clearance_attr = attrs.attr("clearance_level");
        Ok(())
    }

    fn run(&mut self, ctx: &mut Ctx) {
        if self.player.is_none() {
            let id = ctx.graph.create();
            ctx.graph.set(id, self.name_attr, Value::Text("engineer_2233".into()));
            ctx.graph.set(id, self.clearance_attr, Value::Int(0));
            self.player = Some(id);
            println!("[BOOT] System online. Awaiting 'agree' command...");
        }

        for event in ctx.inbox {
            if let OnyxEvent::Ingress(IngressEvent::Command(cmd)) = event {
                let clean_cmd = cmd.trim().to_lowercase();
                
                match clean_cmd.as_str() {
                    "agree" => {
                        if let Some(id) = self.player {
                            ctx.graph.set(id, self.clearance_attr, Value::Int(1));
                            println!("[BOOT] NDA accepted. Clearance updated to 1.");
                        }
                    }
                    "status" => {
                        if let Some(id) = self.player {
                            let clearance = ctx.graph.get(id, self.clearance_attr);
                            println!("[BOOT] Graph State -> Concept: {:?}, Clearance: {:?}", id, clearance);
                        }
                    }
                    _ => {
                        println!("[BOOT] Unknown command: {}", cmd);
                    }
                }
            }
        }
    }
}

fn make_boot(_: &SystemSpec) -> Result<Box<dyn OnyxSystem>, OnyxError> {
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

    // ==========================================
    // 1. FASE DE RECUPERACIÓN (LOAD)
    // ==========================================
    let save_key = "repl_savegame";
    
    match store.load(save_key) {
        Ok(bytes) => {
            println!("[CORE] Snapshot '{}' encontrado. Restaurando estado en WorldGraph...", save_key);
            if let Err(e) = graph.import(&bytes) {
                println!("[CORE ERROR] Fallo al importar estado: {}", e);
            } else {
                println!("[CORE] Grafo restaurado con éxito.");
            }
        }
        Err(_) => {
            println!("[CORE] No hay snapshot previo. Arrancando el universo desde cero.");
        }
    }

    let mut supervisor = Supervisor::new(systems.len());
    supervisor.init_all(&mut systems, &mut graph);
    supervisor.start(0);

    println!("========================================");
    println!("  OnYX Engine REPL (v0.1.0)");
    println!("  Type 'agree', 'status', or 'exit'");
    println!("========================================");

    let mut tick_count = 0;
    let seed = 42u64;

    loop {
        print!("onyx> ");
        io::stdout().flush().unwrap();

        let mut input = String::new();
        io::stdin().read_line(&mut input).unwrap();
        let command = input.trim();

        if command.eq_ignore_ascii_case("exit") {
            println!("Iniciando apagado seguro...");
            
            // ==========================================
            // 2. FASE DE PERSISTENCIA (SAVE)
            // ==========================================
            let snap_bytes = graph.export(); 
            
            match store.save(save_key, &snap_bytes) {
                Ok(_) => println!("[CORE] Grafo persistido en disco (./snapshots/{}.snap).", save_key),
                Err(e) => println!("[CORE ERROR] Fallo crítico guardando la partida: {}", e),
            }
            
            println!("Shutting down...");
            break;
        }

        if !command.is_empty() {
            driver.push_event(OnyxEvent::Ingress(IngressEvent::Command(command.into())));
        }

        tick(
            tick_count,
            seed,
            &mut driver,
            &mut graph,
            &mut store,
            &mut bus,
            &mut systems,
            supervisor.states_mut(),
            &[],
        );

        tick_count += 1;
    }

    println!("Engine stopped safely after {} ticks.", tick_count);
}
