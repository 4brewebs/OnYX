//! `SDD-07` Fase 1 — Motor compartido de depuración temporal (`TimeFreeze`).
//!
//! Módulo de nivel de sesión: lo consumen por igual el bucle `run_session`
//! (TUI `chat`) y `serve` (Web), mismo patrón que `UiIdlePump`.
//!
//! - [`InMemoryTickRing`]: los últimos K=64 ticks (24 MiB máx.) en RAM,
//!   capturados en cada frontera de tick. 100% `std` (`BTreeMap<u64, Vec<u8>>`).
//! - [`TimeDebugState`]: handle clonable y thread-safe (`frozen`, `cursor_tick`,
//!   `present_tick`, `step_llm_budget`). Invariantes: `cursor_tick <= present_tick`;
//!   scrubbing a `< present` auto-congela; el preview es siempre read-only.
//! - [`TimeDebugSnapshotStore`]: store del `AgentHost`: `load` consulta primero el
//!   ring RAM y luego el pack en disco; `save` escribe **solo** al pack en disco
//!   (la RAM nunca termina en `.onyxpack`).
//! - Gate de mutación ([`MutationGateCheck`] / [`check_mutation_gate`]) y
//!   [`confirm_branch_from_cursor`] (bifurcación desde el cursor con 0 probe host,
//!   0 I/O adicional: `pending_revert_files` se lee del grafo vivo en RAM).
//!
//! La Fase 1 no añade UI: los puntos de entrada (freeze/scrub/bifurcación) son
//! APIs públicas que usarán la Fase 3 (TUI) y la Fase 4 (Web/PWA).
//!
//! `#![allow(dead_code)]`: la Fase 1 wiring (bucle de sesión + driver LLM)
//! consume solo parte del API; el resto (scrub/branch/preview) es lo que
//! consumirán las Fases 2-4 y debe quedar compilando ahora.
#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use onyx_soul_agents::{
    AgentGraph, AgentHost, Graph, GraphView, HeadlessDriver, OnyxError, SnapshotStore, Vocab,
    pending_approvals, pending_tool_calls,
};
use onyx_workspace::{
    RecordedEffect, WorkspaceTools, WorkspaceVocab, effects_after_tick,
    restore_git_tree_checkpoint_for_targets,
};

use crate::pack::{SharedPackStore, default_fork_dir, fork_pack_until_tick};

/// Capacidad por defecto del ring en RAM: 64 ticks.
pub const DEFAULT_RING_CAPACITY: usize = 64;
/// Tope de memoria del ring por defecto: 24 MiB.
pub const DEFAULT_RING_MAX_BYTES: usize = 24 * 1024 * 1024;
/// Tope de entradas de la caché de preview (LRU, 100% `std`).
pub const PREVIEW_CACHE_CAPACITY: usize = 8;

/// Error accionable del depurador temporal (nunca pánico, nunca silencio).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimeDebugError(pub String);

impl fmt::Display for TimeDebugError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for TimeDebugError {}

/// Anillo en RAM de snapshots `ONYXAG` por tick: `BTreeMap<u64, Vec<u8>>` con
/// tope de entrada (K) y tope de bytes; al desbordar, evicta el tick más antiguo.
pub struct InMemoryTickRing {
    entries: BTreeMap<u64, Vec<u8>>,
    total_bytes: usize,
    capacity: usize,
    max_bytes: usize,
}

impl InMemoryTickRing {
    pub fn new() -> Self {
        Self::with_limits(DEFAULT_RING_CAPACITY, DEFAULT_RING_MAX_BYTES)
    }

    pub fn with_limits(capacity: usize, max_bytes: usize) -> Self {
        Self {
            entries: BTreeMap::new(),
            total_bytes: 0,
            capacity,
            max_bytes,
        }
    }

    /// Inserta (o reemplaza) el snapshot de `tick` y evicta los más antiguos
    /// hasta cumplir ambos topes.
    pub fn push(&mut self, tick: u64, bytes: &[u8]) {
        if let Some(prev) = self.entries.remove(&tick) {
            self.total_bytes = self.total_bytes.saturating_sub(prev.len());
        }
        self.entries.insert(tick, bytes.to_vec());
        self.total_bytes = self.total_bytes.saturating_add(bytes.len());
        while !self.entries.is_empty()
            && (self.entries.len() > self.capacity || self.total_bytes > self.max_bytes)
        {
            // Nunca se evicta la única entrada: el tick más reciente se
            // conserva aunque solo él supere el tope.
            if self.entries.len() == 1 {
                break;
            }
            if let Some((_, evicted)) = self.entries.pop_first() {
                self.total_bytes = self.total_bytes.saturating_sub(evicted.len());
            }
        }
    }

    pub fn get(&self, tick: u64) -> Option<&[u8]> {
        self.entries.get(&tick).map(|b| b.as_slice())
    }

    /// Ticks presentes en el anillo, ascendentes.
    pub fn ticks(&self) -> Vec<u64> {
        self.entries.keys().copied().collect()
    }

    /// Elimina el futuro estricto del tick indicado (bifurcación).
    pub fn truncate_after(&mut self, tick: u64) {
        let mut removed = 0usize;
        self.entries.retain(|&t, bytes| {
            if t <= tick {
                true
            } else {
                removed += bytes.len();
                false
            }
        });
        self.total_bytes = self.total_bytes.saturating_sub(removed);
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.total_bytes = 0;
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn total_bytes(&self) -> usize {
        self.total_bytes
    }
}

/// Caché LRU (100% `std`: `VecDeque` + `BTreeMap`) de grafos preview efímeros.
pub struct PreviewGraphCache {
    order: VecDeque<u64>,
    graphs: BTreeMap<u64, Arc<AgentGraph>>,
}

impl PreviewGraphCache {
    pub fn new() -> Self {
        Self {
            order: VecDeque::new(),
            graphs: BTreeMap::new(),
        }
    }

    /// Devuelve el grafo cacheado (si existe) y lo marca como más reciente.
    pub fn get(&mut self, tick: u64) -> Option<Arc<AgentGraph>> {
        let graph = self.graphs.get(&tick)?.clone();
        if let Some(pos) = self.order.iter().position(|&t| t == tick) {
            self.order.remove(pos);
        }
        self.order.push_back(tick);
        Some(graph)
    }

    pub fn insert(&mut self, tick: u64, graph: Arc<AgentGraph>) {
        match self.graphs.insert(tick, graph) {
            None => {
                self.order.push_back(tick);
                while self.order.len() > PREVIEW_CACHE_CAPACITY {
                    if self
                        .order
                        .pop_front()
                        .is_some_and(|evict| self.graphs.remove(&evict).is_some())
                    {
                        // Se evictó la entrada más antigua.
                    }
                }
            }
            Some(_) => {
                if let Some(pos) = self.order.iter().position(|&t| t == tick) {
                    self.order.remove(pos);
                }
                self.order.push_back(tick);
            }
        }
    }

    pub fn clear(&mut self) {
        self.order.clear();
        self.graphs.clear();
    }

    pub fn len(&self) -> usize {
        self.graphs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.graphs.is_empty()
    }
}

/// Store del `AgentHost` para el depurador temporal:
/// - `load`: primero el ring RAM (ticks aún no purgados), luego el pack en disco;
///   si no está en ninguno, error accionable.
/// - `save`: **solo** al pack en disco (la RAM nunca termina en `.onyxpack`).
pub struct TimeDebugSnapshotStore {
    pack: Option<SharedPackStore>,
    ring: Arc<Mutex<InMemoryTickRing>>,
}

impl TimeDebugSnapshotStore {
    pub fn new(pack: Option<SharedPackStore>, ring: Arc<Mutex<InMemoryTickRing>>) -> Self {
        Self { pack, ring }
    }

    /// Arc del ring compartido (para construir [`TimeDebugState`]).
    pub fn ring(&self) -> Arc<Mutex<InMemoryTickRing>> {
        Arc::clone(&self.ring)
    }
}

impl SnapshotStore for TimeDebugSnapshotStore {
    fn save(&mut self, key: &str, bytes: &[u8]) -> Result<(), OnyxError> {
        match &mut self.pack {
            Some(pack) => pack.save(key, bytes),
            None => Err(OnyxError::Format(format!(
                "snapshot persistence is disabled: cannot save `{key}` (the RAM ring is not a `.onyxpack` store)"
            ))),
        }
    }

    fn load(&self, key: &str) -> Result<Vec<u8>, OnyxError> {
        if let Ok(tick) = key.parse::<u64>() {
            if let Ok(ring) = self.ring.lock() {
                if let Some(bytes) = ring.get(tick) {
                    return Ok(bytes.to_vec());
                }
            }
        }
        match &self.pack {
            Some(pack) => pack.load(key),
            None => Err(OnyxError::Format(format!(
                "snapshot `{key}`: not in the RAM ring and snapshot persistence is disabled"
            ))),
        }
    }
}

/// Estado del depurador temporal: clonable, thread-safe, sin `UI`.
#[derive(Clone)]
pub struct TimeDebugState {
    frozen: Arc<AtomicBool>,
    cursor_tick: Arc<AtomicU64>,
    present_tick: Arc<AtomicU64>,
    /// Turnos LLM permitidos bajo congelación (`/step-llm`, Fase 2).
    step_llm_budget: Arc<AtomicUsize>,
    ring: Arc<Mutex<InMemoryTickRing>>,
    preview_cache: Arc<Mutex<PreviewGraphCache>>,
    pack: Option<SharedPackStore>,
}

impl TimeDebugState {
    pub fn new(pack: Option<SharedPackStore>) -> Self {
        Self {
            frozen: Arc::new(AtomicBool::new(false)),
            cursor_tick: Arc::new(AtomicU64::new(0)),
            present_tick: Arc::new(AtomicU64::new(0)),
            step_llm_budget: Arc::new(AtomicUsize::new(0)),
            ring: Arc::new(Mutex::new(InMemoryTickRing::new())),
            preview_cache: Arc::new(Mutex::new(PreviewGraphCache::new())),
            pack,
        }
    }

    /// Límites de ring personalizados (tests).
    pub fn with_ring_limits(self, capacity: usize, max_bytes: usize) -> Self {
        if let Ok(mut ring) = self.ring.lock() {
            *ring = InMemoryTickRing::with_limits(capacity, max_bytes);
        }
        self
    }

    /// Arc del ring, para construir [`TimeDebugSnapshotStore`].
    pub fn ring_arc(&self) -> Arc<Mutex<InMemoryTickRing>> {
        Arc::clone(&self.ring)
    }

    pub fn is_frozen(&self) -> bool {
        self.frozen.load(Ordering::Acquire)
    }

    pub fn freeze(&self) {
        self.frozen.store(true, Ordering::Release);
    }

    /// Descongela y descarta cualquier presupuesto de `/step-llm` no consumido.
    pub fn unfreeze(&self) {
        self.frozen.store(false, Ordering::Release);
        self.step_llm_budget.store(0, Ordering::Release);
    }

    pub fn cursor(&self) -> u64 {
        self.cursor_tick.load(Ordering::Acquire)
    }

    pub fn present(&self) -> u64 {
        self.present_tick.load(Ordering::Acquire)
    }

    /// Fija la frontera de presente **y el cursor**: `tick` es el **último
    /// tick completado** de la nueva línea temporal (p. ej. tras
    /// `--resume --tick` o `/rewind`). Convención única (alineada con
    /// `MilestoneSnapshotSystem`, que sella hitos en `host.ticks() - 1`).
    pub fn set_present(&self, tick: u64) {
        self.present_tick.store(tick, Ordering::Release);
        self.cursor_tick.store(tick, Ordering::Release);
    }

    /// Captura la frontera de tick `tick` en el ring y realza `present_tick`
    /// (= **último tick completado**; solo crece durante la ejecución
    /// normal). Avance del cursor (fix de auditoría Fase 2):
    /// - sin congelación → el cursor sigue al presente;
    /// - congelado **en el presente** (`cursor >= prev_present`) → el cursor
    ///   sigue al presente **sin descongelar** (las acciones selladas `/step`
    ///   y el borde de sesión no bifurcan de un presente que ya es el cursor);
    /// - congelado **mirando el pasado** (`cursor < prev_present`) → el
    ///   cursor queda anclado: la siguiente mutación bifurca desde él.
    pub fn record_tick(&self, tick: u64, bytes: &[u8]) {
        if let Ok(mut ring) = self.ring.lock() {
            ring.push(tick, bytes);
        }
        let prev_present = self.present_tick.fetch_max(tick, Ordering::AcqRel);
        let new_present = prev_present.max(tick);
        let cur = self.cursor_tick.load(Ordering::Acquire);
        if !self.is_frozen() || cur >= prev_present {
            self.cursor_tick.store(new_present, Ordering::Release);
        }
    }

    /// Ticks scrubeables: anillo RAM ∪ hitos en disco ∪ presente en vivo.
    pub fn available_ticks(&self) -> Vec<u64> {
        let mut set = BTreeSet::new();
        if let Ok(ring) = self.ring.lock() {
            set.extend(ring.ticks());
        }
        if let Some(pack) = &self.pack {
            set.extend(pack.entries().iter().map(|m| m.tick));
        }
        let present = self.present();
        if present > 0 {
            set.insert(present);
        }
        // Solo ticks de la línea activa actual (los hitos de otra rama
        // superior al presente no son scrubeables aquí).
        set.retain(|&t| t <= present);
        set.into_iter().collect()
    }

    fn available_summary(&self) -> String {
        let ticks = self.available_ticks();
        if ticks.is_empty() {
            "(ninguno aún)".to_string()
        } else {
            ticks
                .iter()
                .take(24)
                .map(u64::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        }
    }

    pub fn tick_available(&self, tick: u64) -> bool {
        if tick == self.present() {
            return true;
        }
        if let Ok(ring) = self.ring.lock() {
            if ring.get(tick).is_some() {
                return true;
            }
        }
        self.pack
            .as_ref()
            .is_some_and(|pack| pack.entries().iter().any(|m| m.tick == tick))
    }

    /// Mueve el cursor a `tick`. Invariante: `tick <= present`. Scrubbing a
    /// `< present` auto-congela; a `== present` (Esc) descongela.
    pub fn scrub_to(&self, tick: u64) -> Result<(), TimeDebugError> {
        let present = self.present();
        if tick > present {
            return Err(TimeDebugError(format!(
                "tick {tick} es futuro (presente = {present}); no existe todavía en esta rama"
            )));
        }
        if tick == present {
            self.cursor_tick.store(tick, Ordering::Release);
            self.frozen.store(false, Ordering::Release);
            return Ok(());
        }
        if !self.tick_available(tick) {
            return Err(TimeDebugError(format!(
                "tick {tick} no es scrubeable (purgado del anillo RAM y sin hito en disco); \
                 disponibles: {}",
                self.available_summary()
            )));
        }
        self.cursor_tick.store(tick, Ordering::Release);
        if tick < present {
            self.frozen.store(true, Ordering::Release);
        }
        Ok(())
    }

    /// Scrubbing relativo sobre `available_ticks()` (+1 / -1, etc.).
    pub fn scrub_delta(&self, delta: i64) -> Result<u64, TimeDebugError> {
        if delta == 0 {
            return Ok(self.cursor());
        }
        let available = self.available_ticks();
        let current = self.cursor();
        let idx = match available.iter().position(|&t| t == current) {
            Some(i) => i,
            None => {
                if delta < 0 {
                    available
                        .iter()
                        .rposition(|&t| t < current)
                        .ok_or_else(|| TimeDebugError("no hay tick anterior scrubeable".into()))?
                } else {
                    available
                        .iter()
                        .position(|&t| t > current)
                        .ok_or_else(|| TimeDebugError("no hay tick posterior scrubeable".into()))?
                }
            }
        };
        let next = if delta < 0 {
            let steps = delta.unsigned_abs() as usize;
            // `steps == idx` alcanza `available[0]` (fix de auditoría: el
            // límite inferior es inclusive).
            if steps > idx {
                return Err(TimeDebugError(format!(
                    "no hay {steps} tick(s) anteriores scrubeables (cursor {current})"
                )));
            }
            available[idx - steps]
        } else {
            let steps = delta.unsigned_abs() as usize;
            available
                .get(idx.saturating_add(steps))
                .copied()
                .ok_or_else(|| {
                    TimeDebugError(format!(
                        "no hay {steps} tick(s) posteriores scrubeables (presente {})",
                        self.present()
                    ))
                })?
        };
        self.scrub_to(next)?;
        Ok(next)
    }

    /// Vuelve al presente (Esc): cursor = presente y descongela.
    pub fn return_to_present(&self) {
        let present = self.present();
        self.cursor_tick.store(present, Ordering::Release);
        self.frozen.store(false, Ordering::Release);
        self.step_llm_budget.store(0, Ordering::Release);
    }

    /// Autoriza exactamente 1 turno LLM bajo congelación (`/step-llm`, Fase 2).
    pub fn grant_step_llm(&self, turns: usize) {
        self.step_llm_budget.fetch_add(turns, Ordering::Release);
    }

    /// Gate del `DynamicLlmDriver::present`:
    /// - no congelado → siempre despacha;
    /// - congelado con presupuesto → consume exactamente 1 turno;
    /// - congelado sin presupuesto → **no despacha** (ni marca el request como
    ///   `dispatched`: al descongelar sale al instante).
    pub fn try_acquire_llm_turn(&self) -> bool {
        if !self.frozen.load(Ordering::Acquire) {
            return true;
        }
        let mut budget = self.step_llm_budget.load(Ordering::Acquire);
        loop {
            if budget == 0 {
                return false;
            }
            match self.step_llm_budget.compare_exchange(
                budget,
                budget - 1,
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(actual) => budget = actual,
            }
        }
    }

    fn load_tick_bytes(&self, tick: u64) -> Result<Vec<u8>, TimeDebugError> {
        // Fix de auditoría: `present` es un tick **real** (el último
        // completado, capturado en el ring), no un tick fantasma: se busca
        // en ring/pack como cualquier otro.
        if let Ok(ring) = self.ring.lock() {
            if let Some(bytes) = ring.get(tick) {
                return Ok(bytes.to_vec());
            }
        }
        if let Some(pack) = &self.pack {
            if let Ok(bytes) = pack.load(&tick.to_string()) {
                return Ok(bytes);
            }
        }
        if tick == self.present() {
            return Err(TimeDebugError(format!(
                "tick {tick} es el presente en vivo y su frontera no está en el anillo ni \
                 en disco: usa el grafo del host (no hace falta preview)"
            )));
        }
        Err(TimeDebugError(format!(
            "no hay bytes para tick {tick} (purgado del anillo RAM y sin hito en disco); \
             disponibles: {}",
            self.available_summary()
        )))
    }

    /// Preview read-only de `tick`: grafo `AgentGraph` efímero (100% RAM),
    /// cacheado LRU por tick. No toca el `AgentHost`, Git ni `.onyxpack`.
    pub fn preview_graph(&self, tick: u64) -> Result<Arc<AgentGraph>, TimeDebugError> {
        {
            let mut cache = self
                .preview_cache
                .lock()
                .map_err(|_| TimeDebugError("preview cache lock poisoned".into()))?;
            if let Some(graph) = cache.get(tick) {
                return Ok(graph);
            }
        }
        let bytes = self.load_tick_bytes(tick)?;
        let mut graph = AgentGraph::new();
        graph.import(&bytes).map_err(|e| {
            TimeDebugError(format!(
                "no se pudo importar el preview de tick {tick}: {e:?}"
            ))
        })?;
        let graph = Arc::new(graph);
        if let Ok(mut cache) = self.preview_cache.lock() {
            cache.insert(tick, Arc::clone(&graph));
        }
        Ok(graph)
    }

    pub fn clear_preview_cache(&self) {
        if let Ok(mut cache) = self.preview_cache.lock() {
            cache.clear();
        }
    }

    /// Qué efectos externos se revertirían al confirmar la bifurcación desde el
    /// cursor: **solo RAM** (grafo vivo), 0 probe host, 0 I/O; funciona con y sin
    /// `--snapshot-dir`.
    pub fn pending_revert_files<G: GraphView + ?Sized>(
        &self,
        graph: &G,
        ws: &WorkspaceVocab,
    ) -> Vec<RecordedEffect> {
        effects_after_tick(graph, ws, self.cursor())
    }
}

/// Resultado del gate de mutación: todos los contadores y la razón accionable.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MutationGateCheck {
    pub llm_in_flight: usize,
    pub running_subagents: usize,
    pub pending_approvals: usize,
    /// Tool calls en ejecución externa (`AgentState::Acting`). Fix de
    /// auditoría: sustituye a `pending_llm_requests` — congelar en `Thinking`
    /// con `llm_in_flight == 0` (breakpoint pre-LLM) **sí** permite mutar.
    pub pending_tool_calls: usize,
}

impl MutationGateCheck {
    pub fn allowed(&self) -> bool {
        self.llm_in_flight == 0
            && self.running_subagents == 0
            && self.pending_approvals == 0
            && self.pending_tool_calls == 0
    }

    /// Razón accionable del primer bloqueo (si lo hay).
    pub fn reason(&self) -> Option<String> {
        if self.llm_in_flight > 0 {
            Some(format!(
                "{} request(s) LLM en vuelo: espera a que se asiente antes de mutar",
                self.llm_in_flight
            ))
        } else if self.running_subagents > 0 {
            Some(format!(
                "{} subagente(s) en ejecución: espera a que terminen antes de mutar",
                self.running_subagents
            ))
        } else if self.pending_approvals > 0 {
            Some(format!(
                "{} aprobación(es) pendiente(s): resuélvelas antes de mutar",
                self.pending_approvals
            ))
        } else if self.pending_tool_calls > 0 {
            Some(format!(
                "{} tool call(s) pendientes: espera a que se asienten antes de mutar",
                self.pending_tool_calls
            ))
        } else {
            None
        }
    }
}

/// Gate completo para acciones mutantes (`/time-branch`, `!cmd`, `/read`, `/inject`,
/// `/graph` mutante): todas las condiciones deben ser ciertas.
pub fn check_mutation_gate(
    graph: &dyn GraphView,
    v: &Vocab,
    llm_in_flight: usize,
    running_subagents: usize,
) -> Result<MutationGateCheck, MutationGateCheck> {
    let check = MutationGateCheck {
        llm_in_flight,
        running_subagents,
        pending_approvals: pending_approvals(graph, v).len(),
        pending_tool_calls: pending_tool_calls(graph, v).len(),
    };
    if check.allowed() {
        Ok(check)
    } else {
        Err(check)
    }
}

/// Confirma la bifurcación desde `cursor` (el futuro queda preservado):
///
/// 1. Gate de mutación (`llm_in_flight == 0`, sin subagentes `Running`, sin
///    aprobaciones ni tool calls pendientes).
/// 2. `effects_after_tick(grafo_vivo, cursor)`: **solo RAM** (0 probe host, 0 I/O).
/// 3. Si algún efecto lleva `tree_sha`: `restore_git_tree_checkpoint_for_targets`
///    (elimina además los `fs_write` untracked creados tras el checkpoint).
/// 4. Si hay store: `fork_pack_until_tick` (el futuro se conserva intacto en el
///    pack original; la línea activa pasa al fork).
/// 5. `host.request_restore(cursor)` + 1 step determinista + `host.set_tick(cursor+1)`;
///    el store del host consulta primero el ring RAM (funciona sin `--snapshot-dir`).
/// 6. Trunca el ring `> cursor`, limpia la caché de preview, `present_tick = cursor`
///    y `tools.clear_seen_reads()` (reset anti-gaslighting, SDD-04).
// `llm_in_flight`/`running_subagents` vienen del driver y del gestor de
// subagentes (fuera del grafo), por lo que no pueden derivarse de `host`.
#[allow(clippy::too_many_arguments)]
pub fn confirm_branch_from_cursor(
    host: &mut AgentHost,
    state: &TimeDebugState,
    workspace_root: &Path,
    ws_vocab: &WorkspaceVocab,
    tools: &WorkspaceTools,
    pack: Option<&SharedPackStore>,
    llm_in_flight: usize,
    running_subagents: usize,
) -> Result<String, TimeDebugError> {
    let cursor = state.cursor();
    let present = state.present();
    if cursor >= present {
        return Err(TimeDebugError(format!(
            "ya estás en el presente (tick {present}); nada que bifurcar"
        )));
    }
    check_mutation_gate(
        host.graph(),
        &host.vocab(),
        llm_in_flight,
        running_subagents,
    )
    .map_err(|check| {
        TimeDebugError(format!(
            "mutación bloqueada: {}",
            check.reason().unwrap_or_default()
        ))
    })?;
    if !state.tick_available(cursor) {
        return Err(TimeDebugError(format!(
            "no se puede bifurcar desde tick {cursor} (purgado del anillo RAM y sin hito en \
             disco); disponibles: {}",
            state
                .available_ticks()
                .iter()
                .map(u64::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }

    let effects = effects_after_tick(host.graph(), ws_vocab, cursor);
    let restored_tree = if let Some(tree_sha) = effects.iter().find_map(|e| e.tree_sha.as_deref()) {
        let created_targets: Vec<&str> = effects
            .iter()
            .filter(|e| e.tool == "fs_write")
            .map(|e| e.target.as_str())
            .collect();
        restore_git_tree_checkpoint_for_targets(workspace_root, tree_sha, &created_targets)
            .map_err(|err| {
                TimeDebugError(format!(
                    "no se pudo restaurar el árbol Git `{tree_sha}`: {err}"
                ))
            })?;
        Some(tree_sha.to_string())
    } else {
        None
    };

    let mut branch_note = String::new();
    if let Some(pack) = pack {
        let src_dir = pack.current_dir();
        if pack.latest_tick().is_some_and(|latest| latest > cursor) {
            let fork_dir = default_fork_dir(&src_dir, cursor, None);
            let report = fork_pack_until_tick(&src_dir, &fork_dir, cursor).map_err(|err| {
                TimeDebugError(format!("no se pudo bifurcar el pack de snapshots: {err:?}"))
            })?;
            pack.switch_dir(&report.dest_dir);
            branch_note = format!(
                " Branched snapshots into `{}` ({} milestones, original `{}` preserved).",
                report.dest_dir.display(),
                report.copied_snapshots,
                src_dir.display()
            );
        }
    }

    host.request_restore(&cursor.to_string());
    host.step(&mut HeadlessDriver::new());
    host.set_tick(cursor.saturating_add(1));

    if let Ok(mut ring) = state.ring.lock() {
        ring.truncate_after(cursor);
    }
    state.clear_preview_cache();
    state.set_present(cursor);
    tools.clear_seen_reads();

    let mut summary = format!(
        "[time-branch] Branched from tick {cursor} (present {present}); clock at tick {}.",
        host.ticks()
    );
    if let Some(sha) = restored_tree {
        summary.push_str(&format!(
            " Restored git tree `{sha}` ({} orphaned effect(s)).",
            effects.len()
        ));
    }
    summary.push_str(&branch_note);
    Ok(summary)
}

// ---------------------------------------------------------------------------
// `SDD-07` Fase 2 — Comandos sellados deterministas (`ONYXTR2`)
// ---------------------------------------------------------------------------

/// Cap (bytes) de la salida sellada de `! <cmd>` / `/read` antes del sellado
/// (truncado `UTF-8`-safe; patrón del cap de `SDD-05`, valor de la Fase 2).
pub const DET_SEALED_OUTPUT_CAP_BYTES: usize = 8 * 1024;

/// Prefijo interno reservado para el resultado sellado de una ejecución shell
/// verificada (`! <cmd>`, Fase 2). El payload JSON lleva el **resultado ya
/// obtenido** (no el comando a ejecutar): en replay se consume con 0 I/O.
/// Mismo patrón que [`crate::subagent::SUBAGENT_PROPOSAL_CMD_PREFIX`].
pub const DET_SHELL_CMD_PREFIX: &str = "/__onyx_det_shell ";

/// Prefijo interno reservado para el resultado sellado de una lectura
/// verificada (`/read`, Fase 2).
pub const DET_READ_CMD_PREFIX: &str = "/__onyx_det_read ";

/// Prefijo interno reservado para el comando sellado de `/step-llm`
/// (Fase 2): el spawner lo intercepta en-tick y reactiva el root
/// (`Done`/`Failed` → `Idle`) de forma determinista (0 I/O); el despacho
/// LLM queda limitado por `try_acquire_llm_turn` (presupuesto 1) en el
/// driver. En replay el mismo comando produce el mismo EAV (bit-a-bit).
pub const DET_STEP_LLM_CMD_PREFIX: &str = "/__onyx_step_llm";

/// Resultado sellado de una ejecución shell del borde de sesión.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SealedShellResult {
    pub command: String,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub output: String,
}

/// Resultado sellado de una lectura de fichero del borde de sesión
/// (líneas 1-based, inclusivas).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SealedReadResult {
    pub path: String,
    pub start_line: Option<usize>,
    pub end_line: Option<usize>,
    pub content: String,
}

/// Serializa un [`SealedShellResult`] como comando interno determinista
/// grabable en `ONYXTR2`.
pub fn encode_det_shell_command(result: &SealedShellResult) -> String {
    let payload = serde_json::to_string(result).unwrap_or_else(|_| "{}".to_string());
    format!("{DET_SHELL_CMD_PREFIX}{payload}")
}

/// Decodifica un comando sellado `/__onyx_det_shell {...}`; `None` ante
/// prefijo o payload corrupto/malformado (nunca pánico, nunca silencio).
pub fn decode_det_shell_command(raw_cmd: &str) -> Option<SealedShellResult> {
    let payload = raw_cmd.trim().strip_prefix(DET_SHELL_CMD_PREFIX)?;
    serde_json::from_str(payload).ok()
}

/// Serializa un [`SealedReadResult`] como comando interno determinista
/// grabable en `ONYXTR2`.
pub fn encode_det_read_command(result: &SealedReadResult) -> String {
    let payload = serde_json::to_string(result).unwrap_or_else(|_| "{}".to_string());
    format!("{DET_READ_CMD_PREFIX}{payload}")
}

/// Decodifica un comando sellado `/__onyx_det_read {...}` (misma disciplina
/// que [`decode_det_shell_command`]).
pub fn decode_det_read_command(raw_cmd: &str) -> Option<SealedReadResult> {
    let payload = raw_cmd.trim().strip_prefix(DET_READ_CMD_PREFIX)?;
    serde_json::from_str(payload).ok()
}

/// Prefijo interno reservado para el comando sellado de `/inject`
/// (fix de auditoría Fase 2): el spawner reinyecta la memoria `user`
/// determinista en-tick (0 I/O, 0 LLM).
pub const DET_INJECT_CMD_PREFIX: &str = "/__onyx_det_inject ";

/// Prefijo interno reservado para el comando sellado de los subcomandos
/// **mutantes** de `/graph` (fix de auditoría Fase 2).
pub const DET_GRAPH_CMD_PREFIX: &str = "/__onyx_det_graph ";

/// Memoria `user` verificada de `/inject <texto>` (sin LLM).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SealedInject {
    pub text: String,
}

/// Operación mutante de `/graph` sellada y reejecutada en-tick por el
/// spawner (función pura de (grafo, comando, tick): bit-a-bit en replay).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op")]
pub enum SealedGraphOp {
    DropMem { start: i64, end: i64 },
    EditMem { seq: i64, text: String },
    SetGoal { text: String },
}

impl SealedGraphOp {
    /// Etiqueta del subcomando (notas accionables y tag del hito en `pack.rs`).
    pub fn label(&self) -> &'static str {
        match self {
            SealedGraphOp::DropMem { .. } => "drop-mem",
            SealedGraphOp::EditMem { .. } => "edit-mem",
            SealedGraphOp::SetGoal { .. } => "set-goal",
        }
    }
}

/// Serializa un [`SealedInject`] como comando interno determinista
/// grabable en `ONYXTR2`.
pub fn encode_det_inject_command(r: &SealedInject) -> String {
    let payload = serde_json::to_string(r).unwrap_or_else(|_| "{}".to_string());
    format!("{DET_INJECT_CMD_PREFIX}{payload}")
}

/// Decodifica un comando sellado `/__onyx_det_inject {...}` (misma
/// disciplina que [`decode_det_shell_command`]).
pub fn decode_det_inject_command(raw_cmd: &str) -> Option<SealedInject> {
    let payload = raw_cmd.trim().strip_prefix(DET_INJECT_CMD_PREFIX)?;
    serde_json::from_str(payload).ok()
}

/// Serializa un [`SealedGraphOp`] como comando interno determinista
/// grabable en `ONYXTR2`.
pub fn encode_det_graph_command(op: &SealedGraphOp) -> String {
    let payload = serde_json::to_string(op).unwrap_or_else(|_| "{}".to_string());
    format!("{DET_GRAPH_CMD_PREFIX}{payload}")
}

/// Decodifica un comando sellado `/__onyx_det_graph {...}` (misma
/// disciplina que [`decode_det_shell_command`]).
pub fn decode_det_graph_command(raw_cmd: &str) -> Option<SealedGraphOp> {
    let payload = raw_cmd.trim().strip_prefix(DET_GRAPH_CMD_PREFIX)?;
    serde_json::from_str(payload).ok()
}

/// Trunca a como mucho `max_bytes` sin partir un carácter `UTF-8`.
pub fn truncate_utf8_safe(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pack::PackSnapshotStore;
    use onyx_soul_agents::AgentConfig;
    use std::fs;
    use std::path::PathBuf;

    fn temp_dir(tag: &str) -> PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!("openonyx-time-debug-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn agent_spec_for_test(goal: &str) -> onyx_soul_agents::AgentSpec {
        onyx_soul_agents::AgentSpec::new("tester", goal).with_tools(
            onyx_workspace::WorkspaceTools::all_tool_names()
                .iter()
                .copied(),
        )
    }

    fn fill_ring(ring: &mut InMemoryTickRing, from: u64, to: u64, size: usize) {
        let payload = vec![b'x'; size];
        for tick in from..=to {
            ring.push(tick, &payload);
        }
    }

    // --- TEST-07.1: ring ---------------------------------------------------

    #[test]
    fn ring_captures_and_evicts_oldest_by_count() {
        let mut ring = InMemoryTickRing::with_limits(4, usize::MAX);
        fill_ring(&mut ring, 0, 9, 8);
        assert_eq!(ring.len(), 4);
        assert_eq!(ring.ticks(), vec![6, 7, 8, 9]);
        assert!(ring.get(5).is_none());
        assert_eq!(ring.get(9), Some([b'x'; 8].as_slice()));
    }

    #[test]
    fn ring_evicts_oldest_by_byte_cap() {
        let mut ring = InMemoryTickRing::with_limits(usize::MAX, 100);
        fill_ring(&mut ring, 0, 9, 30);
        // Estado estacionario: 3 entradas de 30 (90 ≤ 100); la 4ª desborda
        // y evicta siempre la más antigua.
        assert_eq!(ring.ticks(), vec![7, 8, 9]);
        assert_eq!(ring.total_bytes(), 90);
        assert!(ring.total_bytes() <= 100);
    }

    #[test]
    fn ring_single_entry_over_cap_is_kept_alone() {
        let mut ring = InMemoryTickRing::with_limits(4, 10);
        ring.push(0, &[0u8; 12]);
        assert_eq!(ring.ticks(), vec![0]);
        assert_eq!(
            ring.total_bytes(),
            12,
            "una entrada se conserva aunque sola supere el tope"
        );
    }

    #[test]
    fn ring_truncate_after_drops_future() {
        let mut ring = InMemoryTickRing::new();
        fill_ring(&mut ring, 0, 5, 4);
        ring.truncate_after(2);
        assert_eq!(ring.ticks(), vec![0, 1, 2]);
        assert_eq!(ring.total_bytes(), 12);
    }

    #[test]
    fn scrub_invariants_and_auto_freeze() {
        let state = TimeDebugState::new(None).with_ring_limits(64, usize::MAX);
        let payload = vec![b'a'; 16];
        for tick in 0..6 {
            state.record_tick(tick, &payload);
        }
        // `present_tick` = **último tick completado** (5, no 6: no existe tick
        // fantasma), y sin congelación el cursor sigue al presente.
        assert_eq!(state.present(), 5);
        assert_eq!(state.cursor(), 5);

        state.scrub_to(2).unwrap();
        assert!(state.is_frozen(), "scrub a < present auto-congela");
        assert_eq!(state.cursor(), 2);
        assert_eq!(state.present(), 5);

        // Invariante: el futuro no es scrubeable (`present + 1` no existe).
        let err = state.scrub_to(6).unwrap_err();
        assert!(err.0.contains("futuro"), "{err}");

        // Volver al presente descongela.
        state.scrub_to(5).unwrap();
        assert!(!state.is_frozen());
        assert_eq!(state.cursor(), 5);
        assert_eq!(state.cursor(), state.present());

        state.scrub_to(4).unwrap();
        state.return_to_present();
        assert!(!state.is_frozen());
        assert_eq!(state.cursor(), state.present());
    }

    /// Fix de auditoría: `scrub_delta(-n)` puede alcanzar `available[0]`
    /// (`steps == idx`), y desde el presente da el tick inmediatamente
    /// anterior (sin tick fantasma).
    #[test]
    fn scrub_delta_negative_reaches_first_available_tick() {
        let state = TimeDebugState::new(None).with_ring_limits(64, usize::MAX);
        let payload = vec![b'e'; 8];
        for tick in 0..3 {
            state.record_tick(tick, &payload);
        }
        assert_eq!(state.available_ticks(), vec![0, 1, 2]);
        // Desde el presente (2): -1 → 1 (el tick inmediatamente anterior).
        assert_eq!(state.scrub_delta(-1).unwrap(), 1);
        assert!(state.is_frozen());
        // Otro -1 → 0: exactamente `idx` pasos (el caso que fallaba con `>=`).
        assert_eq!(
            state.scrub_delta(-1).unwrap(),
            0,
            "debe alcanzarse available[0] con steps == idx"
        );
        // Un paso más allá del límite inferior: error accionable.
        let err = state.scrub_delta(-1).unwrap_err();
        assert!(err.0.contains("no hay"), "{err}");
        // El límite superior sigue funcionando: desde 0, +2 → presente.
        assert_eq!(state.scrub_delta(2).unwrap(), 2);
        assert!(!state.is_frozen());
    }

    #[test]
    fn available_ticks_is_ring_union_disk_milestones_union_present() {
        let dir = temp_dir("avail");
        let mut store = PackSnapshotStore::new(&dir);
        store.save_milestone(100, "init", &[1, 2, 3]).unwrap();
        store.save_milestone(140, "settled", &[1, 2, 3]).unwrap();
        let shared = SharedPackStore::new(&dir, None);
        let state = TimeDebugState::new(Some(shared));
        let payload = vec![b'b'; 8];
        for tick in 0..4 {
            state.record_tick(tick, &payload);
        }
        // present = 3 (último tick completado, en el ring), hitos = 100/140
        // (imposibles: > present) → solo ring.
        assert_eq!(state.available_ticks(), vec![0, 1, 2, 3]);

        // Hitos en disco dentro del rango: se incluyen.
        let dir2 = temp_dir("avail2");
        let mut store2 = PackSnapshotStore::new(&dir2);
        store2.save_milestone(2, "tool:fs_write", &[9]).unwrap();
        let shared2 = SharedPackStore::new(&dir2, None);
        let state2 = TimeDebugState::new(Some(shared2));
        for tick in 0..5 {
            state2.record_tick(tick, &payload);
        }
        assert_eq!(state2.available_ticks(), vec![0, 1, 2, 3, 4]);
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&dir2);
    }

    #[test]
    fn after_ring_cleared_only_disk_milestones_remain() {
        let dir = temp_dir("cleared");
        let mut store = PackSnapshotStore::new(&dir);
        store.save_milestone(2, "approval:x", &[1]).unwrap();
        store.save_milestone(3, "settled", &[1]).unwrap();
        let shared = SharedPackStore::new(&dir, None);
        let state = TimeDebugState::new(Some(shared));
        let payload = vec![b'c'; 8];
        for tick in 0..4 {
            state.record_tick(tick, &payload);
        }
        state.ring.lock().unwrap().clear();
        // Tras «restart» (ring vacío): solo hitos en disco de esta rama +
        // presente (= 3, que aquí ya es hito).
        assert_eq!(state.available_ticks(), vec![2, 3]);
        assert!(state.tick_available(2));
        assert!(state.tick_available(3));
        assert!(!state.tick_available(1), "tick del ring ya no está");
        let err = state.scrub_to(1).unwrap_err();
        assert!(err.0.contains("no es scrubeable"), "{err}");
        // Scrub al hito en disco funciona y auto-congela.
        state.scrub_to(2).unwrap();
        assert!(state.is_frozen());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn purged_tick_returns_actionable_error_without_panic() {
        let state = TimeDebugState::new(None).with_ring_limits(3, usize::MAX);
        let payload = vec![b'd'; 8];
        for tick in 0..10 {
            state.record_tick(tick, &payload);
        }
        // El tick 0 fue purgado por el tope K=3.
        let err = state.scrub_to(0).unwrap_err();
        assert!(err.0.contains("no es scrubeable"), "{err}");
        assert!(err.0.contains("disponibles"), "{err}");
        // El preview también da error accionable.
        let err = state.preview_graph(0).err().expect("debe fallar");
        assert!(err.0.contains("no hay bytes"), "{err}");
    }

    // --- TEST-07.2: preview es no-mutante ----------------------------------

    #[test]
    fn preview_navigation_does_not_touch_disk_host_or_pack() {
        let dir = temp_dir("preview");
        let policy = onyx_workspace::WorkspacePolicy::new(&dir).unwrap();
        let tools = onyx_workspace::WorkspaceTools::new(policy);
        let (mut host, ws_vocab) =
            crate::session::build_host(1, AgentConfig::default(), Some(&dir), None).unwrap();
        host.spawn(&agent_spec_for_test("write a file"));

        let state = TimeDebugState::new(None);
        let ws_bridge = onyx_workspace::WorkspaceToolBridge::new(host.vocab(), tools)
            .with_mode(onyx_workspace::WorkspaceMode::Build)
            .with_workspace_vocab(ws_vocab)
            .require_read_before_edit(true)
            .wrapping(onyx_soul_agents::HeadlessDriver::new());
        let mut driver = onyx_soul_agents::RecordingBridge::new(
            host.vocab(),
            |req: &onyx_soul_agents::AgentView| {
                if req.memory.iter().any(|(k, _)| k == "observe") {
                    Some(onyx_soul_agents::Reply::done(
                        req.agent,
                        req.request,
                        "done",
                    ))
                } else if req.memory.iter().any(|(k, _)| k == "call") {
                    None
                } else {
                    Some(onyx_soul_agents::Reply::call_structured(
                        req.agent,
                        req.request,
                        "fs_write",
                        r#"{"path":"out.txt","content":"hello"}"#,
                    ))
                }
            },
        )
        .wrapping(ws_bridge);

        let mut guard = 0;
        while guard < 16 && (!host.is_settled() || host.ticks() == 0) {
            host.step(&mut driver);
            guard += 1;
            state.record_tick(host.ticks() - 1, &host.graph().export());
        }
        assert!(host.is_settled());
        let present = state.present();
        assert!(
            present >= 3,
            "la sesión debe tener varios ticks ({present})"
        );
        // Línea de base tras asentar: la navegación preview no debe moverla.
        let live_fp_before = host.fingerprint();
        let files_before: Vec<PathBuf> = fs::read_dir(&dir)
            .map(|rd| rd.flatten().map(|e| e.path()).collect())
            .unwrap_or_default();

        state.freeze();
        // El anillo en el presente (último tick completado) coincide con el
        // grafo vivo: el snapshot se capturó justo después del último step
        // (consistencia del anillo RAM).
        let preview_present = state.preview_graph(present).unwrap();
        assert_eq!(
            preview_present.export(),
            host.graph().export(),
            "el anillo en el presente debe coincidir con el grafo vivo"
        );
        // Un tick atrás sí debe diferir (el grafo acumula conceptos).
        let earlier = present - 1;
        state.scrub_to(earlier).unwrap();
        assert!(state.is_frozen());
        let preview_earlier = state.preview_graph(earlier).unwrap();
        assert_ne!(
            preview_earlier.export(),
            host.graph().export(),
            "preview de tick {earlier} debe diferir del grafo vivo"
        );
        // Y no ha tocado ni el host ni el disco (ni snapshot/rama en el pack).
        assert_eq!(host.fingerprint(), live_fp_before);
        let files_now: Vec<PathBuf> = fs::read_dir(&dir)
            .map(|rd| rd.flatten().map(|e| e.path()).collect())
            .unwrap_or_default();
        assert_eq!(files_now, files_before);

        // Más navegación (con caché LRU) tampoco muta nada.
        state.scrub_delta(-1).unwrap();
        state.scrub_delta(1).unwrap();
        assert_eq!(host.fingerprint(), live_fp_before);

        state.return_to_present();
        assert!(!state.is_frozen());
        assert_eq!(state.cursor(), state.present());
        assert_eq!(host.fingerprint(), live_fp_before);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn preview_cache_is_lru_bounded_to_eight() {
        let state = TimeDebugState::new(None).with_ring_limits(64, usize::MAX);
        // El payload debe ser un export `ONYXAG` válido (el preview lo importa).
        let payload = onyx_soul_agents::AgentGraph::new().export();
        for tick in 0..12 {
            state.record_tick(tick, &payload);
        }
        for tick in 0..12 {
            state.preview_graph(tick).unwrap();
        }
        let cache = state.preview_cache.lock().unwrap();
        assert_eq!(cache.len(), PREVIEW_CACHE_CAPACITY);
        drop(cache);
        // LRU: el más reciente sigue vivo, el más antiguo se evictó.
        assert!(state.preview_graph(11).is_ok());
        // Tras 12 inserciones, el 0 se evictó: se vuelve a cargar desde el ring.
        let _ = state.preview_graph(0).unwrap();
    }

    // --- Gate: try_acquire_llm_turn ----------------------------------------

    /// Fix de auditoría Fase 2: semántica del cursor en `record_tick`.
    #[test]
    fn record_tick_cursor_follows_present_at_frontier_and_anchors_in_past() {
        let state = TimeDebugState::new(None);
        let payload = onyx_soul_agents::AgentGraph::new().export();
        // Sin congelación: el cursor sigue al presente (= último tick
        // completado).
        for tick in 0..2u64 {
            state.record_tick(tick, &payload);
        }
        assert_eq!(state.present(), 1);
        assert_eq!(state.cursor(), 1);

        // (a) Congelado EN el presente (`cursor == present`): `record_tick`
        // avanza ambos manteniendo `frozen == true` (0 bifurcaciones).
        state.freeze();
        state.record_tick(2, &payload);
        assert!(state.is_frozen(), "no debe descongelar");
        assert_eq!(state.present(), 2);
        assert_eq!(
            state.cursor(),
            2,
            "congelado en el presente: el cursor sigue al presente"
        );

        // (b) Congelado MIRANDO el pasado (`cursor < present`): `record_tick`
        // deja el cursor anclado (la siguiente mutación bifurcará desde él).
        state.scrub_to(0).unwrap();
        assert!(state.is_frozen());
        assert_eq!(state.cursor(), 0);
        state.record_tick(3, &payload);
        assert!(state.is_frozen());
        assert_eq!(state.present(), 3);
        assert_eq!(
            state.cursor(),
            0,
            "congelado mirando el pasado: el cursor queda anclado"
        );
    }

    #[test]
    fn llm_turn_gate_unfrozen_frozen_budget() {
        let state = TimeDebugState::new(None);
        assert!(
            state.try_acquire_llm_turn(),
            "sin congelación siempre despacha"
        );
        assert_eq!(
            state.step_llm_budget.load(Ordering::Acquire),
            0,
            "no consume presupuesto"
        );

        state.freeze();
        assert!(
            !state.try_acquire_llm_turn(),
            "congelado sin presupuesto: 0 despachos"
        );
        state.grant_step_llm(1);
        assert!(
            state.try_acquire_llm_turn(),
            "presupuesto 1: consume 1 turno"
        );
        assert!(!state.try_acquire_llm_turn(), "y re-bloquea al instante");
        state.freeze();
        state.grant_step_llm(3);
        assert!(state.try_acquire_llm_turn());
        assert!(state.try_acquire_llm_turn());
        assert!(state.try_acquire_llm_turn());
        assert!(!state.try_acquire_llm_turn());

        state.unfreeze();
        assert!(
            state.try_acquire_llm_turn(),
            "descongelado despacha de nuevo"
        );
        assert_eq!(
            state.step_llm_budget.load(Ordering::Acquire),
            0,
            "unfreeze descarta el presupuesto residual"
        );
    }

    // --- check_mutation_gate ------------------------------------------------

    #[test]
    fn mutation_gate_allows_thinking_but_rejects_tool_calls_inflight_and_subagents() {
        let (mut host, _ws_vocab) =
            crate::session::build_host(1, AgentConfig::default(), None, None).unwrap();
        let v = host.vocab();
        // Grafo vacío: gate abierto.
        let ok = check_mutation_gate(host.graph(), &v, 0, 0).unwrap();
        assert!(ok.allowed());

        // Fix de auditoría: agente `Thinking` con `llm_in_flight == 0`
        // (breakpoint pre-LLM) **sí** permite mutar.
        host.spawn(&onyx_soul_agents::AgentSpec::new("tester", "wait"));
        let mut driver = onyx_soul_agents::HeadlessDriver::new();
        host.step(&mut driver);
        let ok = check_mutation_gate(host.graph(), &v, 0, 0).unwrap();
        assert_eq!(ok.pending_tool_calls, 0);
        assert!(ok.allowed());

        // Agente `Acting` con tool call pendiente → bloqueado (el tool debe
        // estar en el spec para que el runtime acepte la llamada).
        host.spawn(
            &onyx_soul_agents::AgentSpec::new("worker", "run a tool")
                .with_tools(["fs_write"].iter().copied()),
        );
        let mut driver = onyx_soul_agents::RecordingBridge::new(
            host.vocab(),
            |req: &onyx_soul_agents::AgentView| {
                if req.memory.iter().any(|(k, _)| k == "call") {
                    None
                } else {
                    Some(onyx_soul_agents::Reply::call_structured(
                        req.agent,
                        req.request,
                        "fs_write",
                        r#"{"path":"x.txt","content":"x"}"#,
                    ))
                }
            },
        );
        // Avanza hasta que la tool call quede pendiente (el registro puede
        // tardar 1 tick).
        for _ in 0..4 {
            if onyx_soul_agents::pending_tool_calls(host.graph(), &v).is_empty() {
                host.step(&mut driver);
            } else {
                break;
            }
        }
        let err = check_mutation_gate(host.graph(), &v, 0, 0).unwrap_err();
        assert_eq!(err.pending_tool_calls, 1);
        assert!(err.reason().unwrap().contains("tool"), "{:?}", err.reason());

        // LLM en vuelo / subagente en ejecución → razón correspondiente.
        let err = check_mutation_gate(host.graph(), &v, 2, 0).unwrap_err();
        assert_eq!(err.llm_in_flight, 2);
        assert!(err.reason().unwrap().contains("vuelo"));
        let err = check_mutation_gate(host.graph(), &v, 0, 1).unwrap_err();
        assert!(err.reason().unwrap().contains("subagente"));
    }

    // --- Bifurcación desde tick (solo RAM, sin --snapshot-dir) --------------

    fn git_init(dir: &Path) {
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                .output()
                .expect("git");
            assert!(
                out.status.success(),
                "git {args:?} → {:?}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "t@local"]);
        run(&["config", "user.name", "t"]);
        fs::write(dir.join("seed.txt"), b"v1").unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-q", "-m", "seed"]);
    }

    #[test]
    fn branch_from_cursor_ram_only_reverts_via_tree_sha_and_truncates_ring() {
        let dir = temp_dir("branch");
        git_init(&dir);

        let policy = onyx_workspace::WorkspacePolicy::new(&dir).unwrap();
        let tools = onyx_workspace::WorkspaceTools::new(policy);
        let opts = crate::session::SessionOptions::default();
        let state = TimeDebugState::new(None);
        let td_store = TimeDebugSnapshotStore::new(None, state.ring_arc());
        let (mut host, ws_vocab) = crate::session::build_host_with_shared_store(
            1,
            AgentConfig::default(),
            &opts,
            None,
            Some(onyx_workspace::PendingCheckpoints::default()),
            None,
            Some(td_store),
        )
        .unwrap();
        host.spawn(&agent_spec_for_test("write a file"));

        let ws_bridge = onyx_workspace::WorkspaceToolBridge::new(host.vocab(), tools.clone())
            .with_mode(onyx_workspace::WorkspaceMode::Build)
            .with_workspace_vocab(ws_vocab)
            .require_read_before_edit(true)
            .wrapping(onyx_soul_agents::HeadlessDriver::new());
        let mut driver = onyx_soul_agents::RecordingBridge::new(
            host.vocab(),
            |req: &onyx_soul_agents::AgentView| {
                if req.memory.iter().any(|(k, _)| k == "observe") {
                    Some(onyx_soul_agents::Reply::done(
                        req.agent,
                        req.request,
                        "done",
                    ))
                } else if req.memory.iter().any(|(k, _)| k == "call") {
                    None
                } else {
                    Some(onyx_soul_agents::Reply::call_structured(
                        req.agent,
                        req.request,
                        "fs_write",
                        r#"{"path":"out.txt","content":"hello"}"#,
                    ))
                }
            },
        )
        .wrapping(ws_bridge);

        let mut guard = 0;
        while guard < 16 && (!host.is_settled() || host.ticks() == 0) {
            host.step(&mut driver);
            guard += 1;
            state.record_tick(host.ticks() - 1, &host.graph().export());
        }
        assert!(host.is_settled());

        // El efecto `fs_write` tiene `tree_sha` (workspace Git) → cursor justo antes.
        let effects = onyx_workspace::effects_after_tick(host.graph(), &ws_vocab, 0);
        assert_eq!(effects.len(), 1);
        let write_tick = effects[0].tick;
        assert!(
            effects[0].tree_sha.is_some(),
            "el checkpoint Git debe llevar tree_sha"
        );
        let cursor = write_tick.saturating_sub(1);
        assert!(state.tick_available(cursor));

        state.scrub_to(cursor).unwrap();
        assert!(state.is_frozen());

        // `pending_revert_files`: solo RAM, 0 I/O.
        let pending = state.pending_revert_files(host.graph(), &ws_vocab);
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].target, "out.txt");
        assert!(pending[0].tree_sha.is_some());

        let preview = state.preview_graph(cursor).unwrap();
        let preview_count = preview.query(&[]).len();

        let summary =
            confirm_branch_from_cursor(&mut host, &state, &dir, &ws_vocab, &tools, None, 0, 0)
                .unwrap();
        assert!(summary.contains("Branched from tick"), "{summary}");
        assert!(summary.contains("Restored git tree"), "{summary}");

        // 1. El grafo vivo es el del cursor (el futuro dejó de existir).
        assert_eq!(host.ticks(), cursor + 1);
        assert_eq!(state.present(), cursor, "present_tick = cursor");
        assert_eq!(state.cursor(), cursor);
        assert!(state.is_frozen(), "sigue congelado tras bifurcar");
        assert_eq!(
            host.graph().query(&[]).len(),
            preview_count,
            "el grafo restaurado debe coincidir con el preview del cursor"
        );
        // 2. El ring perdió el futuro.
        let ring_ticks = state.ring.lock().unwrap().ticks();
        assert!(ring_ticks.iter().all(|&t| t <= cursor), "{ring_ticks:?}");
        assert_eq!(state.preview_cache.lock().unwrap().len(), 0, "caché limpia");
        // 3. Git: `out.txt` (creado tras el checkpoint) desapareció; `seed.txt` intacto.
        assert!(!dir.join("out.txt").exists());
        assert_eq!(fs::read_to_string(dir.join("seed.txt")).unwrap(), "v1");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn branch_gate_rejects_while_llm_in_flight() {
        let dir = temp_dir("branchgate");
        let policy = onyx_workspace::WorkspacePolicy::new(&dir).unwrap();
        let tools = onyx_workspace::WorkspaceTools::new(policy);
        let (mut host, ws_vocab) =
            crate::session::build_host(1, AgentConfig::default(), None, None).unwrap();
        let spec = onyx_soul_agents::AgentSpec::new("tester", "wait");
        host.spawn(&spec);
        let mut driver = onyx_soul_agents::HeadlessDriver::new();
        host.step(&mut driver); // tick 0
        let state = TimeDebugState::new(None);
        state.record_tick(0, &host.graph().export());
        host.step(&mut driver); // tick 1: el agente sigue `Thinking`
        state.record_tick(1, &host.graph().export());
        state.scrub_to(0).unwrap();
        assert!(state.is_frozen());
        let err =
            confirm_branch_from_cursor(&mut host, &state, &dir, &ws_vocab, &tools, None, 1, 0)
                .unwrap_err();
        assert!(err.0.contains("mutación bloqueada"), "{err}");
        let err =
            confirm_branch_from_cursor(&mut host, &state, &dir, &ws_vocab, &tools, None, 0, 2)
                .unwrap_err();
        assert!(err.0.contains("subagente"), "{err}");
        let _ = fs::remove_dir_all(&dir);
    }

    // --- TimeDebugSnapshotStore: load ring-first, save solo disco -----------

    #[test]
    fn snapshot_store_loads_ring_first_and_saves_to_pack_only() {
        let dir = temp_dir("tdstore");
        let shared = SharedPackStore::new(&dir, None);
        let state = TimeDebugState::new(Some(shared.clone()));
        let mut store = TimeDebugSnapshotStore::new(Some(shared), state.ring_arc());

        // El pack no tiene el tick 3, pero el ring sí.
        state.record_tick(3, &[7, 7, 7]);
        let bytes = store.load("3").unwrap();
        assert_eq!(bytes, vec![7, 7, 7]);

        // `save` va solo al disco (nunca al ring).
        store.save("9", &[42]).unwrap();
        assert!(store.load("9").unwrap() == vec![42]);
        assert!(
            state.ring.lock().unwrap().get(9).is_none(),
            "save nunca escribe en el anillo RAM"
        );

        // Ni ring ni disco → error accionable (nunca pánico).
        let err = store.load("123").unwrap_err();
        assert!(
            matches!(err, OnyxError::Missing | OnyxError::Format(_)),
            "{err:?}"
        );

        // Sin pack: `save` da error accionable (la RAM no es `.onyxpack`).
        let mut ring_only = TimeDebugSnapshotStore::new(None, state.ring_arc());
        let err = ring_only.save("5", &[1]).unwrap_err();
        assert!(matches!(err, OnyxError::Format(_)), "{err:?}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn host_restore_reads_from_ring_when_pack_is_empty() {
        let opts = crate::session::SessionOptions::default();
        let state = TimeDebugState::new(None);
        let td_store = TimeDebugSnapshotStore::new(None, state.ring_arc());
        let (mut host, _ws) = crate::session::build_host_with_shared_store(
            1,
            AgentConfig::default(),
            &opts,
            None,
            None,
            None,
            Some(td_store),
        )
        .unwrap();
        let mut driver = onyx_soul_agents::HeadlessDriver::new();
        host.spawn(&onyx_soul_agents::AgentSpec::new("alpha", "a"));
        host.step(&mut driver); // tick 0: agente `alpha` creado
        state.record_tick(0, &host.graph().export());
        host.spawn(&onyx_soul_agents::AgentSpec::new("beta", "b"));
        host.step(&mut driver); // tick 1: también existe `beta`
        state.record_tick(1, &host.graph().export());
        assert_eq!(host.agents().len(), 2);

        // `request_restore` con store solo-RAM (sin pack en disco): los bytes
        // deben salir del anillo, no del disco.
        host.request_restore("0");
        host.step(&mut driver);
        host.set_tick(1);
        assert_eq!(
            host.agents().len(),
            1,
            "el grafo debe restaurarse desde el anillo RAM (solo `alpha`)"
        );
    }

    // --- TEST-07.2: codecs sellados (`SDD-07` Fase 2) ------------------------

    #[test]
    fn det_shell_codec_round_trips_and_rejects_corruption() {
        let result = SealedShellResult {
            command: "cargo test".to_string(),
            exit_code: Some(0),
            timed_out: false,
            output: "exit_code: 0\nstdout:\nok\nstderr:\n".to_string(),
        };
        let wire = encode_det_shell_command(&result);
        assert!(wire.starts_with(DET_SHELL_CMD_PREFIX), "{wire}");
        assert_eq!(decode_det_shell_command(&wire), Some(result.clone()));
        // Idempotencia del decode: la segunda pasada devuelve el mismo valor.
        assert_eq!(
            decode_det_shell_command(&wire),
            decode_det_shell_command(&wire)
        );

        // Payload corrupto / malformado → `None` (nunca pánico).
        assert_eq!(decode_det_shell_command("/__onyx_det_shell {no json"), None);
        assert_eq!(
            decode_det_shell_command("/__onyx_det_shell {\"command\":"),
            None
        );
        assert_eq!(
            decode_det_shell_command("/__onyx_det_shell {\"command\":1}"),
            None,
            "tipos incorrectos se rechazan"
        );
        assert_eq!(decode_det_shell_command("/__onyx_det_read {}"), None);
        assert_eq!(decode_det_shell_command("/new"), None);
        assert_eq!(decode_det_shell_command(""), None);
    }

    #[test]
    fn det_read_codec_round_trips_and_rejects_corruption() {
        let result = SealedReadResult {
            path: "notes.md".to_string(),
            start_line: Some(2),
            end_line: Some(5),
            content: "line2\nline3\nline4\nline5".to_string(),
        };
        let wire = encode_det_read_command(&result);
        assert!(wire.starts_with(DET_READ_CMD_PREFIX), "{wire}");
        assert_eq!(decode_det_read_command(&wire), Some(result.clone()));
        assert_eq!(
            decode_det_read_command(&wire),
            decode_det_read_command(&wire)
        );

        assert_eq!(decode_det_read_command("/__onyx_det_read [1,2"), None);
        assert_eq!(
            decode_det_read_command("/__onyx_det_read {\"path\":42}"),
            None
        );
        assert_eq!(decode_det_read_command("/__onyx_det_shell {}"), None);
        assert_eq!(decode_det_read_command("/read notes.md"), None);
    }

    /// Fix de auditoría Fase 2: codecs de `/inject` y `/graph` sellados.
    #[test]
    fn det_inject_and_graph_codecs_round_trip_and_reject_corruption() {
        let inj = SealedInject {
            text: "remember: use `--locked` in CI".to_string(),
        };
        let wire_inj = encode_det_inject_command(&inj);
        assert!(wire_inj.starts_with(DET_INJECT_CMD_PREFIX), "{wire_inj}");
        assert_eq!(decode_det_inject_command(&wire_inj), Some(inj.clone()));
        assert_eq!(
            decode_det_inject_command("/__onyx_det_inject {\"text\":"),
            None
        );
        assert_eq!(
            decode_det_inject_command("/__onyx_det_inject {\"n\":1}"),
            None
        );
        assert_eq!(decode_det_inject_command("/inject hi"), None);

        for op in [
            SealedGraphOp::DropMem { start: 1, end: 2 },
            SealedGraphOp::EditMem {
                seq: 3,
                text: "rewrote".to_string(),
            },
            SealedGraphOp::SetGoal {
                text: "brand new goal".to_string(),
            },
        ] {
            let wire = encode_det_graph_command(&op);
            assert!(wire.starts_with(DET_GRAPH_CMD_PREFIX), "{wire}");
            assert_eq!(decode_det_graph_command(&wire), Some(op.clone()));
        }
        // Etiquetas estables (tag del hito `cmd:/graph-mutate` en `pack.rs`).
        assert_eq!(
            SealedGraphOp::DropMem { start: 1, end: 2 }.label(),
            "drop-mem"
        );
        assert_eq!(
            SealedGraphOp::EditMem {
                seq: 1,
                text: "x".into()
            }
            .label(),
            "edit-mem"
        );
        assert_eq!(
            SealedGraphOp::SetGoal { text: "x".into() }.label(),
            "set-goal"
        );
        assert_eq!(decode_det_graph_command("/__onyx_det_graph [1,2"), None);
        assert_eq!(
            decode_det_graph_command("/__onyx_det_graph {\"op\":\"Inspect\"}"),
            None
        );
        assert_eq!(decode_det_graph_command("/__onyx_det_inject {}"), None);
        assert_eq!(decode_det_graph_command("/graph drop-mem 1"), None);
    }

    #[test]
    fn truncate_utf8_safe_never_splits_multibyte() {
        // `hé` = 3 bytes; truncar a 4 debe dejar `hél` (4 bytes), no partir `ó`.
        let s = "héllo wörld";
        assert_eq!(truncate_utf8_safe(s, 4), "hél");
        assert_eq!(truncate_utf8_safe(s, 4).len(), 4);
        // Truncar a 2 deja `h` (1 byte), no medio carácter.
        assert_eq!(truncate_utf8_safe(s, 2), "h");
        // Sin traspaso de cap → cadena idéntica.
        assert_eq!(truncate_utf8_safe(s, 1024), s);
        // Cap 0 → cadena vacía.
        assert_eq!(truncate_utf8_safe(s, 0), "");
    }
}
