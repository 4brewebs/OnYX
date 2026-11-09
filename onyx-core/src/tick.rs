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
/// # Fases, en este orden
///
/// 1. Entrada: se añade `SystemEvent::Tick { n }` al bus y después
///    `driver.poll` vuelca su ingress. El bus se drena una sola vez: ese
///    lote es el inbox del tick. Con un bus FIFO, el inbox empieza por lo
///    que quedó pendiente del tick anterior, sigue con `Tick { n }` y
///    termina con el ingress.
/// 2. Ejecución: cada sistema `Active` ejecuta `run`, en el orden del slice,
///    con el mismo inbox, un outbox compartido y un único `DetRng` derivado de
///    `seed` y `n`. Los sistemas no `Active` no ejecutan `run`. Los sistemas
///    nunca reciben el bus.
/// 3. Control: se recorre primero el inbox y después el outbox, en orden.
///    `SnapshotRequested`, `RestoreRequested`, `Start` y `Stop` se atienden
///    aquí, dentro del mismo tick. El resto de eventos nuevos del outbox
///    vuelve al bus para el siguiente tick. El inbox ya entregado no se
///    vuelve a encolar.
/// 4. Salida: `driver.present` recibe una vista de solo lectura del grafo y
///    `surfaces`, las proyecciones activas que el cerebro declara en este
///    tick. Puede ser un slice vacío.
///
/// Los `Fault` y `SnapshotRestored` que emite el propio tick van al bus y
/// se entregan en el siguiente tick.
///
/// # Estados de los sistemas
///
/// `states` tiene una entrada por sistema y lo conserva el llamador entre
/// ticks.
///
/// - `Start` sobre `Stopped` llama a `on_start` y pasa a `Active`.
///   Sobre `Active` no hace nada. Sobre `Blocked` emite `Fault` y no activa.
/// - `Stop` sobre `Active` llama a `on_stop` y pasa a `Stopped`. En otro
///   estado no hace nada.
/// - Los hooks solo se llaman en transiciones. `Blocked` impide `run` y
///   todos los hooks.
///
/// # Snapshots y restauración
///
/// `SnapshotRequested` exporta el grafo y lo guarda en `store`. Si falla,
/// emite `Fault`. `RestoreRequested` carga e importa. Si la carga o la
/// importación fallan, emite `Fault` y no reinicia ningún sistema.
///
/// Tras cada importación correcta, todos los sistemas vuelven a ejecutar
/// `init`, incluidos los parados y los bloqueados, antes de atender la
/// siguiente petición. Se recorre el slice sistema a sistema: el `init` de
/// un sistema y, si procede, su `on_restore` terminan antes del `init` del
/// siguiente. No hay una fase global de `init` seguida de otra de
/// `on_restore`.
///
/// - Si `init` falla, el sistema pasa a `Blocked`, sin hooks, y se emite
///   `Fault`.
/// - Si `init` tiene éxito sobre `Active`, recibe `on_restore` con una vista
///   de solo lectura, nunca `on_start`.
/// - Si `init` tiene éxito sobre `Blocked`, pasa a `Stopped`, sin activarse
///   ni recibir hooks. `Stopped` se queda igual y reconstruye sus cachés
///   en su próximo `Start`.
///
/// Después se emite `SnapshotRestored`. `init` no debe reiniciar el estado
/// durable restaurado. Si un supervisor llama a `init` fuera del tick, debe
/// aplicar la misma política de `Blocked` y `Stopped`.
///
/// # Determinismo
///
/// `seed` es común a todos los ticks. El llamador la aporta y, para
/// continuar tras restaurar, debe recuperar `seed` y el contador `n`.
/// `n` es un contador, no un reloj.
///
/// Las pruebas de `tests/tick_contract.rs` comprueban este contrato.
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
