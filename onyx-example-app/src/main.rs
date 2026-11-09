use onyx_soul::store::FileSnapshotStore;
use onyx_soul::{
    Graph, HeadlessDriver, InMemoryBus, InMemorySystemRegistry, OnyxError, RustLoader,
    SnapshotStore, Supervisor,
    event::{IngressEvent, OnyxEvent},
    system::SystemSpec,
    tick,
};
use std::io::{self, Write};

mod boot_system;
mod stages;

use boot_system::BootSystem;

fn make_boot(_: &SystemSpec) -> Result<Box<dyn onyx_soul::system::OnyxSystem>, OnyxError> {
    Ok(Box::new(BootSystem::new()))
}

fn main() {
    let mut graph = onyx_soul::WorldGraph::new();
    let mut bus = InMemoryBus::new();
    let mut driver = HeadlessDriver::new();
    let mut store = FileSnapshotStore::new("./snapshots").unwrap();
    let mut registry = InMemorySystemRegistry::new();
    let mut loader = RustLoader::new();

    // El loader registra sistemas y stages, y devuelve los sistemas listos
    let mut systems = loader
        .load_app(
            &mut registry,
            &[("boot", make_boot)],
            &stages::all_stages(),
            "boot",
        )
        .unwrap();

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

        // TODO: this demo reads stdin directly. Terminal I/O should go through a
        // PlatformDriver (TuiDriver, kept outside the zero-dependency soul for now)
        // or run headless.
        let mut input = String::new();
        // EOF (`Ok(0)`) or unreadable input (e.g. non-UTF-8) is treated as `exit`,
        // so a closed stdin shuts down cleanly instead of spinning forever.
        let command = match io::stdin().read_line(&mut input) {
            Ok(0) | Err(_) => "exit",
            Ok(_) => input.trim(),
        };

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
