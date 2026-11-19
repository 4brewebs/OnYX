use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

use onyx_soul_agents::{
    Attr, AttrRegistry, ConceptId, Ctx, Graph, GraphView, OnyxError, OnyxEvent, OnyxSystem,
    SystemEvent, Value, Vocab,
};
use serde_json::Value as JsonValue;

/// Modo operativo del agente sobre el repositorio.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkspaceMode {
    /// Solo lectura y análisis (`fs_read`, `fs_list`, `code_search`).
    Plan,
    /// Lectura y modificación controlada (`fs_edit` exige `fs_read` previo si se habilita).
    Build,
    /// Exploración rápida de solo lectura.
    Explore,
}

impl WorkspaceMode {
    pub fn allows_mutation(self) -> bool {
        matches!(self, Self::Build)
    }

    pub fn as_label(self) -> &'static str {
        match self {
            Self::Plan => "PLAN",
            Self::Build => "BUILD",
            Self::Explore => "EXPLORE",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_uppercase().as_str() {
            "PLAN" => Some(Self::Plan),
            "BUILD" => Some(Self::Build),
            "EXPLORE" => Some(Self::Explore),
            _ => None,
        }
    }

    fn to_u8(self) -> u8 {
        match self {
            Self::Plan => 0,
            Self::Build => 1,
            Self::Explore => 2,
        }
    }

    fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::Plan,
            2 => Self::Explore,
            _ => Self::Build,
        }
    }
}

/// Handle compartible y thread-safe del `WorkspaceMode` actual para alternar en caliente (e.g. `Tab` en TUI).
#[derive(Clone, Debug)]
pub struct WorkspaceModeHandle(Arc<AtomicU8>);

impl WorkspaceModeHandle {
    pub fn new(mode: WorkspaceMode) -> Self {
        Self(Arc::new(AtomicU8::new(mode.to_u8())))
    }

    pub fn get(&self) -> WorkspaceMode {
        WorkspaceMode::from_u8(self.0.load(Ordering::Relaxed))
    }

    pub fn set(&self, mode: WorkspaceMode) {
        self.0.store(mode.to_u8(), Ordering::Relaxed);
    }

    pub fn toggle_plan_build(&self) -> WorkspaceMode {
        let next = match self.get() {
            WorkspaceMode::Build => WorkspaceMode::Plan,
            WorkspaceMode::Plan | WorkspaceMode::Explore => WorkspaceMode::Build,
        };
        self.set(next);
        next
    }
}

impl Default for WorkspaceModeHandle {
    fn default() -> Self {
        Self::new(WorkspaceMode::Build)
    }
}

/// Clasificación física de un efecto secundario externo para reconciliación en `/rewind`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EffectReversibility {
    /// Deshacible automáticamente (ej. edición local con checkpoint o `git commit`).
    AutoReversible,
    /// Requiere acción compensatoria explícita (ej. `git push`, `git merge`).
    Compensable,
    /// Efecto externo opaco o no reversible automáticamente (ej. `shell_exec` arbitrario).
    Irreversible,
}

impl EffectReversibility {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AutoReversible => "auto_reversible",
            Self::Compensable => "compensable",
            Self::Irreversible => "irreversible",
        }
    }
}

/// Vocabulario EAV registrado por `onyx-workspace` en el grafo del host.
#[derive(Clone, Copy, Debug)]
pub struct WorkspaceVocab {
    pub mode: Attr,
    pub plan_text: Attr,
    pub file_path: Attr,
    pub file_read_tick: Attr,
    pub file_dirty: Attr,
    pub file_hash: Attr,
    pub effect_tick: Attr,
    pub effect_agent: Attr,
    pub effect_tool: Attr,
    pub effect_target: Attr,
    pub effect_reversibility: Attr,
    pub effect_summary: Attr,
    pub effect_head_sha: Attr,
    pub effect_tree_sha: Attr,
    pub effect_undone: Attr,
}

impl WorkspaceVocab {
    pub fn resolve(attrs: &mut dyn AttrRegistry) -> Self {
        Self::resolve_with(|name| attrs.attr(name))
    }

    pub fn resolve_with(mut reg: impl FnMut(&str) -> Attr) -> Self {
        Self {
            mode: reg("ws.mode"),
            plan_text: reg("ws.plan.text"),
            file_path: reg("ws.file.path"),
            file_read_tick: reg("ws.file.read_tick"),
            file_dirty: reg("ws.file.dirty"),
            file_hash: reg("ws.file.hash"),
            effect_tick: reg("ws.effect.tick"),
            effect_agent: reg("ws.effect.agent"),
            effect_tool: reg("ws.effect.tool"),
            effect_target: reg("ws.effect.target"),
            effect_reversibility: reg("ws.effect.reversibility"),
            effect_summary: reg("ws.effect.summary"),
            effect_head_sha: reg("ws.effect.head_sha"),
            effect_tree_sha: reg("ws.effect.tree_sha"),
            effect_undone: reg("ws.effect.undone"),
        }
    }
}

/// Representación de solo lectura de un efecto registrado en el ledger EAV.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordedEffect {
    pub id: ConceptId,
    pub tick: u64,
    pub agent: u64,
    pub tool: String,
    pub target: String,
    pub reversibility: String,
    pub summary: String,
    pub head_sha: Option<String>,
    pub tree_sha: Option<String>,
}

/// Mapa compartido entre `WorkspaceToolBridge` (que captura `git write-tree`
/// antes de mutar disco) y `WorkspaceLedgerSystem` (que sella el checkpoint en el grafo).
pub type PendingCheckpoints = Arc<Mutex<HashMap<(u64, String, String), (String, String)>>>;

/// Captura `(head_sha, tree_sha)` del árbol de trabajo actual sin mutar `HEAD`,
/// el índice real de Git ni crear commits (`GIT_INDEX_FILE` temporal + `git add -A` + `git write-tree`).
/// Funciona incluso antes del primer commit (`head_sha` vacío).
/// Devuelve `None` si `root` no es un repositorio Git válido.
pub fn capture_git_tree_checkpoint(root: &Path) -> Option<(String, String)> {
    static CKPT_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    let in_repo = Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(root)
        .output()
        .ok()
        .is_some_and(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).trim() == "true");
    if !in_repo {
        return None;
    }

    let head_sha = Command::new("git")
        .args(["rev-parse", "--verify", "HEAD"])
        .current_dir(root)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = CKPT_SEQ.fetch_add(1, Ordering::Relaxed);
    let temp_index =
        std::env::temp_dir().join(format!("onyx_git_idx_{}_{nanos}_{seq}", std::process::id()));

    let add_ok = Command::new("git")
        .args([
            "add",
            "-A",
            "--",
            ".",
            ":(exclude)*.onyxpack",
            ":(exclude)*.onyxpack.tmp",
            ":(exclude)tick-*.bin",
            ":(exclude)*/tick-*.bin",
            ":(exclude)*.tr",
        ])
        .env("GIT_INDEX_FILE", &temp_index)
        .current_dir(root)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !add_ok {
        let _ = std::fs::remove_file(&temp_index);
        return None;
    }

    let tree_out = Command::new("git")
        .args(["write-tree"])
        .env("GIT_INDEX_FILE", &temp_index)
        .current_dir(root)
        .output()
        .ok();
    let _ = std::fs::remove_file(&temp_index);

    let tree_out = tree_out?;
    if !tree_out.status.success() {
        return None;
    }
    let tree_sha = String::from_utf8_lossy(&tree_out.stdout).trim().to_string();
    if tree_sha.is_empty() {
        None
    } else {
        Some((head_sha, tree_sha))
    }
}

pub(crate) fn parse_inline_checkpoint(obs_text: &str) -> Option<(String, String)> {
    let marker = " [checkpoint:";
    let start = obs_text.rfind(marker)? + marker.len();
    let rest = &obs_text[start..];
    let end = rest.find(']')?;
    let payload = &rest[..end];
    let (head, tree) = payload.split_once(':')?;
    let head = head.trim();
    let tree = tree.trim();
    if tree.is_empty() || !tree.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    if !head.is_empty() && !head.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some((head.to_string(), tree.to_string()))
}

/// Restaura el árbol de trabajo en `root` al estado exacto capturado en `tree_sha`
/// sin reescribir la historia de commits (`git read-tree --reset -u <tree_sha>`).
pub fn restore_git_tree_checkpoint(root: &Path, tree_sha: &str) -> Result<(), String> {
    let out = Command::new("git")
        .args(["read-tree", "--reset", "-u", tree_sha])
        .current_dir(root)
        .output()
        .map_err(|e| format!("failed to run git read-tree: {e}"))?;
    if !out.status.success() {
        return Err(format!("git read-tree --reset -u {tree_sha} failed"));
    }
    let has_head = Command::new("git")
        .args(["rev-parse", "--verify", "HEAD"])
        .current_dir(root)
        .output()
        .is_ok_and(|o| o.status.success());
    if has_head {
        let _ = Command::new("git")
            .args(["reset", "--quiet", "HEAD"])
            .current_dir(root)
            .output();
    }
    Ok(())
}

/// Consulta en el grafo si un archivo relativo ya fue leído en la sesión.
pub fn is_file_read(graph: &dyn GraphView, ws: &WorkspaceVocab, rel_path: &str) -> bool {
    file_read_tick(graph, ws, rel_path).is_some()
}

/// Devuelve el tick más reciente de lectura registrada para `rel_path`, si existe.
pub fn file_read_tick(graph: &dyn GraphView, ws: &WorkspaceVocab, rel_path: &str) -> Option<i64> {
    let norm = normalize_rel_path(rel_path);
    for id in graph.query(&[ws.file_path, ws.file_read_tick]) {
        if let Some(Value::Text(p)) = graph.get(id, ws.file_path) {
            if p == norm {
                if let Some(Value::Int(t)) = graph.get(id, ws.file_read_tick) {
                    return Some(t);
                }
            }
        }
    }
    None
}

fn read_recorded_effect<G: GraphView + ?Sized>(
    graph: &G,
    ws: &WorkspaceVocab,
    id: ConceptId,
) -> Option<RecordedEffect> {
    let Value::Int(t) = graph.get(id, ws.effect_tick)? else {
        return None;
    };
    let tick = u64::try_from(t).unwrap_or(0);
    let agent = match graph.get(id, ws.effect_agent) {
        Some(Value::Int(a)) => u64::try_from(a).unwrap_or(0),
        _ => 0,
    };
    let tool = prop_text(graph, id, ws.effect_tool).unwrap_or_default();
    let target = prop_text(graph, id, ws.effect_target).unwrap_or_default();
    let reversibility = prop_text(graph, id, ws.effect_reversibility).unwrap_or_default();
    let summary = prop_text(graph, id, ws.effect_summary).unwrap_or_default();
    let head_sha = prop_text(graph, id, ws.effect_head_sha).filter(|s| !s.is_empty());
    let tree_sha = prop_text(graph, id, ws.effect_tree_sha).filter(|s| !s.is_empty());
    Some(RecordedEffect {
        id,
        tick,
        agent,
        tool,
        target,
        reversibility,
        summary,
        head_sha,
        tree_sha,
    })
}

/// Devuelve todos los efectos externos registrados después de `after_tick`
/// para construir el informe de reconciliación antes de un `/rewind`.
pub fn effects_after_tick<G: GraphView + ?Sized>(
    graph: &G,
    ws: &WorkspaceVocab,
    after_tick: u64,
) -> Vec<RecordedEffect> {
    let mut out = all_recorded_effects(graph, ws);
    out.retain(|e| e.tick > after_tick);
    out
}

/// Devuelve todos los efectos externos registrados en el ledger EAV ordenados por `(tick, id)`.
pub fn all_recorded_effects<G: GraphView + ?Sized>(
    graph: &G,
    ws: &WorkspaceVocab,
) -> Vec<RecordedEffect> {
    let mut out = Vec::new();
    for id in graph.query(&[ws.effect_tick, ws.effect_tool]) {
        if let Some(eff) = read_recorded_effect(graph, ws, id) {
            out.push(eff);
        }
    }
    out.sort_by_key(|e| (e.tick, e.id.0));
    out
}

/// Devuelve si el efecto indicado fue marcado como deshecho (`ws.effect.undone == true`).
pub fn is_effect_undone<G: GraphView + ?Sized>(
    graph: &G,
    ws: &WorkspaceVocab,
    effect_id: ConceptId,
) -> bool {
    matches!(
        graph.get(effect_id, ws.effect_undone),
        Some(Value::Bool(true))
    )
}

/// Devuelve el último efecto con `tree_sha` que todavía no haya sido deshecho (`effect_undone != true`).
pub fn latest_undoable_effect<G: GraphView + ?Sized>(
    graph: &G,
    ws: &WorkspaceVocab,
) -> Option<RecordedEffect> {
    let mut best: Option<RecordedEffect> = None;
    for id in graph.query(&[ws.effect_tick, ws.effect_tool]) {
        if matches!(graph.get(id, ws.effect_undone), Some(Value::Bool(true))) {
            continue;
        }
        let Some(eff) = read_recorded_effect(graph, ws, id) else {
            continue;
        };
        if eff.tree_sha.is_none() {
            continue;
        }
        let replace = match &best {
            None => true,
            Some(prev) => (eff.tick, eff.id.0) > (prev.tick, prev.id.0),
        };
        if replace {
            best = Some(eff);
        }
    }
    best
}

/// Persiste el `WorkspaceMode` actual en el grafo EAV para mantener el estado en snapshots y replays.
pub fn set_workspace_mode<G: Graph + ?Sized>(
    graph: &mut G,
    ws: &WorkspaceVocab,
    mode: WorkspaceMode,
) {
    let id = graph
        .query(&[ws.mode])
        .into_iter()
        .next()
        .unwrap_or_else(|| graph.create());
    graph.set(id, ws.mode, Value::Text(mode.as_label().to_string()));
}

/// Lee el `WorkspaceMode` persistido en el grafo EAV, si existe.
pub fn graph_workspace_mode<G: GraphView + ?Sized>(
    graph: &G,
    ws: &WorkspaceVocab,
) -> Option<WorkspaceMode> {
    let id = graph.query(&[ws.mode]).into_iter().next()?;
    prop_text(graph, id, ws.mode).and_then(|s| WorkspaceMode::parse(&s))
}

/// Persiste el plan activo (`ws.plan.text`) en el grafo EAV.
pub fn set_workspace_plan<G: Graph + ?Sized>(graph: &mut G, ws: &WorkspaceVocab, plan_text: &str) {
    let id = graph
        .query(&[ws.plan_text])
        .into_iter()
        .next()
        .unwrap_or_else(|| graph.create());
    graph.set(id, ws.plan_text, Value::Text(plan_text.to_string()));
}

/// Lee el plan activo (`ws.plan.text`) persistido en el grafo EAV, si existe y no está vacío.
pub fn graph_workspace_plan<G: GraphView + ?Sized>(
    graph: &G,
    ws: &WorkspaceVocab,
) -> Option<String> {
    let id = graph.query(&[ws.plan_text]).into_iter().next()?;
    prop_text(graph, id, ws.plan_text).filter(|s| !s.trim().is_empty())
}

/// Marca en el grafo la intención determinista de deshacer el último efecto reversible,
/// invalidando `file_dirty` sin ejecutar I/O en disco/git (el driver vivo lo materializa).
pub fn mark_latest_effect_undone<G: Graph + ?Sized>(
    graph: &mut G,
    ws: &WorkspaceVocab,
) -> Option<RecordedEffect> {
    let eff = latest_undoable_effect(graph, ws)?;
    graph.set(eff.id, ws.effect_undone, Value::Bool(true));
    let file_id = ensure_file_node(graph, ws, &eff.target);
    graph.set(file_id, ws.file_dirty, Value::Bool(true));
    Some(eff)
}

/// Devuelve todos los efectos marcados como deshechos (`ws.effect.undone == true`) que poseen `tree_sha`,
/// ordenados cronológicamente por `(tick, id)`.
pub fn undone_effects<G: GraphView + ?Sized>(
    graph: &G,
    ws: &WorkspaceVocab,
) -> Vec<RecordedEffect> {
    let mut out = Vec::new();
    for id in graph.query(&[ws.effect_tick, ws.effect_undone]) {
        if !matches!(graph.get(id, ws.effect_undone), Some(Value::Bool(true))) {
            continue;
        }
        let Some(eff) = read_recorded_effect(graph, ws, id) else {
            continue;
        };
        if eff.tree_sha.is_some() {
            out.push(eff);
        }
    }
    out.sort_by_key(|e| (e.tick, e.id.0));
    out
}

/// Sistema determinista que observa resultados de herramientas en el grafo y
/// mantiene al día los nodos `File` (leído/dirty) y el `SideEffectLedger` (`ExternalEffect`).
pub struct WorkspaceLedgerSystem {
    agent_vocab: Option<Vocab>,
    ws_vocab: Option<WorkspaceVocab>,
    recorded_mem_seqs: BTreeSet<(u64, i64)>,
    checkpoints: Option<PendingCheckpoints>,
}

impl Default for WorkspaceLedgerSystem {
    fn default() -> Self {
        Self::new()
    }
}

impl WorkspaceLedgerSystem {
    pub fn new() -> Self {
        Self {
            agent_vocab: None,
            ws_vocab: None,
            recorded_mem_seqs: BTreeSet::new(),
            checkpoints: None,
        }
    }

    pub fn with_checkpoints(mut self, checkpoints: PendingCheckpoints) -> Self {
        self.checkpoints = Some(checkpoints);
        self
    }
}

impl OnyxSystem for WorkspaceLedgerSystem {
    fn name(&self) -> &str {
        "onyx_workspace_ledger"
    }

    fn init(&mut self, attrs: &mut dyn AttrRegistry) -> Result<(), OnyxError> {
        self.agent_vocab = Some(Vocab::resolve(attrs));
        self.ws_vocab = Some(WorkspaceVocab::resolve(attrs));
        Ok(())
    }

    fn on_restore(&mut self, graph: &dyn GraphView) {
        self.recorded_mem_seqs.clear();
        let Some(av) = self.agent_vocab else { return };
        for agent_id in graph.query(&[av.state]) {
            for mem_id in graph.related(agent_id, av.rel_memory) {
                if prop_text(graph, mem_id, av.mem_kind).as_deref() == Some("observe") {
                    if let Some(Value::Int(seq)) = graph.get(mem_id, av.mem_seq) {
                        self.recorded_mem_seqs.insert((agent_id.0, seq));
                    }
                }
            }
        }
    }

    fn run(&mut self, ctx: &mut Ctx) {
        let (Some(av), Some(wv)) = (self.agent_vocab, self.ws_vocab) else {
            return;
        };
        let fallback_tick = i64::try_from(current_tick(ctx.inbox)).unwrap_or(i64::MAX);

        let agents = ctx.graph.query(&[av.state]);
        for agent_id in agents {
            let mems = ctx.graph.related(agent_id, av.rel_memory);
            let mut last_call: Option<(String, String)> = None;

            for mem_id in mems {
                let kind = prop_text(ctx.graph, mem_id, av.mem_kind).unwrap_or_default();
                let text = prop_text(ctx.graph, mem_id, av.mem_text).unwrap_or_default();
                let seq = match ctx.graph.get(mem_id, av.mem_seq) {
                    Some(Value::Int(s)) => s,
                    _ => continue,
                };
                let mem_tick = match ctx.graph.get(mem_id, av.mem_tick) {
                    Some(Value::Int(t)) => t,
                    _ => fallback_tick,
                };

                match kind.as_str() {
                    "call" => {
                        let (tool, args) = match text.split_once(' ') {
                            Some((t, a)) => (t.to_string(), a.to_string()),
                            None => (text, "{}".to_string()),
                        };
                        last_call = Some((tool, args));
                    }
                    "observe" => {
                        let Some((tool, args)) = last_call.take() else {
                            continue;
                        };
                        if !self.recorded_mem_seqs.insert((agent_id.0, seq)) {
                            continue;
                        }
                        record_tool_observation(
                            ctx.graph,
                            &wv,
                            ToolObservation {
                                agent_id: agent_id.0,
                                tick: mem_tick,
                                tool: &tool,
                                args_json: &args,
                                obs_text: &text,
                            },
                            self.checkpoints.as_ref(),
                        );
                    }
                    "tool_error" | "denied" => {
                        last_call = None;
                    }
                    _ => {}
                }
            }
        }
    }
}

fn current_tick(inbox: &[OnyxEvent]) -> u64 {
    inbox
        .iter()
        .find_map(|e| match e {
            OnyxEvent::System(SystemEvent::Tick { n }) => Some(*n),
            _ => None,
        })
        .unwrap_or(0)
}

struct ToolObservation<'a> {
    agent_id: u64,
    tick: i64,
    tool: &'a str,
    args_json: &'a str,
    obs_text: &'a str,
}

fn record_tool_observation(
    graph: &mut dyn Graph,
    wv: &WorkspaceVocab,
    obs: ToolObservation<'_>,
    checkpoints: Option<&PendingCheckpoints>,
) {
    let parsed: JsonValue = serde_json::from_str(obs.args_json).unwrap_or(JsonValue::Null);
    match obs.tool {
        "fs_read" => {
            if let Some(raw_path) = parsed.get("path").and_then(|v| v.as_str()) {
                let file_id = ensure_file_node(graph, wv, raw_path);
                graph.set(file_id, wv.file_read_tick, Value::Int(obs.tick));
            }
        }
        "fs_write" | "fs_edit" => {
            let raw_path = parsed
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let file_id = ensure_file_node(graph, wv, raw_path);
            graph.set(file_id, wv.file_dirty, Value::Bool(true));
            graph.set(file_id, wv.file_read_tick, Value::Int(obs.tick));

            let eff = graph.create();
            graph.set(eff, wv.effect_tick, Value::Int(obs.tick));
            graph.set(
                eff,
                wv.effect_agent,
                Value::Int(i64::try_from(obs.agent_id).unwrap_or(0)),
            );
            graph.set(eff, wv.effect_tool, Value::Text(obs.tool.to_string()));
            let target = normalize_rel_path(raw_path);
            graph.set(eff, wv.effect_target, Value::Text(target.clone()));
            graph.set(
                eff,
                wv.effect_reversibility,
                Value::Text(EffectReversibility::AutoReversible.as_str().to_string()),
            );
            graph.set(
                eff,
                wv.effect_summary,
                Value::Text(format!("{} on {target}", obs.tool)),
            );
            let cp_from_map = checkpoints.and_then(|cp_map| {
                cp_map.lock().ok().and_then(|mut guard| {
                    guard.remove(&(obs.agent_id, obs.tool.to_string(), target))
                })
            });
            if let Some((head_sha, tree_sha)) =
                parse_inline_checkpoint(obs.obs_text).or(cp_from_map)
            {
                if !head_sha.is_empty() {
                    graph.set(eff, wv.effect_head_sha, Value::Text(head_sha));
                }
                if !tree_sha.is_empty() {
                    graph.set(eff, wv.effect_tree_sha, Value::Text(tree_sha));
                }
            }
        }
        "plan_update" => {
            let plan = crate::tools::format_plan_steps(obs.args_json)
                .unwrap_or_else(|_| obs.obs_text.to_string());
            set_workspace_plan(graph, wv, &plan);
        }
        "shell_exec" => {
            let cmd = parsed
                .get("command")
                .and_then(|v| v.as_str())
                .unwrap_or("<command>");
            let rev = classify_shell_reversibility(cmd);
            let eff = graph.create();
            graph.set(eff, wv.effect_tick, Value::Int(obs.tick));
            graph.set(
                eff,
                wv.effect_agent,
                Value::Int(i64::try_from(obs.agent_id).unwrap_or(0)),
            );
            graph.set(eff, wv.effect_tool, Value::Text("shell_exec".to_string()));
            graph.set(eff, wv.effect_target, Value::Text(cmd.to_string()));
            graph.set(
                eff,
                wv.effect_reversibility,
                Value::Text(rev.as_str().to_string()),
            );
            graph.set(
                eff,
                wv.effect_summary,
                Value::Text(format!("shell_exec: {cmd}")),
            );
        }
        _ => {}
    }
}

fn classify_shell_reversibility(cmd: &str) -> EffectReversibility {
    let trimmed = cmd.trim();
    if trimmed.starts_with("git push") || trimmed.starts_with("git merge") {
        EffectReversibility::Compensable
    } else if trimmed.starts_with("git commit") || trimmed.starts_with("git add") {
        EffectReversibility::AutoReversible
    } else {
        EffectReversibility::Irreversible
    }
}

fn ensure_file_node<G: Graph + ?Sized>(
    graph: &mut G,
    wv: &WorkspaceVocab,
    raw_path: &str,
) -> ConceptId {
    let norm = normalize_rel_path(raw_path);
    for id in graph.query(&[wv.file_path]) {
        if let Some(Value::Text(existing)) = graph.get(id, wv.file_path) {
            if existing == norm {
                return id;
            }
        }
    }
    let id = graph.create();
    graph.set(id, wv.file_path, Value::Text(norm));
    graph.set(id, wv.file_dirty, Value::Bool(false));
    id
}

pub(crate) fn normalize_rel_path(raw: &str) -> String {
    raw.trim()
        .trim_start_matches("./")
        .trim_start_matches(".\\")
        .replace('\\', "/")
}

fn prop_text<G: GraphView + ?Sized>(g: &G, id: ConceptId, attr: Attr) -> Option<String> {
    match g.get(id, attr) {
        Some(Value::Text(s)) => Some(s),
        _ => None,
    }
}
