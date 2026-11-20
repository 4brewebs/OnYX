//! Sistema determinista que procesa `IngressEvent::Command` en sesiones interactivas:
//! `/new`, `/mode`, `/undo`, `/compact`, `/services`, `/agent`, `/rewind`, `/fork` y continuación multi-turno.

use std::sync::{Arc, Mutex};

use onyx_soul_agents::{
    AgentSpec, AttrRegistry, ConceptId, Ctx, GraphView, IngressEvent, OnyxError, OnyxEvent,
    OnyxSystem, SystemEvent, Value, Vocab, append_agent_memory, continue_agent, spawn_event,
};
use onyx_workspace::{
    SproutSupervisor, WorkspaceMode, WorkspaceModeHandle, WorkspaceTools, WorkspaceVocab,
    format_services_panel_from_toon, graph_workspace_mode, graph_workspace_plan,
    graph_workspace_services, mark_latest_effect_undone, set_workspace_mode,
    set_workspace_services,
};

use crate::context::{COMPACT_MARKER, build_compaction_summary};
use crate::pack::SharedPackStore;

/// Acción de sesión solicitada por un comando slash que requiere operar sobre `AgentHost`,
/// `PackSnapshotStore` o el árbol Git fuera de un tick individual (`REQ-03.2`, `REQ-03.3`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionAction {
    Rewind { requested_tick: u64 },
    Fork { label: Option<String> },
}

#[derive(Clone, Default)]
pub struct PendingSessionAction(Arc<Mutex<Option<SessionAction>>>);

impl PendingSessionAction {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&self, action: SessionAction) {
        if let Ok(mut guard) = self.0.lock() {
            *guard = Some(action);
        }
    }

    pub fn take(&self) -> Option<SessionAction> {
        self.0.lock().ok().and_then(|mut g| g.take())
    }
}

/// Cola thread-safe de notas de sistema que `CommandSpawnerSystem` proyecta en la memoria del
/// agente raíz en el siguiente tick (manteniendo todas las mutaciones del grafo dentro de `OnyxSystem`).
#[derive(Clone, Default)]
pub struct PendingSystemNotes(Arc<Mutex<Vec<String>>>);

impl PendingSystemNotes {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&self, note: impl Into<String>) {
        if let Ok(mut guard) = self.0.lock() {
            guard.push(note.into());
        }
    }

    pub fn drain(&self) -> Vec<String> {
        self.0
            .lock()
            .map(|mut g| std::mem::take(&mut *g))
            .unwrap_or_default()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashCommand {
    New(Option<String>),
    Mode(Option<String>),
    Undo,
    Compact(Option<String>),
    Services,
    Service(String),
    Agent(Option<String>),
    Rewind(Option<u64>),
    RewindInvalid(String),
    Fork(Option<String>),
}

pub fn parse_slash_command(raw: &str) -> Option<SlashCommand> {
    let cmd = raw.trim();
    if cmd == "/new" {
        return Some(SlashCommand::New(None));
    }
    if let Some(goal) = cmd.strip_prefix("/new ") {
        let trimmed = goal.trim();
        return Some(SlashCommand::New(if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }));
    }
    if cmd == "/mode" {
        return Some(SlashCommand::Mode(None));
    }
    if let Some(arg) = cmd.strip_prefix("/mode ") {
        let trimmed = arg.trim();
        return Some(SlashCommand::Mode(if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }));
    }
    if cmd == "/undo" {
        return Some(SlashCommand::Undo);
    }
    if cmd == "/compact" {
        return Some(SlashCommand::Compact(None));
    }
    if let Some(focus) = cmd.strip_prefix("/compact ") {
        let trimmed = focus.trim();
        return Some(SlashCommand::Compact(if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }));
    }
    if cmd == "/services" || cmd == "/service" {
        return Some(SlashCommand::Services);
    }
    if let Some(rest) = cmd
        .strip_prefix("/service ")
        .or_else(|| cmd.strip_prefix("/services "))
    {
        let trimmed = rest.trim();
        if !trimmed.is_empty() {
            return Some(SlashCommand::Service(trimmed.to_string()));
        }
        return Some(SlashCommand::Services);
    }
    if cmd == "/agent" || cmd == "/agents" {
        return Some(SlashCommand::Agent(None));
    }
    if let Some(rest) = cmd
        .strip_prefix("/agent ")
        .or_else(|| cmd.strip_prefix("/agents "))
    {
        let trimmed = rest.trim();
        return Some(SlashCommand::Agent(if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }));
    }
    if cmd == "/rewind" {
        return Some(SlashCommand::Rewind(None));
    }
    if let Some(rest) = cmd.strip_prefix("/rewind ") {
        let trimmed = rest.trim();
        if trimmed.is_empty() {
            return Some(SlashCommand::Rewind(None));
        }
        return match trimmed.parse::<u64>() {
            Ok(tick) => Some(SlashCommand::Rewind(Some(tick))),
            Err(_) => Some(SlashCommand::RewindInvalid(trimmed.to_string())),
        };
    }
    if cmd == "/fork" {
        return Some(SlashCommand::Fork(None));
    }
    if let Some(rest) = cmd.strip_prefix("/fork ") {
        let trimmed = rest.trim();
        return Some(SlashCommand::Fork(if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }));
    }
    None
}

fn read_agent_memory_ordered<G: GraphView + ?Sized>(
    graph: &G,
    v: &Vocab,
    agent: ConceptId,
) -> (String, Vec<(String, String)>) {
    let goal = match graph.get(agent, v.goal) {
        Some(Value::Text(s)) => s,
        _ => String::new(),
    };
    let mut rows: Vec<(i64, String, String)> = graph
        .related(agent, v.rel_memory)
        .into_iter()
        .filter_map(|mem_id| {
            let seq = match graph.get(mem_id, v.mem_seq) {
                Some(Value::Int(n)) => n,
                _ => 0,
            };
            let kind = match graph.get(mem_id, v.mem_kind)? {
                Value::Text(k) => k,
                _ => return None,
            };
            let text = match graph.get(mem_id, v.mem_text) {
                Some(Value::Text(t)) => t,
                _ => String::new(),
            };
            Some((seq, kind, text))
        })
        .collect();
    rows.sort_by_key(|(seq, _, _)| *seq);
    let mut memory: Vec<(String, String)> = rows.into_iter().map(|(_, k, t)| (k, t)).collect();
    if let Some(Value::Text(res)) = graph.get(agent, v.result) {
        if !res.is_empty() {
            memory.push(("say".to_string(), res));
        }
    }
    (goal, memory)
}

pub struct CommandSpawnerSystem {
    vocab: Option<Vocab>,
    ws_vocab: Option<WorkspaceVocab>,
    mode_handle: WorkspaceModeHandle,
    sprout: Option<SproutSupervisor>,
    snapshot_store: Option<SharedPackStore>,
    pending_action: PendingSessionAction,
    pending_notes: PendingSystemNotes,
    auto_approve: bool,
    max_steps_per_turn: i64,
    force_new_agent: bool,
    counter: usize,
}

impl CommandSpawnerSystem {
    pub fn new(auto_approve: bool) -> Self {
        Self {
            vocab: None,
            ws_vocab: None,
            mode_handle: WorkspaceModeHandle::default(),
            sprout: None,
            snapshot_store: None,
            pending_action: PendingSessionAction::new(),
            pending_notes: PendingSystemNotes::new(),
            auto_approve,
            max_steps_per_turn: 128,
            force_new_agent: false,
            counter: 0,
        }
    }

    pub fn with_max_steps(mut self, max_steps_per_turn: i64) -> Self {
        self.max_steps_per_turn = max_steps_per_turn.max(1);
        self
    }

    pub fn with_mode_handle(mut self, mode_handle: WorkspaceModeHandle) -> Self {
        self.mode_handle = mode_handle;
        self
    }

    pub fn with_sprout(mut self, sprout: SproutSupervisor) -> Self {
        self.sprout = Some(sprout);
        self
    }

    pub fn with_snapshot_store(mut self, store: SharedPackStore) -> Self {
        self.snapshot_store = Some(store);
        self
    }

    pub fn with_pending_action(mut self, pending_action: PendingSessionAction) -> Self {
        self.pending_action = pending_action;
        self
    }

    pub fn with_pending_notes(mut self, pending_notes: PendingSystemNotes) -> Self {
        self.pending_notes = pending_notes;
        self
    }

    fn spawn_spec<G: GraphView + ?Sized>(
        &mut self,
        graph: &G,
        v: &Vocab,
        goal: String,
    ) -> AgentSpec {
        let existing = graph.query(&[v.state]).len();
        self.counter = self.counter.max(existing) + 1;
        let mut spec = AgentSpec::new(format!("openonyx-{}", self.counter), goal).with_tools(
            WorkspaceTools::all_tool_names_with_subagents()
                .iter()
                .copied(),
        );
        if !self.auto_approve {
            spec =
                spec.with_approval_tools(WorkspaceTools::approval_required_names().iter().copied());
        }
        spec
    }
}

pub fn find_latest_root_agent<G: GraphView + ?Sized>(graph: &G, v: &Vocab) -> Option<ConceptId> {
    graph
        .query(&[v.state])
        .into_iter()
        .rfind(|id| graph.related(*id, v.rel_parent).is_empty())
}

impl OnyxSystem for CommandSpawnerSystem {
    fn name(&self) -> &str {
        "openonyx_command_spawner"
    }

    fn init(&mut self, attrs: &mut dyn AttrRegistry) -> Result<(), OnyxError> {
        self.vocab = Some(Vocab::resolve(attrs));
        self.ws_vocab = Some(WorkspaceVocab::resolve(attrs));
        Ok(())
    }

    fn on_restore(&mut self, graph: &dyn GraphView) {
        if let Some(wv) = self.ws_vocab {
            if let Some(mode) = graph_workspace_mode(graph, &wv) {
                self.mode_handle.set(mode);
            }
        }
    }

    fn run(&mut self, ctx: &mut Ctx) {
        let Some(v) = self.vocab else { return };
        let now = ctx
            .inbox
            .iter()
            .find_map(|event| match event {
                OnyxEvent::System(SystemEvent::Tick { n }) => Some(*n),
                _ => None,
            })
            .unwrap_or(0);

        let notes = self.pending_notes.drain();
        if !notes.is_empty() {
            if let Some(root_id) = find_latest_root_agent(&*ctx.graph, &v) {
                for note in notes {
                    append_agent_memory(ctx.graph, &v, root_id, "system", &note, now);
                }
            }
        }

        for event in ctx.inbox {
            let OnyxEvent::Ingress(IngressEvent::Command(raw_cmd)) = event else {
                continue;
            };
            let cmd = raw_cmd.trim();
            if cmd.is_empty() {
                continue;
            }

            if let Some(slash) = parse_slash_command(cmd) {
                match slash {
                    SlashCommand::New(None) => {
                        self.force_new_agent = true;
                        if let Some(root_id) = find_latest_root_agent(&*ctx.graph, &v) {
                            append_agent_memory(
                                ctx.graph,
                                &v,
                                root_id,
                                "system",
                                "[/new] Next prompt will start a fresh agent.",
                                now,
                            );
                        }
                    }
                    SlashCommand::New(Some(goal)) => {
                        self.force_new_agent = false;
                        let spec = self.spawn_spec(&*ctx.graph, &v, goal);
                        ctx.outbox.push(spawn_event(&v, &spec, None));
                    }
                    SlashCommand::Mode(arg_opt) => {
                        let next_mode = match arg_opt.as_deref() {
                            None => match self.mode_handle.get() {
                                WorkspaceMode::Build => WorkspaceMode::Plan,
                                WorkspaceMode::Plan | WorkspaceMode::Explore => {
                                    WorkspaceMode::Build
                                }
                            },
                            Some(arg) => match WorkspaceMode::parse(arg) {
                                Some(parsed) => parsed,
                                None => {
                                    if let Some(root_id) = find_latest_root_agent(&*ctx.graph, &v) {
                                        append_agent_memory(
                                            ctx.graph,
                                            &v,
                                            root_id,
                                            "system",
                                            &format!(
                                                "[/mode] Unknown workspace mode `{arg}` (use `plan` or `build`)."
                                            ),
                                            now,
                                        );
                                    }
                                    continue;
                                }
                            },
                        };

                        self.mode_handle.set(next_mode);
                        if let Some(wv) = self.ws_vocab {
                            set_workspace_mode(ctx.graph, &wv, next_mode);
                        }
                        if let Some(root_id) = find_latest_root_agent(&*ctx.graph, &v) {
                            append_agent_memory(
                                ctx.graph,
                                &v,
                                root_id,
                                "system",
                                &format!(
                                    "[/mode] Switched workspace mode to {}",
                                    next_mode.as_label()
                                ),
                                now,
                            );
                        }
                    }
                    SlashCommand::Undo => {
                        let Some(wv) = self.ws_vocab else { continue };
                        let note = match mark_latest_effect_undone(ctx.graph, &wv) {
                            Some(eff) => {
                                let tree_sha = eff.tree_sha.unwrap_or_default();
                                format!(
                                    "[/undo] Restored git checkpoint {tree_sha} (reverted {} on `{}` from tick {})",
                                    eff.tool, eff.target, eff.tick
                                )
                            }
                            None => "[/undo] No reversible git checkpoint available.".to_string(),
                        };
                        if let Some(root_id) = find_latest_root_agent(&*ctx.graph, &v) {
                            append_agent_memory(ctx.graph, &v, root_id, "system", &note, now);
                        }
                    }
                    SlashCommand::Compact(focus) => {
                        if let Some(root_id) = find_latest_root_agent(&*ctx.graph, &v) {
                            let (goal, memory) =
                                read_agent_memory_ordered(&*ctx.graph, &v, root_id);
                            let active_plan = self
                                .ws_vocab
                                .as_ref()
                                .and_then(|wv| graph_workspace_plan(&*ctx.graph, wv));
                            let summary = build_compaction_summary(
                                &goal,
                                &memory,
                                active_plan.as_deref(),
                                focus.as_deref(),
                            );
                            let entry = format!("{COMPACT_MARKER}{summary}");
                            append_agent_memory(ctx.graph, &v, root_id, "system", &entry, now);
                        }
                    }
                    SlashCommand::Services => {
                        let note = if let Some(sprout) = &self.sprout {
                            match sprout.status_toon() {
                                Ok(toon) => {
                                    if let Some(wv) = self.ws_vocab {
                                        let panel = format_services_panel_from_toon(&toon);
                                        set_workspace_services(ctx.graph, &wv, &panel);
                                    }
                                    format!("[/services]\n{toon}")
                                }
                                Err(err) => {
                                    if let Some(wv) = self.ws_vocab {
                                        if ctx.graph.query(&[wv.services_text]).is_empty() {
                                            set_workspace_services(ctx.graph, &wv, "");
                                        }
                                    }
                                    format!("[/services] {err}")
                                }
                            }
                        } else {
                            // Keep EAV concept allocation 100% identical to live `chat` during offline replay:
                            // if `/services` runs before any agent spawns, reserve the `ws.services.text` singleton node.
                            if let Some(wv) = self.ws_vocab {
                                if ctx.graph.query(&[wv.services_text]).is_empty() {
                                    set_workspace_services(ctx.graph, &wv, "");
                                }
                            }
                            if let Some(existing) = self
                                .ws_vocab
                                .as_ref()
                                .and_then(|wv| graph_workspace_services(&*ctx.graph, wv))
                            {
                                format!("[/services]\n{existing}")
                            } else {
                                "[/services] No sprout.toml services loaded.".to_string()
                            }
                        };
                        if let Some(root_id) = find_latest_root_agent(&*ctx.graph, &v) {
                            append_agent_memory(ctx.graph, &v, root_id, "system", &note, now);
                        }
                    }
                    SlashCommand::Service(arg) => {
                        let parts: Vec<&str> = arg.split_whitespace().collect();
                        let mutates_services =
                            matches!(parts.as_slice(), ["start", _] | ["stop", _]);
                        let note = if let Some(sprout) = &self.sprout {
                            match parts.as_slice() {
                                ["start", target] => {
                                    if !self.mode_handle.get().allows_mutation() {
                                        format!(
                                            "[/service] Blocked in `{}` mode: switch to BUILD to start `{target}`.",
                                            self.mode_handle.get().as_label()
                                        )
                                    } else {
                                        let res = if target.eq_ignore_ascii_case("all") {
                                            sprout.start_all(None)
                                        } else {
                                            sprout.start(target, None)
                                        };
                                        match res {
                                            Ok(out) => {
                                                if let Some(wv) = self.ws_vocab {
                                                    let panel =
                                                        format_services_panel_from_toon(&out);
                                                    set_workspace_services(ctx.graph, &wv, &panel);
                                                }
                                                format!("[/service start {target}]\n{out}")
                                            }
                                            Err(err) => {
                                                if let Some(wv) = self.ws_vocab {
                                                    if ctx.graph.query(&[wv.services_text]).is_empty()
                                                    {
                                                        set_workspace_services(ctx.graph, &wv, "");
                                                    }
                                                }
                                                format!("[/service start {target}] ERROR: {err}")
                                            }
                                        }
                                    }
                                }
                                ["stop", target] => {
                                    if !self.mode_handle.get().allows_mutation() {
                                        format!(
                                            "[/service] Blocked in `{}` mode: switch to BUILD to stop `{target}`.",
                                            self.mode_handle.get().as_label()
                                        )
                                    } else {
                                        let is_all = target.eq_ignore_ascii_case("all");
                                        let res = if is_all {
                                            sprout.stop(None, true)
                                        } else {
                                            sprout.stop(Some(target), false)
                                        };
                                        match res {
                                            Ok(out) => {
                                                if let Some(wv) = self.ws_vocab {
                                                    let panel =
                                                        format_services_panel_from_toon(&out);
                                                    set_workspace_services(ctx.graph, &wv, &panel);
                                                }
                                                format!("[/service stop {target}]\n{out}")
                                            }
                                            Err(err) => {
                                                if let Some(wv) = self.ws_vocab {
                                                    if ctx.graph.query(&[wv.services_text]).is_empty()
                                                    {
                                                        set_workspace_services(ctx.graph, &wv, "");
                                                    }
                                                }
                                                format!("[/service stop {target}] ERROR: {err}")
                                            }
                                        }
                                    }
                                }
                                ["logs", id] => match sprout.logs(id, Some(40), None, false) {
                                    Ok(out) => format!("[/service logs {id}]\n{out}"),
                                    Err(err) => format!("[/service logs {id}] ERROR: {err}"),
                                },
                                ["logs", id, tail_str] => {
                                    let tail = tail_str.parse::<usize>().ok();
                                    match sprout.logs(id, tail, None, false) {
                                        Ok(out) => format!("[/service logs {id}]\n{out}"),
                                        Err(err) => format!("[/service logs {id}] ERROR: {err}"),
                                    }
                                }
                                _ => "[/service] Usage: /service start <id|all> | /service stop <id|all> | /service logs <id> [tail]".to_string(),
                            }
                        } else {
                            if mutates_services && self.mode_handle.get().allows_mutation() {
                                if let Some(wv) = self.ws_vocab {
                                    if ctx.graph.query(&[wv.services_text]).is_empty() {
                                        set_workspace_services(ctx.graph, &wv, "");
                                    }
                                }
                            }
                            format!("[/service] {arg} (offline replay)")
                        };
                        if let Some(root_id) = find_latest_root_agent(&*ctx.graph, &v) {
                            append_agent_memory(ctx.graph, &v, root_id, "system", &note, now);
                        }
                    }
                    SlashCommand::Agent(target) => {
                        let agents = ctx.graph.query(&[v.state]);
                        let roster: Vec<String> = agents
                            .iter()
                            .map(|id| {
                                let name = match ctx.graph.get(*id, v.name) {
                                    Some(Value::Text(s)) => s,
                                    _ => String::new(),
                                };
                                let state = match ctx.graph.get(*id, v.state) {
                                    Some(Value::Text(s)) => s,
                                    _ => String::new(),
                                };
                                let role = if ctx.graph.related(*id, v.rel_parent).is_empty() {
                                    "root"
                                } else {
                                    "sub"
                                };
                                format!("{}:{} [{}] ({})", id.0, name, role, state)
                            })
                            .collect();
                        let note = if roster.is_empty() {
                            "[/agent] No active agents yet.".to_string()
                        } else if let Some(t) = target {
                            format!(
                                "[/agent] Focus `{t}` is handled by the TUI (F2 / /agent). Agents: {}",
                                roster.join(" | ")
                            )
                        } else {
                            format!("[/agent] Agents: {}", roster.join(" | "))
                        };
                        if let Some(root_id) = find_latest_root_agent(&*ctx.graph, &v) {
                            append_agent_memory(ctx.graph, &v, root_id, "system", &note, now);
                        }
                    }
                    SlashCommand::Rewind(None) => {
                        let note = if let Some(store) = &self.snapshot_store {
                            let entries = store.entries();
                            if entries.is_empty() {
                                format!(
                                    "[/rewind] No milestones recorded yet in `{}`.",
                                    store.current_dir().display()
                                )
                            } else {
                                let mut lines = vec![format!(
                                    "[/rewind] Milestones in `{}` (use `/rewind <tick>` to jump):",
                                    store.current_dir().display()
                                )];
                                for meta in entries {
                                    lines.push(format!(
                                        "  - tick={} [{}] ({} B)",
                                        meta.tick, meta.tag, meta.stored_len
                                    ));
                                }
                                lines.join("\n")
                            }
                        } else {
                            "[/rewind] Snapshot persistence is disabled; start with `--snapshot-dir <dir>` to enable interactive time-travel.".to_string()
                        };
                        if let Some(root_id) = find_latest_root_agent(&*ctx.graph, &v) {
                            append_agent_memory(ctx.graph, &v, root_id, "system", &note, now);
                        }
                    }
                    SlashCommand::RewindInvalid(bad) => {
                        if let Some(root_id) = find_latest_root_agent(&*ctx.graph, &v) {
                            append_agent_memory(
                                ctx.graph,
                                &v,
                                root_id,
                                "system",
                                &format!(
                                    "[/rewind] Invalid tick `{bad}` (expected a non-negative integer, e.g. `/rewind 12`)."
                                ),
                                now,
                            );
                        }
                    }
                    SlashCommand::Rewind(Some(requested_tick)) => {
                        let any_live = ctx.graph.query(&[v.state]).into_iter().any(|id| {
                            matches!(
                                ctx.graph.get(id, v.state),
                                Some(Value::Text(ref s))
                                    if matches!(
                                        s.as_str(),
                                        "thinking"
                                            | "acting"
                                            | "waiting_children"
                                            | "waiting_approval"
                                    )
                            )
                        });
                        if self.snapshot_store.is_none() {
                            if let Some(root_id) = find_latest_root_agent(&*ctx.graph, &v) {
                                append_agent_memory(
                                    ctx.graph,
                                    &v,
                                    root_id,
                                    "system",
                                    "[/rewind] Snapshot persistence is disabled; start with `--snapshot-dir <dir>` to enable interactive rewind.",
                                    now,
                                );
                            }
                        } else if any_live {
                            if let Some(root_id) = find_latest_root_agent(&*ctx.graph, &v) {
                                append_agent_memory(
                                    ctx.graph,
                                    &v,
                                    root_id,
                                    "system",
                                    "[/rewind] Cannot rewind while an agent is active or awaiting approval; wait until settled.",
                                    now,
                                );
                            }
                        } else {
                            self.pending_action
                                .set(SessionAction::Rewind { requested_tick });
                        }
                    }
                    SlashCommand::Fork(label) => {
                        if self.snapshot_store.is_none() {
                            if let Some(root_id) = find_latest_root_agent(&*ctx.graph, &v) {
                                append_agent_memory(
                                    ctx.graph,
                                    &v,
                                    root_id,
                                    "system",
                                    "[/fork] Snapshot persistence is disabled; start with `--snapshot-dir <dir>` to fork timelines.",
                                    now,
                                );
                            }
                        } else {
                            self.pending_action.set(SessionAction::Fork { label });
                        }
                    }
                }
                continue;
            }

            if !self.force_new_agent {
                if let Some(root_id) = find_latest_root_agent(&*ctx.graph, &v) {
                    if continue_agent(ctx.graph, &v, root_id, cmd, self.max_steps_per_turn, now) {
                        continue;
                    }
                    let root_state = match ctx.graph.get(root_id, v.state) {
                        Some(Value::Text(s)) => s,
                        _ => String::new(),
                    };
                    if matches!(
                        root_state.as_str(),
                        "thinking" | "acting" | "waiting_children" | "waiting_approval"
                    ) {
                        append_agent_memory(
                            ctx.graph,
                            &v,
                            root_id,
                            "system",
                            &format!(
                                "[system] Root agent #{root} is currently `{root_state}`; wait for completion, press F2 (or `/agent`) to inspect active agents, or use `/new <goal>` to start a separate root agent.",
                                root = root_id.0
                            ),
                            now,
                        );
                        continue;
                    }
                }
            }

            self.force_new_agent = false;
            let spec = self.spawn_spec(&*ctx.graph, &v, cmd.to_string());
            ctx.outbox.push(spawn_event(&v, &spec, None));
        }
    }
}
