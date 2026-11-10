//! `AgentHost`: el bucle completo montado, para que una app no toque el core.
//!
//! Reúne grafo, bus, almacén de snapshots, sistemas, supervisor, semilla y
//! contador de ticks de este soul (`AgentGraph`, `AgentBus`, `RustLoader`,
//! `Supervisor`). No depende de `onyx-soul`. Los sistemas del framework se
//! construyen por la vía del loader (blueprint + fábricas); la app añade los
//! suyos con el mismo mecanismo (`factory` + `system_spec`) o directamente
//! (`system`, `tool`).
//!
//! El host no inventa `SurfaceId` ni impone un layout: [`AgentHost::step`]
//! pasa un slice vacío y [`AgentHost::step_with`] reenvía el que declare la
//! app. Tras un restore, [`AgentHost::set_tick`] fija el `n` del próximo
//! `step` (el que reciben `tick` y `DetRng`). No va acoplado a
//! [`AgentHost::request_restore`].

use std::collections::HashMap;

use onyx_core::{
    AttrOnly, AttrRegistry, ConceptId, EventBus, GraphView, OnyxError, OnyxEvent, OnyxSystem,
    PlatformDriver, SnapshotStore, SurfaceBinding, SystemEvent, SystemSpec, Value, tick,
};

use crate::bus::AgentBus;
use crate::config::AgentConfig;
use crate::graph::AgentGraph;
use crate::loader::RustLoader;
use crate::policy::{AgentView, MemoryPolicy, ScriptedPolicy, agent_view};
use crate::registry::{InMemorySystemRegistry, SystemFactory};
use crate::root::{ExecutionRoot, RootError, begin_event, execution_root};
use crate::runtime::{AgentRuntime, RUNTIME_NAME};
use crate::spec::{AgentSpec, spawn_event};
use crate::supervisor::Supervisor;
use crate::tools::{ToolExecutor, ToolSystem};
use crate::util::text;
use crate::vocab::{AgentState, Vocab};
use crate::{FACTORIES, stage_with};

/// Almacén de snapshots en memoria. Útil en tests y cuando la app no quiere
/// persistir a disco.
#[derive(Default)]
pub struct MemStore {
    blobs: HashMap<String, Vec<u8>>,
}

impl MemStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl SnapshotStore for MemStore {
    fn save(&mut self, key: &str, bytes: &[u8]) -> Result<(), OnyxError> {
        self.blobs.insert(key.to_string(), bytes.to_vec());
        Ok(())
    }

    fn load(&self, key: &str) -> Result<Vec<u8>, OnyxError> {
        self.blobs.get(key).cloned().ok_or(OnyxError::Missing)
    }
}

/// Estado observable del grafo: conceptos, propiedades por nombre y
/// relaciones. No depende de la numeración de `Attr`. Sirve para comparar
/// ejecuciones y para tests de reproducción.
pub type Fingerprint = Vec<(u64, Vec<(String, Value)>, Vec<(String, ConceptId)>)>;

pub struct HostBuilder {
    seed: u64,
    config: AgentConfig,
    stage_name: String,
    extra_specs: Vec<SystemSpec>,
    factories: Vec<(String, SystemFactory)>,
    tools: Vec<Box<dyn ToolExecutor>>,
    memory: Option<Box<dyn MemoryPolicy>>,
    scripted: Option<Box<dyn ScriptedPolicy>>,
    store: Option<Box<dyn SnapshotStore>>,
    systems: Vec<Box<dyn OnyxSystem>>,
}

impl HostBuilder {
    pub fn new(seed: u64) -> Self {
        Self {
            seed,
            config: AgentConfig::default(),
            stage_name: "agents".to_string(),
            extra_specs: Vec::new(),
            factories: Vec::new(),
            tools: Vec::new(),
            memory: None,
            scripted: None,
            store: None,
            systems: Vec::new(),
        }
    }

    pub fn config(mut self, config: AgentConfig) -> Self {
        self.config = config;
        self
    }

    pub fn stage_name(mut self, name: impl Into<String>) -> Self {
        self.stage_name = name.into();
        self
    }

    /// Fábrica propia, registrada junto a las del framework.
    pub fn factory(mut self, kind: impl Into<String>, factory: SystemFactory) -> Self {
        self.factories.push((kind.into(), factory));
        self
    }

    /// Sistema propio declarado en el blueprint (su `kind` debe tener fábrica).
    pub fn system_spec(mut self, spec: SystemSpec) -> Self {
        self.extra_specs.push(spec);
        self
    }

    /// Sistema ya construido, añadido tras los del blueprint.
    pub fn system(mut self, system: Box<dyn OnyxSystem>) -> Self {
        self.systems.push(system);
        self
    }

    /// Herramienta determinista. Todas las añadidas comparten un `ToolSystem`.
    pub fn tool(mut self, tool: Box<dyn ToolExecutor>) -> Self {
        self.tools.push(tool);
        self
    }

    pub fn memory(mut self, memory: Box<dyn MemoryPolicy>) -> Self {
        self.memory = Some(memory);
        self
    }

    pub fn scripted(mut self, scripted: Box<dyn ScriptedPolicy>) -> Self {
        self.scripted = Some(scripted);
        self
    }

    pub fn store(mut self, store: Box<dyn SnapshotStore>) -> Self {
        self.store = Some(store);
        self
    }

    /// Sin `config.execution` (modo heredado) se comporta como antes: los
    /// sistemas del blueprint leen la configuración de los params y el runtime
    /// sustituido por `memory`/`scripted` y las herramientas reciben `config`
    /// tal cual, sin normalizar. Con `config.execution`, la configuración debe
    /// volver exacta de `to_params` (contadores no mayores que `i64::MAX`), de
    /// modo que todos los sistemas y el host vean una única configuración; si
    /// no, es un error.
    pub fn build(self) -> Result<AgentHost, OnyxError> {
        let config = self.config;
        if config.execution.is_some() && AgentConfig::from_params(&config.to_params())? != config {
            return Err(OnyxError::Format(
                "execution config does not round-trip exactly through params".into(),
            ));
        }
        let mut graph = AgentGraph::new();
        let vocab = Vocab::resolve_with(&mut graph, &config.vocab_prefix);

        let blueprint = {
            let mut b = stage_with(&self.stage_name, &config);
            b.systems.extend(self.extra_specs);
            b
        };
        let mut factories: Vec<(&str, SystemFactory)> = FACTORIES.to_vec();
        factories.extend(self.factories.iter().map(|(k, f)| (k.as_str(), *f)));

        let mut registry = InMemorySystemRegistry::new();
        let mut loader = RustLoader::new();
        let mut systems =
            loader.load_app(&mut registry, &factories, &[blueprint], &self.stage_name)?;

        if self.memory.is_some() || self.scripted.is_some() {
            let mut runtime = AgentRuntime::new(config.clone());
            if let Some(m) = self.memory {
                runtime = runtime.with_memory(m);
            }
            if let Some(s) = self.scripted {
                runtime = runtime.with_scripted(s);
            }
            let slot = systems
                .iter()
                .position(|s| s.name() == RUNTIME_NAME)
                .ok_or(OnyxError::Missing)?;
            systems[slot] = Box::new(runtime);
        }
        if !self.tools.is_empty() {
            let mut tools = ToolSystem::new(config.clone());
            for t in self.tools {
                tools = tools.with_tool(t);
            }
            systems.push(Box::new(tools));
        }
        systems.extend(self.systems);

        for system in systems.iter_mut() {
            system.init(&mut AttrOnly(&mut graph))?;
        }
        let mut supervisor = Supervisor::new(systems.len());
        for i in 0..systems.len() {
            supervisor.start(i);
        }

        Ok(AgentHost {
            graph,
            bus: AgentBus::new(),
            store: self.store.unwrap_or_else(|| Box::new(MemStore::new())),
            systems,
            supervisor,
            vocab,
            config,
            seed: self.seed,
            n: 0,
        })
    }
}

pub struct AgentHost {
    graph: AgentGraph,
    bus: AgentBus,
    store: Box<dyn SnapshotStore>,
    systems: Vec<Box<dyn OnyxSystem>>,
    supervisor: Supervisor,
    vocab: Vocab,
    config: AgentConfig,
    seed: u64,
    n: u64,
}

impl AgentHost {
    pub fn builder(seed: u64) -> HostBuilder {
        HostBuilder::new(seed)
    }

    pub fn vocab(&self) -> Vocab {
        self.vocab
    }

    pub fn graph(&self) -> &AgentGraph {
        &self.graph
    }

    /// Registra (o recupera) un atributo propio de la app.
    pub fn attr(&mut self, name: &str) -> onyx_core::Attr {
        self.graph.attr(name)
    }

    /// El `n` del próximo [`step`](Self::step). Tras un `step` es el
    /// siguiente contador; un restore no lo realinea.
    pub fn ticks(&self) -> u64 {
        self.n
    }

    /// Fija el `n` del próximo [`step`](Self::step), el que reciben `tick`
    /// y `DetRng`. Justo después del step que atiende un snapshot,
    /// [`ticks`](Self::ticks) ya es ese valor: no hay que poner el `n` del
    /// tick que guardó. No se llama desde [`request_restore`](Self::request_restore).
    pub fn set_tick(&mut self, n: u64) {
        self.n = n;
    }

    pub fn system_names(&self) -> Vec<&str> {
        self.systems.iter().map(|s| s.name()).collect()
    }

    /// Un tick completo. No declara superficies.
    pub fn step(&mut self, driver: &mut dyn PlatformDriver) {
        self.step_with(driver, &[]);
    }

    /// Un tick completo con las superficies que declara la app.
    pub fn step_with(&mut self, driver: &mut dyn PlatformDriver, surfaces: &[SurfaceBinding]) {
        tick(
            self.n,
            self.seed,
            driver,
            &mut self.graph,
            &mut *self.store,
            &mut self.bus,
            &mut self.systems,
            self.supervisor.states_mut(),
            surfaces,
        );
        self.n += 1;
    }

    pub fn run(&mut self, driver: &mut dyn PlatformDriver, ticks: usize) {
        for _ in 0..ticks {
            self.step(driver);
        }
    }

    /// Ticks hasta que no queda ningún agente sin terminar, con un tope.
    /// Devuelve `true` si se asentó. Sin agentes cuenta como asentado.
    pub fn run_until_settled(&mut self, driver: &mut dyn PlatformDriver, max_ticks: usize) -> bool {
        for _ in 0..max_ticks {
            self.step(driver);
            if self.is_settled() {
                return true;
            }
        }
        false
    }

    pub fn push(&mut self, event: OnyxEvent) {
        self.bus.push(event);
    }

    pub fn spawn(&mut self, spec: &AgentSpec) {
        self.bus.push(spawn_event(&self.vocab, spec, None));
    }

    /// Arranque nuevo explícito: pide crear la raíz de ejecución (se atiende
    /// en el próximo tick, solo con grafo vacío). Pídelo antes del primer
    /// snapshot recuperable. Para reanudar no se usa: restaura un snapshot que
    /// ya contenga su raíz y compruébala con [`execution_root`](Self::execution_root).
    pub fn begin_execution(&mut self) {
        self.bus.push(begin_event(&self.vocab));
    }

    /// Valida la raíz contra la configuración del host. Solo lectura. Un `Ok`
    /// no habilita efectos externos: la frontera del receptor y TR2 faltan.
    pub fn execution_root(&self) -> Result<ExecutionRoot, RootError> {
        execution_root(&self.graph, &self.vocab, &self.config)
    }

    /// Pide guardar el grafo (se atiende al final del próximo tick).
    pub fn request_snapshot(&mut self, key: &str) {
        self.bus
            .push(OnyxEvent::System(SystemEvent::SnapshotRequested(
                key.to_string(),
            )));
    }

    /// Pide restaurar el grafo (se atiende al final del próximo tick).
    /// Los eventos en vuelo no forman parte del snapshot.
    pub fn request_restore(&mut self, key: &str) {
        self.bus
            .push(OnyxEvent::System(SystemEvent::RestoreRequested(
                key.to_string(),
            )));
    }

    pub fn agents(&self) -> Vec<ConceptId> {
        self.graph.query(&[self.vocab.state])
    }

    pub fn agent_state(&self, id: ConceptId) -> Option<AgentState> {
        text(&self.graph, id, self.vocab.state).and_then(|s| AgentState::parse(&s))
    }

    pub fn view(&self, id: ConceptId) -> AgentView {
        agent_view(&self.graph, &self.vocab, id)
    }

    pub fn result(&self, id: ConceptId) -> Option<String> {
        text(&self.graph, id, self.vocab.result)
    }

    /// `true` si no hay agentes o todos están en `Done` o `Failed`.
    /// Un `spawn` aún no procesado no crea agentes: `is_settled` será `true`
    /// hasta el próximo [`step`](Self::step).
    pub fn is_settled(&self) -> bool {
        self.agents()
            .into_iter()
            .all(|id| self.agent_state(id).is_some_and(AgentState::is_terminal))
    }

    pub fn fingerprint(&self) -> Fingerprint {
        self.graph
            .query(&[])
            .into_iter()
            .map(|id| (id.0, self.graph.props(id), self.graph.links(id)))
            .collect()
    }
}
