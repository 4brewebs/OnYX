use std::io::{self, Write};
use onyx_soul::{
    event::{IngressEvent, OnyxEvent},
    system::{BlueprintLoader, SystemRegistry, SystemSpec},
    tick, HeadlessDriver, InMemoryBus, InMemorySystemRegistry, OnyxError,
    RustLoader, Supervisor, Graph, SnapshotStore,
};
use onyx_soul::store::FileSnapshotStore;

mod boot_system;
use boot_system::BootSystem;

fn make_boot(_: &SystemSpec) -> Result<Box<dyn onyx_soul::OnyxSystem>, OnyxError> {
    Ok(Box::new(BootSystem::new()))
}

fn main() {
    let mut graph = onyx_soul::WorldGraph::new();
    let mut bus = InMemoryBus::new();
    let mut driver = HeadlessDriver::new();
    let mut store = FileSnapshotStore::new("./snapshots").unwrap();
    let mut registry = InMemorySystemRegistry::new();
    let mut loader = RustLoader::new();

    registry.register("boot", make_boot);

    loader.register(onyx_soul::system::StageBlueprint {
        name: "boot".into(),
        systems: vec![SystemSpec {
            kind: "boot".into(),
            params: vec![],
            persistent: true,
        }],
        entities: vec![],
    });

    let bp = loader.load("boot").unwrap();
    let mut systems: Vec<Box<dyn onyx_soul::OnyxSystem>> = bp
        .systems
        .iter()
        .map(|spec| registry.build(spec).unwrap())
        .collect();

    // LOAD
    let save_key = "repl_savegame";
    match store.load(save_key) {
        Ok(bytes) => {
            println!("[CORE] Restaurando '{}'...", save_key);
            graph.import(&bytes).unwrap();
            println!("[CORE] Grafo restaurado.");
        }
        Err(_) => println!("[CORE] Arranque limpio."),
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
            println!("Apagado seguro...");
            let snap = graph.export();
            store.save(save_key, &snap).unwrap();
            println!("[CORE] Guardado en ./snapshots/{}.snap", save_key);
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

    println!("Engine stopped after {} ticks.", tick_count);
}