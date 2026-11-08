use onyx_soul::{
    HeadlessDriver, InMemoryBus, InMemorySystemRegistry,
    RustLoader, Supervisor, WorldGraph, FileSnapshotStore,
};
use onyx_core::tick;

fn main() {
    let mut graph = WorldGraph::new();
    let mut bus = InMemoryBus::new();
    let mut driver = HeadlessDriver::new();
    let mut store = FileSnapshotStore::new("./snapshots").unwrap();
    let mut registry = InMemorySystemRegistry::new();
    let mut loader = RustLoader::new();

    // registrar sistemas y blueprints...
    // let mut systems = vec![];
    // let mut supervisor = Supervisor::new(systems.len());

    let seed = 42u64;
    let mut n = 0u64;

    // loop {
    //     tick(n, seed, &mut driver, &mut graph, &mut store, &mut bus,
    //          &mut systems, supervisor.states_mut(), &[]);
    //     n += 1;
    // }
}
