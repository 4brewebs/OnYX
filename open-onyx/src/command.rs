//! Sistema determinista que procesa `IngressEvent::Command` en sesiones interactivas:
//! `/new`, `/mode`, `/undo` (como intención pura sobre el grafo) y continuación multi-turno.

use onyx_soul_agents::{
    AgentSpec, AttrRegistry, ConceptId, Ctx, GraphView, IngressEvent, OnyxError, OnyxEvent,
    OnyxSystem, SystemEvent, Vocab, append_agent_memory, continue_agent, spawn_event,
};
use onyx_workspace::{
    WorkspaceMode, WorkspaceModeHandle, WorkspaceTools, WorkspaceVocab, graph_workspace_mode,
    mark_latest_effect_undone, set_workspace_mode,
};

pub struct CommandSpawnerSystem {
    vocab: Option<Vocab>,
    ws_vocab: Option<WorkspaceVocab>,
    mode_handle: WorkspaceModeHandle,
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

        for event in ctx.inbox {
            let OnyxEvent::Ingress(IngressEvent::Command(raw_cmd)) = event else {
                continue;
            };
            let cmd = raw_cmd.trim();
            if cmd.is_empty() {
                continue;
            }

            if cmd == "/new" {
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
                continue;
            }

            if let Some(goal) = cmd
                .strip_prefix("/new ")
                .map(str::trim)
                .filter(|g| !g.is_empty())
            {
                self.force_new_agent = false;
                let spec = self.spawn_spec(&*ctx.graph, &v, goal.to_string());
                ctx.outbox.push(spawn_event(&v, &spec, None));
                continue;
            }

            if cmd == "/mode" || cmd.starts_with("/mode ") {
                let arg = cmd.strip_prefix("/mode").unwrap_or("").trim();
                let next_mode = if arg.is_empty() {
                    match self.mode_handle.get() {
                        WorkspaceMode::Build => WorkspaceMode::Plan,
                        WorkspaceMode::Plan | WorkspaceMode::Explore => WorkspaceMode::Build,
                    }
                } else if let Some(parsed) = WorkspaceMode::parse(arg) {
                    parsed
                } else {
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
                continue;
            }

            if cmd == "/undo" {
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
                continue;
            }

            if !self.force_new_agent {
                if let Some(root_id) = find_latest_root_agent(&*ctx.graph, &v) {
                    if continue_agent(ctx.graph, &v, root_id, cmd, self.max_steps_per_turn, now) {
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
