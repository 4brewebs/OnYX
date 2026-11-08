//! Bucle síncrono del cerebro.
//! Los sistemas activos se ejecutan en el orden del slice.
//! Las peticiones de control se atienden después de ejecutar los sistemas.
//! Los demás eventos nuevos se entregan en el siguiente tick.

use crate::bus::EventBus;
use crate::detrng::DetRng;
use crate::driver::PlatformDriver;
use crate::event::{OnyxEvent, SystemEvent};
use crate::graph::{AttrOnly, Graph, ReadOnly, SnapshotStore};
use crate::surface::SurfaceBinding;
use crate::system::{Ctx, OnyxSystem, SystemState};

/// Ejecuta un tick completo.
///
/// `surfaces` son las proyecciones activas que el cerebro declara en este tick.
/// El driver las recibe en `present`. Puede ser un slice vacío.
///
/// `seed` es común a todos los ticks. El llamador la aporta y debe recuperarla tras restore.
///
/// # Panics
/// Si `systems` y `states` tienen longitudes distintas.
pub fn tick(
    n: u64,
    seed: u64,
    driver: &mut dyn PlatformDriver,
    graph: &mut dyn Graph,
    store: &mut dyn SnapshotStore,
    bus: &mut dyn EventBus,
    systems: &mut [Box<dyn OnyxSystem>],
    states: &mut [SystemState],
    surfaces: &[SurfaceBinding],
) {
    assert_eq!(systems.len(), states.len());
    let mut rng = DetRng::from_seed_and_tick(seed, n);

    bus.push(OnyxEvent::System(SystemEvent::Tick { n }));
    driver.poll(bus);

    let inbox = bus.drain();
    let mut outbox = Vec::new();

    for (system, state) in systems.iter_mut().zip(states.iter()) {
        if *state == SystemState::Active {
            system.run(&mut Ctx {
                graph: &mut *graph,
                inbox: &inbox,
                outbox: &mut outbox,
                rng: &mut rng,
            });
        }
    }

    let events = inbox
        .into_iter()
        .map(|event| (event, false))
        .chain(outbox.into_iter().map(|event| (event, true)));

    for (event, is_new) in events {
        match event {
            OnyxEvent::System(SystemEvent::SnapshotRequested(key)) => {
                let bytes = graph.export();
                if let Err(e) = store.save(&key, &bytes) {
                    bus.push(OnyxEvent::System(SystemEvent::Fault(format!(
                        "snapshot save failed: {:?}",
                        e
                    ))));
                }
            }
            OnyxEvent::System(SystemEvent::RestoreRequested(key)) => match store.load(&key) {
                Ok(bytes) => match graph.import(&bytes) {
                    Ok(()) => {
                        for (system, state) in systems.iter_mut().zip(states.iter_mut()) {
                            match system.init(&mut AttrOnly(&mut *graph)) {
                                Ok(()) => match *state {
                                    SystemState::Active => system.on_restore(&ReadOnly(&*graph)),
                                    SystemState::Blocked => *state = SystemState::Stopped,
                                    SystemState::Stopped => {}
                                },
                                Err(e) => {
                                    *state = SystemState::Blocked;
                                    bus.push(OnyxEvent::System(SystemEvent::Fault(format!(
                                        "re-init failed for {}: {:?}",
                                        system.name(),
                                        e
                                    ))));
                                }
                            }
                        }
                        bus.push(OnyxEvent::System(SystemEvent::SnapshotRestored(key)));
                    }
                    Err(e) => {
                        bus.push(OnyxEvent::System(SystemEvent::Fault(format!(
                            "snapshot import failed: {:?}",
                            e
                        ))));
                    }
                },
                Err(e) => {
                    bus.push(OnyxEvent::System(SystemEvent::Fault(format!(
                        "snapshot load failed: {:?}",
                        e
                    ))));
                }
            },
            OnyxEvent::System(SystemEvent::Start(name)) => {
                for (system, state) in systems.iter_mut().zip(states.iter_mut()) {
                    if system.name() == name.as_str() {
                        match *state {
                            SystemState::Stopped => {
                                system.on_start(graph);
                                *state = SystemState::Active;
                            }
                            SystemState::Blocked => {
                                bus.push(OnyxEvent::System(SystemEvent::Fault(format!(
                                    "start rejected for blocked system {}",
                                    system.name()
                                ))));
                            }
                            SystemState::Active => {}
                        }
                    }
                }
            }
            OnyxEvent::System(SystemEvent::Stop(name)) => {
                for (system, state) in systems.iter_mut().zip(states.iter_mut()) {
                    if system.name() == name.as_str() && *state == SystemState::Active {
                        system.on_stop(graph);
                        *state = SystemState::Stopped;
                    }
                }
            }
            event => {
                if is_new {
                    bus.push(event);
                }
            }
        }
    }

    driver.present(&ReadOnly(&*graph), surfaces);
}
