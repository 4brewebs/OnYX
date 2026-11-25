//! Sistema determinista que procesa `IngressEvent::Command` en sesiones interactivas:
//! `/new`, `/mode`, `/undo`, `/compact`, `/services`, `/agent`, `/rewind`, `/fork` y continuación multi-turno.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use onyx_soul_agents::{
    AgentSpec, AgentState, Attr, AttrRegistry, ConceptId, Ctx, GraphView, IngressEvent, OnyxError,
    OnyxEvent, OnyxSystem, SystemEvent, Value, Vocab, append_agent_memory, continue_agent,
    spawn_event,
};
use onyx_workspace::{
    RecordedLlmProfile, SproutSupervisor, WorkspaceMode, WorkspaceModeHandle, WorkspaceTools,
    WorkspaceVocab, evaluate_workspace_trust, format_services_panel_from_toon,
    graph_subagent_records, graph_workspace_llm_profile, graph_workspace_mode,
    graph_workspace_plan, graph_workspace_services, mark_latest_effect_undone,
    revoke_workspace_trust, set_workspace_llm_profile, set_workspace_mode, set_workspace_services,
    trust_workspace, update_subagent_status, upsert_subagent_record,
};

use crate::context::{
    COMPACT_MARKER, ReasoningEffort, SessionTelemetryHandle, build_compaction_summary,
};
use crate::llm_profile::{LlmProfileHandle, LlmProvider, ResolvedLlmTarget, effort_label};
use crate::pack::SharedPackStore;
use crate::subagent::{SubagentHandle, decode_proposal_command};
use crate::time_debug::{
    DET_STEP_LLM_CMD_PREFIX, SealedGraphOp, decode_det_graph_command, decode_det_inject_command,
    decode_det_read_command, decode_det_shell_command,
};

/// Acción de sesión solicitada por un comando slash que requiere operar sobre `AgentHost`,
/// `PackSnapshotStore` o el árbol Git fuera de un tick individual (`REQ-03.2`, `REQ-03.3`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionAction {
    Rewind {
        requested_tick: u64,
    },
    Fork {
        label: Option<String>,
    },
    MergeSubagent {
        job_id: String,
    },
    DiscardSubagent {
        job_id: String,
    },
    /// `/spawn [--driver <id>] <tarea>` (`SDD-10` Fase 4): lanza un job de worktree
    /// en segundo plano sin gastar un turno de `root`.
    SpawnSubagent {
        goal: String,
        driver: Option<String>,
    },
    /// `! <cmd>` (`SDD-07` Fase 2): el spawner solo **solicita** la acción
    /// (no-op en `openonyx replay`, 0 I/O); en vivo `apply_session_action`
    /// ejecuta (gate + bifurcación + sandbox) y reinyecta el resultado sellado.
    DetShell {
        command: String,
    },
    /// `/read <ruta> [a..b]` (`SDD-07` Fase 2): mismo patrón que [`SessionAction::DetShell`].
    DetRead {
        path: String,
        start_line: Option<usize>,
        end_line: Option<usize>,
    },
    /// `/step [n]` (`SDD-07` Fase 2): `n` ticks deterministas con 0 LLM (frozen).
    Step(usize),
    /// `/step-llm` (`SDD-07` Fase 2): gate + bifurcación + `grant_step_llm(1)`
    /// + reactivación `Done`→`Idle` (1 turno exacto, recongelado al responder).
    StepLlm,
    /// `/go` / `/unfreeze` (`SDD-07` Fase 2): bifurca si aplica + `unfreeze`.
    Go,
    /// `/time` (`SDD-07` Fase 2): conmuta `frozen`.
    TimeToggle,
    /// `/inject <texto>` (fix de auditoría Fase 2): memoria `user` sellada
    /// (mismo patrón que [`SessionAction::DetShell`]): gate + bifurcación y
    /// reinyección en-tick del comando sellado. En `openonyx replay` el
    /// slash crudo es no-op (0 I/O).
    Inject {
        text: String,
    },
    /// `/graph drop-mem <seq|a..b>` (fix de auditoría Fase 2): sellado
    /// `drop-mem`/`edit-mem`/`set-goal` reejecutado en-tick por el spawner.
    GraphDropMem {
        start: i64,
        end: i64,
    },
    /// `/graph edit-mem <seq> <texto>` (fix de auditoría Fase 2).
    GraphEditMem {
        seq: i64,
        text: String,
    },
    /// `/graph set-goal <texto>` (fix de auditoría Fase 2).
    GraphSetGoal {
        text: String,
    },
}

/// `SDD-07` Fase 4B (fix de auditoría): comandos de sesión que deben ejecutarse
/// **inmediatamente** aunque el agente raíz esté ocupado (`root_busy == true`).
///
/// Sin esta lista, bajo `TimeFreeze` en el breakpoint principal (pre-LLM:
/// `root_busy` + `frozen`) las vías de escape (`/go`, `/step-llm`,
/// `/graph drop-mem`, `!cmd`…) se encolarían en los prompts pendientes
/// esperando un idle que nunca llega mientras el tiempo sigue congelado →
/// deadlock (el usuario no puede salir del freeze con su propia UI).
///
/// Función única compartida por TUI y Web para que las dos superficies no
/// diverjan: antes eran dos copias idénticas byte a byte
/// (`is_immediate_slash_cmd` en `tui.rs` y `is_immediate_web_slash` en
/// `web.rs`).
pub(crate) fn is_immediate_session_command(cmd: &str) -> bool {
    let lower = cmd.trim().to_ascii_lowercase();
    lower == "/mode"
        || lower.starts_with("/mode ")
        || lower == "/profile"
        || lower.starts_with("/profile ")
        || lower == "/provider"
        || lower.starts_with("/provider ")
        || lower == "/model"
        || lower.starts_with("/model ")
        || lower == "/effort"
        || lower.starts_with("/effort ")
        || lower == "/services"
        || lower.starts_with("/services ")
        || lower == "/service"
        || lower.starts_with("/service ")
        || lower == "/subagents"
        || lower == "/subagent"
        || lower == "/spawn"
        || lower.starts_with("/spawn ")
        || lower.starts_with("/merge ")
        || lower.starts_with("/discard ")
        || lower == "/trust"
        || lower.starts_with("/trust ")
        || lower == "/undo"
        || lower == "/cancel"
        || lower.starts_with("/cancel ")
        || lower == "/stop"
        || lower.starts_with("/stop ")
        // `SDD-07` Fase 4B: vías de escape del TimeFreeze y primitivas
        // 0-tokens (deben llegar al bus incluso con `root_busy` + `frozen`).
        || lower == "/time"
        || lower == "/go"
        || lower == "/unfreeze"
        || lower == "/step"
        || lower.starts_with("/step ")
        || lower == "/step-llm"
        || lower == "/graph"
        || lower.starts_with("/graph ")
        || lower == "/inject"
        || lower.starts_with("/inject ")
        || lower == "/read"
        || lower.starts_with("/read ")
        || lower.starts_with('!')
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

#[derive(Default)]
struct PendingSystemQueue {
    notes: Vec<String>,
    subagent_statuses: Vec<(String, &'static str)>,
}

/// Cola thread-safe de notas de sistema que `CommandSpawnerSystem` proyecta en la memoria del
/// agente raíz en el siguiente tick (manteniendo todas las mutaciones del grafo dentro de `OnyxSystem`).
#[derive(Clone, Default)]
pub struct PendingSystemNotes(Arc<Mutex<PendingSystemQueue>>);

impl PendingSystemNotes {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&self, note: impl Into<String>) {
        if let Ok(mut guard) = self.0.lock() {
            guard.notes.push(note.into());
        }
    }

    pub fn drain(&self) -> Vec<String> {
        self.0
            .lock()
            .map(|mut g| std::mem::take(&mut g.notes))
            .unwrap_or_default()
    }

    pub fn push_subagent_status(&self, job_id: impl Into<String>, status: &'static str) {
        if let Ok(mut guard) = self.0.lock() {
            guard.subagent_statuses.push((job_id.into(), status));
        }
    }

    pub fn drain_subagent_statuses(&self) -> Vec<(String, &'static str)> {
        self.0
            .lock()
            .map(|mut g| std::mem::take(&mut g.subagent_statuses))
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
    Subagents,
    /// `/spawn [--driver <id>] <tarea>` (`SDD-10` Fase 4): subagente en segundo plano sin
    /// turno de `root`. `driver` es `None`/`"native"` → motor en proceso; otro `id` →
    /// harness CLI de `[[external_drivers]]`.
    SpawnSubagent {
        driver: Option<String>,
        goal: Option<String>,
    },
    MergeSubagent(String),
    DiscardSubagent(String),
    Rewind(Option<u64>),
    RewindInvalid(String),
    Fork(Option<String>),
    Trust(Option<String>),
    /// `/profile [id]` (`SDD-03` Fase 2): cambia a un perfil LLM registrado o lista los disponibles.
    Profile(Option<String>),
    /// `/provider <gemini|openai|anthropic>` (`SDD-03` Fase 2): conmuta el provider LLM en caliente.
    Provider(Option<String>),
    /// `/model <name>` (`SDD-03` Fase 2): cambia el modelo del objetivo LLM activo.
    Model(Option<String>),
    /// `/effort [auto|low|medium|high]` (`SDD-03` Fase 2): cambia el esfuerzo de razonamiento.
    Effort(Option<String>),
    Cancel {
        all: bool,
    },
    /// `! <cmd>` (`SDD-07` Fase 2): shell determinista sellado; se ejecuta en el
    /// borde de sesión (sandbox + `[permissions]` + `WorkspaceMode`) y el
    /// resultado viaja sellado en `ONYXTR2`. En replay es no-op (0 I/O).
    DetShell(String),
    /// `/read <ruta> [start..end|start-end]` (`SDD-07` Fase 2): lectura directa
    /// sellada (mismo patrón que [`SlashCommand::DetShell`]). Líneas 1-based.
    DetRead {
        path: String,
        start_line: Option<usize>,
        end_line: Option<usize>,
    },
    /// `/read` con rango/path inválido (`SDD-07` Fase 2): nota de uso accionable.
    DetReadInvalid(String),
    /// `/inject <texto>` (`SDD-07` Fase 2): memoria `user` verificada **sin
    /// disparar LLM** (queda disponible para el siguiente turno real).
    Inject(String),
    /// `/step [n]` (`SDD-07` Fase 2): `n` ticks deterministas con 0 LLM
    /// (requiere congelación; clamp `1..=64`).
    Step(usize),
    /// `/step-llm` (`SDD-07` Fase 2): autoriza exactamente 1 turno LLM.
    StepLlm,
    /// `/go` / `/unfreeze` (`SDD-07` Fase 2): descongela y reanuda al LLM.
    Go,
    /// `/time` (`SDD-07` Fase 2): conmuta congelación.
    TimeToggle,
    /// `/graph inspect [agent|mem|ws]` (`SDD-07` Fase 2): inspección read-only.
    GraphInspect(Option<String>),
    /// `/graph drop-mem <seq|a..b>` (`SDD-07` Fase 2): `unrelate`+`destroy` por
    /// `mem_seq` inclusivo (sin decrementar `mem_count`; `mem_seq` monótono).
    GraphDropMem {
        start: i64,
        end: i64,
    },
    /// `/graph edit-mem <seq> <texto>` (`SDD-07` Fase 2): reescribe `mem_text`.
    GraphEditMem {
        seq: i64,
        text: String,
    },
    /// `/graph set-goal <texto>` (`SDD-07` Fase 2): reescribe `goal` del root.
    GraphSetGoal(String),
    /// `/graph <sub>` no válido (`SDD-07` Fase 2): nota listando los 4 válidos.
    GraphInvalid(String),
}

pub fn parse_slash_command(raw: &str) -> Option<SlashCommand> {
    let cmd = raw.trim();
    let lower = cmd.to_ascii_lowercase();
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
    // `SDD-03` Fase 3: la **palabra** del comando matchea case-insensitive
    // (coherente con `is_immediate_slash_cmd` de TUI/Web y el patrón ASCII-safe
    // de `/spawn`); el **argumento** se toma de `cmd` para conservar la case del
    // usuario.
    if lower == "/profile" {
        return Some(SlashCommand::Profile(None));
    }
    if lower.starts_with("/profile ") {
        let trimmed = cmd["/profile ".len()..].trim();
        return Some(SlashCommand::Profile(if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }));
    }
    if lower == "/provider" {
        return Some(SlashCommand::Provider(None));
    }
    if lower.starts_with("/provider ") {
        let trimmed = cmd["/provider ".len()..].trim();
        return Some(SlashCommand::Provider(if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }));
    }
    if lower == "/model" {
        return Some(SlashCommand::Model(None));
    }
    if lower.starts_with("/model ") {
        let trimmed = cmd["/model ".len()..].trim();
        return Some(SlashCommand::Model(if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }));
    }
    if lower == "/effort" {
        return Some(SlashCommand::Effort(None));
    }
    if lower.starts_with("/effort ") {
        let trimmed = cmd["/effort ".len()..].trim();
        return Some(SlashCommand::Effort(if trimmed.is_empty() {
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
    if cmd == "/trust" {
        return Some(SlashCommand::Trust(None));
    }
    if let Some(rest) = cmd.strip_prefix("/trust ") {
        let trimmed = rest.trim();
        return Some(SlashCommand::Trust(if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }));
    }
    if lower == "/subagents" || lower == "/subagent" {
        return Some(SlashCommand::Subagents);
    }
    if let Some(rest) = cmd.strip_prefix("/merge ") {
        let trimmed = rest.trim();
        if !trimmed.is_empty() {
            return Some(SlashCommand::MergeSubagent(trimmed.to_string()));
        }
    }
    if let Some(rest) = cmd.strip_prefix("/discard ") {
        let trimmed = rest.trim();
        if !trimmed.is_empty() {
            return Some(SlashCommand::DiscardSubagent(trimmed.to_string()));
        }
    }
    if lower == "/spawn" {
        return Some(SlashCommand::SpawnSubagent {
            driver: None,
            goal: None,
        });
    }
    if lower.starts_with("/spawn ") {
        // `SDD-10` Fase 4: comando case-insensitive; los **args** se toman de `cmd` en la
        // misma posición (se conserva la mayúscula/minúscula del usuario, a diferencia del
        // resto de comandos que solo comparan en minúsculas), con tolerancia a espacios
        // múltiples (`trim_start` tras `/spawn ` y tras `--driver`, `splitn` en
        // cualquier carácter de whitespace).
        let rest = cmd["/spawn ".len()..].trim_start();
        let first_token = rest.split(char::is_whitespace).next().unwrap_or("");
        let (driver, goal_src) = if first_token.eq_ignore_ascii_case("--driver") {
            let after = rest["--driver".len()..].trim_start();
            if after.is_empty() {
                // Flag incompleta (sin id): sin driver ni tarea → nota de uso.
                (None, "")
            } else {
                let mut parts = after.splitn(2, char::is_whitespace);
                let id = parts.next().unwrap_or("").trim();
                let driver = if id.is_empty() {
                    None
                } else {
                    Some(id.to_string())
                };
                let goal_src = parts.next().map(str::trim).unwrap_or("");
                (driver, goal_src)
            }
        } else {
            (None, rest.trim_end())
        };
        return Some(SlashCommand::SpawnSubagent {
            driver,
            goal: if goal_src.is_empty() {
                None
            } else {
                Some(goal_src.to_string())
            },
        });
    }
    if lower == "/cancel" || lower == "/stop" {
        return Some(SlashCommand::Cancel { all: false });
    }
    if let Some(rest) = lower
        .strip_prefix("/cancel ")
        .or_else(|| lower.strip_prefix("/stop "))
    {
        let arg = rest.trim();
        return Some(SlashCommand::Cancel {
            all: arg == "all" || arg == "root",
        });
    }

    // --- `SDD-07` Fase 2: primitivas 0-tokens y cirugía `/graph` -------------
    // La **palabra** matchea case-insensitive; los **args** conservan la case
    // del usuario (patrón `/spawn`). El `!` es un prefijo reservado (no slash).

    if cmd.starts_with('!') {
        let rest = cmd.strip_prefix('!').unwrap_or("").trim();
        return Some(SlashCommand::DetShell(rest.to_string()));
    }

    if lower == "/read" || lower.starts_with("/read ") {
        let rest = cmd["/read".len()..].trim();
        if rest.is_empty() {
            return Some(SlashCommand::DetReadInvalid(
                "usage: /read <path> [start..end]".to_string(),
            ));
        }
        let (path, range_tok) = split_last_token(rest);
        let (start_line, end_line) = parse_read_range(range_tok).unwrap_or((None, None));
        let path = if start_line.is_some() || end_line.is_some() {
            path
        } else {
            // El último token no era rango: es parte de la ruta.
            rest.to_string()
        };
        return Some(SlashCommand::DetRead {
            path,
            start_line,
            end_line,
        });
    }

    if lower == "/inject" || lower.starts_with("/inject ") {
        let trimmed = cmd["/inject".len()..].trim();
        return Some(SlashCommand::Inject(trimmed.to_string()));
    }

    if lower == "/step" {
        return Some(SlashCommand::Step(1));
    }
    if lower.starts_with("/step ") {
        let trimmed = cmd["/step".len()..].trim();
        let n = trimmed.parse::<usize>().ok().unwrap_or(1).clamp(1, 64);
        return Some(SlashCommand::Step(n));
    }
    if lower == "/step-llm" {
        return Some(SlashCommand::StepLlm);
    }
    if lower == "/go" || lower == "/unfreeze" {
        return Some(SlashCommand::Go);
    }
    if lower == "/time" {
        return Some(SlashCommand::TimeToggle);
    }

    if lower == "/graph" || lower.starts_with("/graph ") {
        let rest = cmd["/graph".len()..].trim();
        if rest.is_empty() {
            return Some(SlashCommand::GraphInvalid(
                "usage: /graph <inspect|drop-mem|edit-mem|set-goal>".to_string(),
            ));
        }
        let mut parts = rest.splitn(2, char::is_whitespace);
        let sub = parts.next().unwrap_or("").to_ascii_lowercase();
        let arg = parts.next().unwrap_or("").trim();
        let cmd = match sub.as_str() {
            "inspect" => SlashCommand::GraphInspect(if arg.is_empty() {
                None
            } else {
                Some(arg.to_string())
            }),
            "drop-mem" => {
                let usage = "usage: /graph drop-mem <seq|a..b>".to_string();
                if arg.is_empty() {
                    SlashCommand::GraphInvalid(usage)
                } else if let Some((a, b)) = parse_i64_range(arg) {
                    SlashCommand::GraphDropMem { start: a, end: b }
                } else if let Ok(s) = arg.parse::<i64>() {
                    SlashCommand::GraphDropMem { start: s, end: s }
                } else {
                    SlashCommand::GraphInvalid(usage)
                }
            }
            "edit-mem" => {
                let usage = "usage: /graph edit-mem <seq> <text>".to_string();
                let mut p = arg.splitn(2, char::is_whitespace);
                let seq_tok = p.next().unwrap_or("").trim();
                let text = p.next().unwrap_or("").trim();
                match seq_tok.parse::<i64>() {
                    Ok(seq) if !text.is_empty() => SlashCommand::GraphEditMem {
                        seq,
                        text: text.to_string(),
                    },
                    _ => SlashCommand::GraphInvalid(usage),
                }
            }
            "set-goal" => {
                if arg.is_empty() {
                    SlashCommand::GraphInvalid("usage: /graph set-goal <text>".to_string())
                } else {
                    SlashCommand::GraphSetGoal(arg.to_string())
                }
            }
            other => SlashCommand::GraphInvalid(format!(
                "unknown /graph subcommand `{other}` (use `inspect`, `drop-mem`, `edit-mem` or `set-goal`)"
            )),
        };
        return Some(cmd);
    }

    None
}

/// Divide `s` en (todo salvo el último token, con trailing trim, último token).
fn split_last_token(s: &str) -> (String, &str) {
    match s.rfind(char::is_whitespace) {
        Some(idx) => {
            let path = s[..idx].trim_end();
            let last = s[idx + 1..].trim_start();
            (path.to_string(), last)
        }
        None => (String::new(), s.trim()),
    }
}

/// Parsea un rango de `/read`: `a..b` o `a-b` (1-based inclusivo) o `a`
/// (solo inicio). `None` si no es un rango numérico válido.
fn parse_read_range(tok: &str) -> Option<(Option<usize>, Option<usize>)> {
    let digits = |t: &str| t.parse::<usize>().ok().filter(|&n| n >= 1);
    if let Some((a, b)) = tok.split_once("..") {
        return match (digits(a), digits(b)) {
            (Some(x), Some(y)) => Some((Some(x), Some(y))),
            _ => None,
        };
    }
    if let Some((a, b)) = tok.split_once('-') {
        return match (digits(a), digits(b)) {
            (Some(x), Some(y)) => Some((Some(x), Some(y))),
            _ => None,
        };
    }
    if let Some(a) = digits(tok) {
        return Some((Some(a), None));
    }
    None
}

/// Parsea un rango `a..b` (o `a-b`) de enteros para `/graph drop-mem`.
fn parse_i64_range(tok: &str) -> Option<(i64, i64)> {
    let nums = |t: &str| t.trim().parse::<i64>().ok();
    if let Some((a, b)) = tok.split_once("..") {
        let (a, b) = (nums(a), nums(b));
        if let (Some(a), Some(b)) = (a, b) {
            if a <= b {
                return Some((a, b));
            }
        }
        return None;
    }
    if let Some((a, b)) = tok.split_once('-') {
        // `-` también es signo: solo tratamos como rango si ambas partes parsean.
        let (a, b) = (nums(a), nums(b));
        if let (Some(a), Some(b)) = (a, b) {
            if a <= b {
                return Some((a, b));
            }
        }
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
    subagents: Option<SubagentHandle>,
    workspace_root: Option<PathBuf>,
    snapshot_store: Option<SharedPackStore>,
    pending_action: PendingSessionAction,
    pending_notes: PendingSystemNotes,
    /// Registro LLM para hot-swap en caliente (`SDD-03` Fase 2); `None` en sesiones
    /// sin LLM (tests, `run` headless sin handle inyectado).
    llm_handle: Option<LlmProfileHandle>,
    /// Telemetría efímera para sincronizar el modelo activo en `/effort` etc.
    telemetry: Option<SessionTelemetryHandle>,
    auto_approve: bool,
    max_steps_per_turn: i64,
    force_new_agent: bool,
    counter: usize,
    /// Nombres de tools MCP `mcp__*` del registro de arranque (`SDD-04 REQ-04.3`):
    /// se añaden a los tools del spec de cada agente (orden determinista).
    mcp_tool_names: Vec<String>,
    /// `SDD-04` 4C-audit: handle a `WorkspaceTools` para que `/compact` limpie
    /// `seen_reads` (los anclajes de deduplicación apuntan a lecturas que el
    /// compactado acaba de sacar del contexto).
    tools: Option<WorkspaceTools>,
}

impl CommandSpawnerSystem {
    pub fn new(auto_approve: bool) -> Self {
        Self {
            vocab: None,
            ws_vocab: None,
            mode_handle: WorkspaceModeHandle::default(),
            sprout: None,
            subagents: None,
            workspace_root: None,
            snapshot_store: None,
            pending_action: PendingSessionAction::new(),
            pending_notes: PendingSystemNotes::new(),
            llm_handle: None,
            telemetry: None,
            auto_approve,
            max_steps_per_turn: 128,
            force_new_agent: false,
            counter: 0,
            mcp_tool_names: Vec::new(),
            tools: None,
        }
    }

    pub fn with_subagents(mut self, subagents: SubagentHandle) -> Self {
        self.subagents = Some(subagents);
        self
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

    pub fn with_workspace_root(mut self, root: PathBuf) -> Self {
        self.workspace_root = Some(root);
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

    /// Registro LLM para hot-swap en caliente (`SDD-03` Fase 2): habilita
    /// `/profile`/`/provider`/`/model`/`/effort` y el sellado `ws.llm.*` en tick 0.
    pub fn with_llm_profile(mut self, handle: LlmProfileHandle) -> Self {
        self.llm_handle = Some(handle);
        self
    }

    /// Telemetría efímera (el spawner sincroniza el modelo activo en caliente).
    pub fn with_telemetry(mut self, telemetry: SessionTelemetryHandle) -> Self {
        self.telemetry = Some(telemetry);
        self
    }

    /// Tools MCP `mcp__*` del registro de arranque (`SDD-04 REQ-04.3`): se
    /// declaran en el spec de cada agente nuevo.
    pub fn with_mcp_tool_names(mut self, mcp_tool_names: Vec<String>) -> Self {
        self.mcp_tool_names = mcp_tool_names;
        self
    }

    /// `SDD-04` 4C-audit: habilita que `/compact` limpie la caché de
    /// deduplicación de `fs_read` (`WorkspaceTools::clear_seen_reads`).
    pub fn with_tools(mut self, tools: WorkspaceTools) -> Self {
        self.tools = Some(tools);
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
        let mut tool_names: Vec<&str> = WorkspaceTools::all_tool_names_with_subagents().to_vec();
        tool_names.extend(self.mcp_tool_names.iter().map(|t| t.as_str()));
        let mut spec =
            AgentSpec::new(format!("openonyx-{}", self.counter), goal).with_tools(tool_names);
        if !self.auto_approve {
            // `SDD-04 REQ-04.4`: los tools MCP `mcp__*` requieren aprobación cuando no
            // hay auto-approve (misma regla que los built-in mutantes).
            let mut approval: Vec<&str> = WorkspaceTools::approval_required_names().to_vec();
            approval.extend(self.mcp_tool_names.iter().map(|t| t.as_str()));
            spec = spec.with_approval_tools(approval);
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

/// `SDD-07` Fase 2: lee un atributo `Text` del grafo (read-only, para
/// `/graph inspect`).
fn graph_text<G: GraphView + ?Sized>(graph: &G, id: ConceptId, attr: Attr) -> Option<String> {
    match graph.get(id, attr) {
        Some(Value::Text(s)) => Some(s.clone()),
        _ => None,
    }
}

/// `SDD-07` Fase 2: lee un atributo `Int` del grafo (read-only, para
/// `/graph inspect`).
fn graph_int<G: GraphView + ?Sized>(graph: &G, id: ConceptId, attr: Attr) -> Option<i64> {
    match graph.get(id, attr) {
        Some(Value::Int(n)) => Some(n),
        _ => None,
    }
}

impl CommandSpawnerSystem {
    /// `SDD-03` Fase 2: nota de error accionable cuando un comando LLM se ejecuta
    /// sin handle (sesión sin LLM inyectado).
    fn llm_cmd_unavailable(&self, ctx: &mut Ctx, v: &Vocab, now: u64, cmd: &str) {
        if let Some(root_id) = find_latest_root_agent(&*ctx.graph, v) {
            append_agent_memory(
                ctx.graph,
                v,
                root_id,
                "system",
                &format!(
                    "[{cmd}] LLM hot-swap is not available in this session (no LLM profile handle)."
                ),
                now,
            );
        }
    }

    /// `SDD-03` Fase 2: nota al agente raíz (memoria `system`) para comandos LLM.
    fn llm_note(&self, ctx: &mut Ctx, v: &Vocab, now: u64, note: &str) {
        if let Some(root_id) = find_latest_root_agent(&*ctx.graph, v) {
            append_agent_memory(ctx.graph, v, root_id, "system", note, now);
        }
    }

    /// `SDD-03` Fase 2: aplica un cambio modal LLM — persiste `ws.llm.*` en el grafo
    /// EAV (sin credenciales), sincroniza la telemetría efímera y notifica al root.
    fn apply_llm_target(
        &self,
        ctx: &mut Ctx,
        v: &Vocab,
        now: u64,
        target: &ResolvedLlmTarget,
        note: &str,
    ) {
        if let Some(wv) = self.ws_vocab {
            set_workspace_llm_profile(
                ctx.graph,
                &wv,
                &RecordedLlmProfile {
                    profile_id: target.profile_id.clone(),
                    provider: target.provider.as_str().to_string(),
                    model: target.model.clone(),
                    effort: target.effort.map(|e| e.as_str().to_string()),
                },
            );
        }
        if let Some(tel) = &self.telemetry {
            tel.set_active_model(&target.model, target.is_local_endpoint);
        }
        if let Some(root_id) = find_latest_root_agent(&*ctx.graph, v) {
            append_agent_memory(ctx.graph, v, root_id, "system", note, now);
        }
    }

    /// `SDD-07` Fase 2: `/graph inspect [agent|mem|ws]` — informe read-only
    /// determinista en memoria `system` del root (sin gate, sin efectos EAV
    /// más que la propia memoria).
    /// Reejecuta una mutación de `/graph` sellada en-tick (fix de auditoría
    /// Fase 2): `unrelate`+`destroy` por `mem_seq` inclusivo (sin decrementar
    /// `mem_count`), reescrita de `mem_text` o de `goal`. Función pura de
    /// (grafo, comando, tick): **no** se tocan atributos reservados
    /// (`state`, `pending`, `steps`, `max_steps`) y en replay produce el mismo
    /// EAV (0 I/O).
    fn apply_det_graph_op(&mut self, ctx: &mut Ctx, v: &Vocab, now: u64, op: SealedGraphOp) {
        let Some(root_id) = find_latest_root_agent(&*ctx.graph, v) else {
            return;
        };
        match op {
            SealedGraphOp::DropMem { start, end } => {
                let mut dropped = 0;
                for mem in ctx.graph.related(root_id, v.rel_memory) {
                    let in_range = matches!(
                        ctx.graph.get(mem, v.mem_seq),
                        Some(Value::Int(n)) if (start..=end).contains(&n)
                    );
                    if in_range {
                        ctx.graph.unrelate(root_id, v.rel_memory, mem);
                        ctx.graph.destroy(mem);
                        dropped += 1;
                    }
                }
                // Misma disciplina que `/compact`: la deduplicación de
                // `fs_read` anclaba a lecturas que ya no están en contexto.
                if let Some(tools) = &self.tools {
                    tools.clear_seen_reads();
                }
                append_agent_memory(
                    ctx.graph,
                    v,
                    root_id,
                    "system",
                    &format!(
                        "[/graph drop-mem] Dropped {dropped} memory node(s) with mem_seq in [{start}..={end}]. `mem_count` stays monotonic (gaps are intentional)."
                    ),
                    now,
                );
            }
            SealedGraphOp::EditMem { seq, text } => {
                let mut found = false;
                for mem in ctx.graph.related(root_id, v.rel_memory) {
                    if matches!(
                        ctx.graph.get(mem, v.mem_seq),
                        Some(Value::Int(n)) if n == seq
                    ) {
                        ctx.graph.set(mem, v.mem_text, Value::Text(text.clone()));
                        found = true;
                        break;
                    }
                }
                if let Some(tools) = &self.tools {
                    tools.clear_seen_reads();
                }
                let note = if found {
                    format!("[/graph edit-mem] Rewrote mem_seq {seq}.")
                } else {
                    format!(
                        "[/graph edit-mem] No memory with mem_seq {seq} on root (seqs are monotonic; use `/graph inspect mem` to list them)."
                    )
                };
                append_agent_memory(ctx.graph, v, root_id, "system", &note, now);
            }
            SealedGraphOp::SetGoal { text } => {
                ctx.graph.set(root_id, v.goal, Value::Text(text.clone()));
                let preview: String = text.chars().take(80).collect();
                append_agent_memory(
                    ctx.graph,
                    v,
                    root_id,
                    "system",
                    &format!("[/graph set-goal] Root goal rewritten.\n  goal: {preview}"),
                    now,
                );
            }
        }
    }

    fn graph_inspect(&self, ctx: &mut Ctx, v: &Vocab, now: u64, scope: Option<String>) {
        let Some(root_id) = find_latest_root_agent(&*ctx.graph, v) else {
            return;
        };
        let agents = ctx.graph.query(&[v.state]);
        let mut report = String::from("[/graph inspect] ");
        match scope.as_deref().map(str::to_ascii_lowercase) {
            Some(ref s) if s == "agent" => {
                report.push_str(&format!("{} agent(s):\n", agents.len()));
                for id in &agents {
                    let is_root = ctx.graph.related(*id, v.rel_parent).is_empty();
                    let state =
                        graph_text(&*ctx.graph, *id, v.state).unwrap_or_else(|| "?".to_string());
                    let goal = graph_text(&*ctx.graph, *id, v.goal).unwrap_or_default();
                    let goal_preview: String = goal.chars().take(60).collect();
                    report.push_str(&format!(
                        "  - #{} {} state={state} mem={}\n    goal: {goal_preview}\n",
                        id.0,
                        if is_root { "[root]" } else { "[sub]" },
                        ctx.graph.related(*id, v.rel_memory).len(),
                    ));
                }
            }
            Some(ref s) if s == "mem" => {
                let mems = ctx.graph.related(root_id, v.rel_memory);
                report.push_str(&format!(
                    "root memories: {} (mem_count={})",
                    mems.len(),
                    graph_int(&*ctx.graph, root_id, v.mem_count).unwrap_or(0)
                ));
                for mem in &mems {
                    let seq = graph_int(&*ctx.graph, *mem, v.mem_seq).unwrap_or(-1);
                    let kind = graph_text(&*ctx.graph, *mem, v.mem_kind).unwrap_or_default();
                    let tick = graph_int(&*ctx.graph, *mem, v.mem_tick).unwrap_or(0);
                    let text = graph_text(&*ctx.graph, *mem, v.mem_text).unwrap_or_default();
                    let preview: String = text.replace('\n', "\\n").chars().take(80).collect();
                    report.push_str(&format!("\n  seq={seq} kind={kind} tick={tick} {preview}"));
                }
            }
            Some(ref s) if s == "ws" => {
                report.push_str(&format!(
                    "workspace mode={} · root={} · agents={}",
                    self.mode_handle.get().as_label(),
                    self.workspace_root
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "(none)".to_string()),
                    agents.len(),
                ));
            }
            Some(ref other) => {
                report.push_str(&format!(
                    "unknown scope `{other}` (use `agent`, `mem` or `ws`)"
                ));
            }
            None => {
                let root_state =
                    graph_text(&*ctx.graph, root_id, v.state).unwrap_or_else(|| "?".to_string());
                let root_goal = graph_text(&*ctx.graph, root_id, v.goal).unwrap_or_default();
                let goal_preview: String = root_goal.chars().take(60).collect();
                report.push_str(&format!(
                    "tick={} · mode={} · agents={} · root state={root_state}\n  goal: {goal_preview}",
                    now,
                    self.mode_handle.get().as_label(),
                    agents.len(),
                ));
            }
        }
        append_agent_memory(ctx.graph, v, root_id, "system", &report, now);
    }
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
            // `SDD-03` Fase 2: sincroniza el estado modal LLM restaurado (read-only,
            // idempotente; `chat --resume --tick`, `/rewind` y replay lo invocan vía tick).
            if let (Some(handle), Some(rec)) =
                (&self.llm_handle, graph_workspace_llm_profile(graph, &wv))
            {
                handle.restore_from_graph(&rec);
                if let Some(tel) = &self.telemetry {
                    let target = handle.active_target();
                    tel.set_active_model(&target.model, target.is_local_endpoint);
                }
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

        let status_updates = self.pending_notes.drain_subagent_statuses();
        if !status_updates.is_empty() {
            if let Some(wv) = self.ws_vocab {
                for (job_id, status) in status_updates {
                    update_subagent_status(ctx.graph, &wv, &job_id, status);
                }
            }
        }

        let notes = self.pending_notes.drain();
        if !notes.is_empty() {
            if let Some(root_id) = find_latest_root_agent(&*ctx.graph, &v) {
                for note in notes {
                    append_agent_memory(ctx.graph, &v, root_id, "system", &note, now);
                }
            }
        }

        // `SDD-03` Fase 2: sella `ws.llm.*` desde el tick 0 (idempotente: en resume el
        // grafo ya trae el estado modal; en replay el handle best-effort escribe el
        // mismo shape, conservando la asignación EAV idéntica al `chat` en vivo).
        if let (Some(wv), Some(handle)) = (self.ws_vocab, &self.llm_handle) {
            if graph_workspace_llm_profile(&*ctx.graph, &wv).is_none() {
                let target = handle.active_target();
                set_workspace_llm_profile(
                    ctx.graph,
                    &wv,
                    &RecordedLlmProfile {
                        profile_id: target.profile_id.clone(),
                        provider: target.provider.as_str().to_string(),
                        model: target.model.clone(),
                        effort: target.effort.map(|e| e.as_str().to_string()),
                    },
                );
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

            // Ingesta determinista y proactiva de propuestas `SubagentProposal` en frontera de tick
            // (`SDD-09` Fase 3): reconcilia semánticamente en grafo (`job_id` / `ws.file.path`, nunca
            // `ConceptId`) y despierta proactivamente a `root` si estaba ocioso sin hacer polling.
            if let Some((proposal, cancelled)) = decode_proposal_command(cmd) {
                let status = if cancelled {
                    "cancelled"
                } else if proposal.ok {
                    "done"
                } else {
                    "failed"
                };
                if let Some(wv) = self.ws_vocab {
                    upsert_subagent_record(ctx.graph, &wv, &proposal, status);
                }
                let touched = if proposal.touched_files.is_empty() {
                    "(no file edits)".to_string()
                } else {
                    proposal.touched_files.join(", ")
                };
                let summary_preview: String = proposal.summary.trim().chars().take(240).collect();
                let notice = format!(
                    "[subagent `{}` finished · status={status} · branch={}]\ntouched: {touched}\nsummary: {summary_preview}\nUse `subagent_merge` (or `/merge {id}`) to reconcile 3-way into root, or `subagent_discard` (`/discard {id}`) to drop the worktree.",
                    proposal.job_id,
                    proposal.branch_name,
                    id = proposal.job_id
                );
                if let Some(root_id) = find_latest_root_agent(&*ctx.graph, &v) {
                    if cancelled
                        || !continue_agent(
                            ctx.graph,
                            &v,
                            root_id,
                            &notice,
                            self.max_steps_per_turn,
                            now,
                        )
                    {
                        append_agent_memory(ctx.graph, &v, root_id, "system", &notice, now);
                    }
                }
                continue;
            }

            // `SDD-07` Fase 2: reinyección del resultado sellado grabado en
            // `ONYXTR2` durante el borde de sesión en vivo (`! <cmd>` /
            // `/read`). El spawner es la única función de (grafo, comando,
            // tick): convierte el **resultado ya obtenido** en memoria del
            // root con `mem_kind` `"shell"`/`"observe"` — 0 I/O, 0 LLM; en
            // replay no se vuelve a ejecutar nada (bit-a-bit).
            if let Some(result) = decode_det_shell_command(cmd) {
                if let Some(root_id) = find_latest_root_agent(&*ctx.graph, &v) {
                    let report = format!("[det-shell] `{}`\n{}", result.command, result.output);
                    append_agent_memory(ctx.graph, &v, root_id, "shell", &report, now);
                }
                continue;
            }
            if let Some(result) = decode_det_read_command(cmd) {
                if let Some(root_id) = find_latest_root_agent(&*ctx.graph, &v) {
                    let range = match (result.start_line, result.end_line) {
                        (Some(a), Some(b)) => format!(" (lines {a}..{b})"),
                        (Some(a), None) => format!(" (from line {a})"),
                        (None, Some(b)) => format!(" (up to line {b})"),
                        (None, None) => String::new(),
                    };
                    let report = format!("[det-read] `{}`{range}\n{}", result.path, result.content);
                    append_agent_memory(ctx.graph, &v, root_id, "observe", &report, now);
                }
                continue;
            }
            // Fix de auditoría Fase 2: memoria `user` sellada de `/inject`
            // (0 I/O, 0 LLM; bit-a-bit en replay).
            if let Some(result) = decode_det_inject_command(cmd) {
                if let Some(root_id) = find_latest_root_agent(&*ctx.graph, &v) {
                    append_agent_memory(ctx.graph, &v, root_id, "user", &result.text, now);
                }
                continue;
            }
            // Fix de auditoría Fase 2: mutación sellada de `/graph`
            // (función pura de (grafo, comando, tick): bit-a-bit en replay).
            if let Some(op) = decode_det_graph_command(cmd) {
                self.apply_det_graph_op(ctx, &v, now, op);
                continue;
            }
            // `SDD-07` Fase 2: comando sellado `/step-llm`: reactiva el root
            // (`Done`/`Failed` → `Idle`) en-tick, sin memoria `user` y con 0 I/O.
            // El siguiente tick el runtime lo lleva a `Thinking` y el gate
            // (`try_acquire_llm_turn`, presupuesto 1) despacha exactamente 1
            // turno; en replay el mismo comando produce el mismo EAV.
            if cmd.starts_with(DET_STEP_LLM_CMD_PREFIX) {
                if let Some(root_id) = find_latest_root_agent(&*ctx.graph, &v) {
                    let terminal = matches!(
                        ctx.graph.get(root_id, v.state),
                        Some(Value::Text(ref s))
                            if AgentState::parse(s).is_some_and(|st| st.is_terminal())
                    );
                    if terminal {
                        let steps = match ctx.graph.get(root_id, v.steps) {
                            Some(Value::Int(n)) => n,
                            _ => 0,
                        };
                        ctx.graph.set(
                            root_id,
                            v.max_steps,
                            Value::Int(steps + self.max_steps_per_turn),
                        );
                        ctx.graph.set(root_id, v.result, Value::Text(String::new()));
                        ctx.graph.set(root_id, v.pending, Value::Int(0));
                        ctx.graph.set(
                            root_id,
                            v.state,
                            Value::Text(AgentState::Idle.as_str().into()),
                        );
                    }
                }
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
                    SlashCommand::Profile(arg_opt) => {
                        let Some(handle) = self.llm_handle.clone() else {
                            self.llm_cmd_unavailable(ctx, &v, now, "/profile");
                            continue;
                        };
                        match arg_opt {
                            Some(id) => match handle.switch_profile(&id) {
                                Ok(target) => {
                                    self.apply_llm_target(
                                        ctx,
                                        &v,
                                        now,
                                        &target,
                                        &format!(
                                            "[/profile] LLM profile `{id}` active: {}/{}, effort {}.",
                                            target.provider.as_str(),
                                            target.model,
                                            effort_label(target.effort)
                                        ),
                                    );
                                }
                                Err(err) => {
                                    self.llm_note(ctx, &v, now, &format!("[/profile] {err}"));
                                }
                            },
                            None => {
                                let snap = handle.public_snapshot();
                                let mut lines = vec![format!(
                                    "[/profile] Active: `{}` ({}, {}, effort {})",
                                    snap.active_profile_id, snap.provider, snap.model, snap.effort
                                )];
                                for p in &snap.profiles {
                                    let mark = if p.id == snap.active_profile_id {
                                        "*"
                                    } else {
                                        " "
                                    };
                                    if p.available {
                                        lines.push(format!(
                                            "  {mark} `{}` {} {} (effort {}, available)",
                                            p.id, p.provider, p.model, p.effort
                                        ));
                                    } else {
                                        lines.push(format!(
                                            "  {mark} `{}` {} {} (effort {}, unavailable: {})",
                                            p.id,
                                            p.provider,
                                            p.model,
                                            p.effort,
                                            p.unavailable_reason.as_deref().unwrap_or("no details")
                                        ));
                                    }
                                }
                                lines.push(
                                    "Use `/profile <id>` to switch (or `/provider`, `/model`, `/effort`).".to_string(),
                                );
                                self.llm_note(ctx, &v, now, &lines.join("\n"));
                            }
                        }
                    }
                    SlashCommand::Provider(arg_opt) => {
                        let Some(handle) = self.llm_handle.clone() else {
                            self.llm_cmd_unavailable(ctx, &v, now, "/provider");
                            continue;
                        };
                        let Some(arg) = arg_opt else {
                            let avail = handle.public_snapshot().available_providers.join(", ");
                            self.llm_note(
                                ctx,
                                &v,
                                now,
                                &format!(
                                    "[/provider] Usage: `/provider <name>`. Available providers: {avail}."
                                ),
                            );
                            continue;
                        };
                        match LlmProvider::parse(&arg) {
                            Err(err) => self.llm_note(ctx, &v, now, &format!("[/provider] {err}")),
                            Ok(provider) => match handle.set_provider(provider) {
                                Ok(target) => {
                                    self.apply_llm_target(
                                        ctx,
                                        &v,
                                        now,
                                        &target,
                                        &format!(
                                            "[/provider] LLM provider set to `{}` (model `{}`, effort {}).",
                                            provider.as_str(),
                                            target.model,
                                            effort_label(target.effort)
                                        ),
                                    );
                                }
                                Err(err) => {
                                    self.llm_note(ctx, &v, now, &format!("[/provider] {err}"));
                                }
                            },
                        }
                    }
                    SlashCommand::Model(arg_opt) => {
                        let Some(handle) = self.llm_handle.clone() else {
                            self.llm_cmd_unavailable(ctx, &v, now, "/model");
                            continue;
                        };
                        let Some(arg) = arg_opt else {
                            self.llm_note(
                                ctx,
                                &v,
                                now,
                                "[/model] Usage: `/model <name>` (changes the model of the active LLM target in place).",
                            );
                            continue;
                        };
                        match handle.set_model(&arg) {
                            Ok(target) => {
                                self.apply_llm_target(
                                    ctx,
                                    &v,
                                    now,
                                    &target,
                                    &format!(
                                        "[/model] LLM model set to `{}` (provider `{}`, effort {}).",
                                        target.model,
                                        target.provider.as_str(),
                                        effort_label(target.effort)
                                    ),
                                );
                            }
                            Err(err) => self.llm_note(ctx, &v, now, &format!("[/model] {err}")),
                        }
                    }
                    SlashCommand::Effort(arg_opt) => {
                        let Some(handle) = self.llm_handle.clone() else {
                            self.llm_cmd_unavailable(ctx, &v, now, "/effort");
                            continue;
                        };
                        let effort = match arg_opt.as_deref() {
                            None => None,
                            Some(arg) => match arg.to_ascii_lowercase().as_str() {
                                "auto" | "none" | "off" => None,
                                other => match ReasoningEffort::parse(other) {
                                    Ok(e) => Some(e),
                                    Err(_) => {
                                        self.llm_note(
                                            ctx,
                                            &v,
                                            now,
                                            &format!(
                                                "[/effort] Unknown effort `{arg}` (use `auto`, `low`, `medium`, or `high`)."
                                            ),
                                        );
                                        continue;
                                    }
                                },
                            },
                        };
                        let target = handle.set_effort(effort);
                        self.apply_llm_target(
                            ctx,
                            &v,
                            now,
                            &target,
                            &format!(
                                "[/effort] LLM reasoning effort set to {}.",
                                effort_label(effort)
                            ),
                        );
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
                        // `SDD-04` 4C-audit: tras el compactado, los anclajes de
                        // deduplicación de `fs_read` están obsoletos (el contexto
                        // ya perdió la lectura original); se limpian para que la
                        // siguiente `fs_read` devuelva el contenido completo.
                        if let Some(tools) = &self.tools {
                            tools.clear_seen_reads();
                        }
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
                    SlashCommand::Subagents => {
                        let live_list = self
                            .subagents
                            .as_ref()
                            .map(|h| h.list_summaries())
                            .unwrap_or_default();
                        let note = if !live_list.is_empty() {
                            let mut lines = vec![format!(
                                "[/subagents] {} background worktree job(s):",
                                live_list.len()
                            )];
                            for s in live_list {
                                let files = if s.touched_files.is_empty() {
                                    "0 files".to_string()
                                } else {
                                    s.touched_files.join(", ")
                                };
                                // `SDD-10` Fase 4: badge del driver (`→ opencode`); el
                                // driver nativo es el default y no se muestra.
                                let driver_badge = if s.driver == "native" {
                                    String::new()
                                } else {
                                    format!("→ {} ", s.driver)
                                };
                                lines.push(format!(
                                    "  - [bg #{}] ({}) {driver_badge}branch={} touched=[{}] goal=\"{}\"",
                                    s.job_id, s.status, s.branch_name, files, s.goal
                                ));
                            }
                            lines.join("\n")
                        } else if let Some(wv) = self.ws_vocab {
                            let recs = graph_subagent_records(&*ctx.graph, &wv);
                            if recs.is_empty() {
                                "[/subagents] No async background worktree jobs.".to_string()
                            } else {
                                let mut lines = vec![format!(
                                    "[/subagents] {} recorded worktree job(s):",
                                    recs.len()
                                )];
                                for r in recs {
                                    // `SDD-10` Fase 4: badge del driver (`→ opencode`);
                                    // el driver nativo es el default y no se muestra.
                                    let driver_badge = if r.driver == "native" {
                                        String::new()
                                    } else {
                                        format!("→ {} ", r.driver)
                                    };
                                    lines.push(format!(
                                        "  - [bg #{}] ({}) {driver_badge}branch={} touched=[{}]",
                                        r.job_id,
                                        r.status,
                                        r.branch,
                                        r.touched_files.join(", ")
                                    ));
                                }
                                lines.join("\n")
                            }
                        } else {
                            "[/subagents] No async background worktree jobs.".to_string()
                        };
                        if let Some(root_id) = find_latest_root_agent(&*ctx.graph, &v) {
                            append_agent_memory(ctx.graph, &v, root_id, "system", &note, now);
                        }
                    }
                    SlashCommand::MergeSubagent(job_id) => {
                        if !self.mode_handle.get().allows_mutation() {
                            if let Some(root_id) = find_latest_root_agent(&*ctx.graph, &v) {
                                append_agent_memory(
                                    ctx.graph,
                                    &v,
                                    root_id,
                                    "system",
                                    &format!(
                                        "[/merge] Blocked in `{}` mode: switch to BUILD to merge `{job_id}`.",
                                        self.mode_handle.get().as_label()
                                    ),
                                    now,
                                );
                            }
                        } else if self.subagents.is_none() {
                            if let Some(root_id) = find_latest_root_agent(&*ctx.graph, &v) {
                                append_agent_memory(
                                    ctx.graph,
                                    &v,
                                    root_id,
                                    "system",
                                    "[/merge] Async subagent worktree manager is unavailable in this mode (offline replay).",
                                    now,
                                );
                            }
                        } else {
                            self.pending_action
                                .set(SessionAction::MergeSubagent { job_id });
                        }
                    }
                    SlashCommand::DiscardSubagent(job_id) => {
                        if self.subagents.is_none() {
                            if let Some(root_id) = find_latest_root_agent(&*ctx.graph, &v) {
                                append_agent_memory(
                                    ctx.graph,
                                    &v,
                                    root_id,
                                    "system",
                                    "[/discard] Async subagent worktree manager is unavailable in this mode (offline replay).",
                                    now,
                                );
                            }
                        } else {
                            self.pending_action
                                .set(SessionAction::DiscardSubagent { job_id });
                        }
                    }
                    SlashCommand::SpawnSubagent { driver, goal } => {
                        let Some(goal) = goal else {
                            // Nota de uso (si no hay `root` todavía, la nota es un no-op y
                            // no se muestra nada, igual que el resto de comandos).
                            if let Some(root_id) = find_latest_root_agent(&*ctx.graph, &v) {
                                append_agent_memory(
                                    ctx.graph,
                                    &v,
                                    root_id,
                                    "system",
                                    "[/spawn] Usage: `/spawn [--driver <id>] <task>` — runs the task in an isolated worktree without spending a root turn (list jobs with `/subagents`).",
                                    now,
                                );
                            }
                            continue;
                        };
                        self.pending_action
                            .set(SessionAction::SpawnSubagent { goal, driver });
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
                    SlashCommand::Trust(arg) => {
                        let action = arg
                            .as_deref()
                            .map(|s| s.trim().to_ascii_lowercase())
                            .unwrap_or_default();
                        let note = match &self.workspace_root {
                            None => {
                                "[/trust] Workspace trust is unavailable in offline replay mode."
                                    .to_string()
                            }
                            Some(root) => match action.as_str() {
                                "status" | "check" => {
                                    let rep = evaluate_workspace_trust(root, None);
                                    format!(
                                        "[/trust] status={} (config={}, sprout={})",
                                        rep.status_label(),
                                        rep.config_blake3
                                            .as_deref()
                                            .map(|h| &h[..h.len().min(12)])
                                            .unwrap_or("none"),
                                        rep.sprout_blake3
                                            .as_deref()
                                            .map(|h| &h[..h.len().min(12)])
                                            .unwrap_or("none"),
                                    )
                                }
                                "revoke" => match revoke_workspace_trust(root, None) {
                                    Ok(removed) => {
                                        let rep = evaluate_workspace_trust(root, None);
                                        if let Some(sprout) = &self.sprout {
                                            sprout.set_trust_state(
                                                rep.sprout_trusted,
                                                rep.status.clone(),
                                            );
                                            if let Some(wv) = self.ws_vocab {
                                                if let Some(panel) = sprout.render_panel_text() {
                                                    set_workspace_services(ctx.graph, &wv, &panel);
                                                }
                                            }
                                        }
                                        if removed {
                                            "[/trust] Workspace trust revoked. Project `allow` rules and `sprout.toml` execution are blocked until re-approved.".to_string()
                                        } else {
                                            "[/trust] Workspace was not in trusted_workspaces.toml."
                                                .to_string()
                                        }
                                    }
                                    Err(e) => format!("[/trust] ERROR: {e}"),
                                },
                                "" | "add" | "approve" => match trust_workspace(root, None) {
                                    Ok(rep) => {
                                        if let Some(sprout) = &self.sprout {
                                            sprout.set_trust_state(true, rep.status.clone());
                                            if let Some(wv) = self.ws_vocab {
                                                if let Some(panel) = sprout.render_panel_text() {
                                                    set_workspace_services(ctx.graph, &wv, &panel);
                                                }
                                            }
                                        }
                                        format!(
                                            "[/trust] Workspace trusted (config={}, sprout={}). Sprout is now enabled; restart `chat` to reload project `[permissions].allow` / `[sandbox]` overrides.",
                                            rep.config_blake3
                                                .as_deref()
                                                .map(|h| &h[..h.len().min(12)])
                                                .unwrap_or("none"),
                                            rep.sprout_blake3
                                                .as_deref()
                                                .map(|h| &h[..h.len().min(12)])
                                                .unwrap_or("none"),
                                        )
                                    }
                                    Err(e) => format!("[/trust] ERROR: {e}"),
                                },
                                other => format!(
                                    "[/trust] Unknown action `{other}` (usage: `/trust`, `/trust status`, or `/trust revoke`)."
                                ),
                            },
                        };
                        if let Some(root_id) = find_latest_root_agent(&*ctx.graph, &v) {
                            append_agent_memory(ctx.graph, &v, root_id, "system", &note, now);
                        }
                    }
                    SlashCommand::Cancel { all } => {
                        let live_children: Vec<ConceptId> = ctx
                            .graph
                            .query(&[v.state])
                            .into_iter()
                            .filter(|id| {
                                !ctx.graph.related(*id, v.rel_parent).is_empty()
                                    && matches!(
                                        ctx.graph.get(*id, v.state),
                                        Some(Value::Text(ref s))
                                            if AgentState::parse(s).is_some_and(|st| !st.is_terminal())
                                    )
                            })
                            .collect();

                        for &child in &live_children {
                            ctx.graph
                                .set(child, v.pending_tool, Value::Text(String::new()));
                            ctx.graph
                                .set(child, v.pending_args, Value::Text(String::new()));
                            ctx.graph
                                .set(child, v.pending_call_id, Value::Text(String::new()));
                            ctx.graph.set(child, v.pending, Value::Int(0));
                            ctx.graph.set(
                                child,
                                v.state,
                                Value::Text(AgentState::Failed.as_str().into()),
                            );
                            ctx.graph.set(
                                child,
                                v.result,
                                Value::Text("cancelled by user".to_string()),
                            );
                            append_agent_memory(
                                ctx.graph,
                                &v,
                                child,
                                "system",
                                "[/cancel] Subagent cancelled by user.",
                                now,
                            );
                        }

                        if !live_children.is_empty() && !all {
                            if let Some(root_id) = find_latest_root_agent(&*ctx.graph, &v) {
                                append_agent_memory(
                                    ctx.graph,
                                    &v,
                                    root_id,
                                    "system",
                                    &format!(
                                        "[/cancel] Cancelled {} active subagent(s); returning control to parent agent (use `/cancel all` to stop the parent too).",
                                        live_children.len()
                                    ),
                                    now,
                                );
                            }
                        } else {
                            let live_roots: Vec<ConceptId> = ctx
                                .graph
                                .query(&[v.state])
                                .into_iter()
                                .filter(|id| {
                                    ctx.graph.related(*id, v.rel_parent).is_empty()
                                        && matches!(
                                            ctx.graph.get(*id, v.state),
                                            Some(Value::Text(ref s))
                                                if AgentState::parse(s).is_some_and(|st| !st.is_terminal())
                                        )
                                })
                                .collect();
                            for &root in &live_roots {
                                ctx.graph
                                    .set(root, v.pending_tool, Value::Text(String::new()));
                                ctx.graph
                                    .set(root, v.pending_args, Value::Text(String::new()));
                                ctx.graph
                                    .set(root, v.pending_call_id, Value::Text(String::new()));
                                ctx.graph.set(root, v.pending, Value::Int(0));
                                ctx.graph.set(
                                    root,
                                    v.state,
                                    Value::Text(AgentState::Done.as_str().into()),
                                );
                                ctx.graph.set(root, v.result, Value::Text(String::new()));
                                append_agent_memory(
                                    ctx.graph,
                                    &v,
                                    root,
                                    "system",
                                    "[/cancel] Active turn interrupted by user. Ready for your next prompt.",
                                    now,
                                );
                            }
                            if live_children.is_empty() && live_roots.is_empty() {
                                if let Some(root_id) = find_latest_root_agent(&*ctx.graph, &v) {
                                    append_agent_memory(
                                        ctx.graph,
                                        &v,
                                        root_id,
                                        "system",
                                        "[/cancel] No active task to cancel.",
                                        now,
                                    );
                                }
                            }
                        }
                    }
                    // --- `SDD-07` Fase 2: primitivas 0-tokens y cirugía `/graph` ----
                    SlashCommand::DetShell(command) => {
                        if command.trim().is_empty() {
                            self.llm_note(
                                ctx,
                                &v,
                                now,
                                "[!] Usage: `! <command>` — runs a user-verified shell command (sandboxed, `[permissions]`-checked) and injects the sealed result with 0 LLM tokens.",
                            );
                        } else {
                            self.pending_action.set(SessionAction::DetShell { command });
                        }
                    }
                    SlashCommand::DetRead {
                        path,
                        start_line,
                        end_line,
                    } => {
                        self.pending_action.set(SessionAction::DetRead {
                            path,
                            start_line,
                            end_line,
                        });
                    }
                    SlashCommand::DetReadInvalid(msg) => {
                        self.llm_note(ctx, &v, now, &format!("[/read] {msg}"));
                    }
                    SlashCommand::Inject(text) => {
                        // Fix de auditoría Fase 2: el spawner solo **solicita**
                        // la acción (no-op en `openonyx replay`, 0 I/O); en vivo
                        // `apply_session_action` aplica gate + bifurcación y
                        // reinyecta el sellado `/__onyx_det_inject` (memoria
                        // `user` en-tick, 0 LLM).
                        if text.trim().is_empty() {
                            self.llm_note(
                                ctx,
                                &v,
                                now,
                                "[/inject] Usage: `/inject <text>` — appends verified user memory without spending an LLM turn.",
                            );
                        } else {
                            self.pending_action.set(SessionAction::Inject { text });
                        }
                    }
                    SlashCommand::Step(n) => {
                        self.pending_action.set(SessionAction::Step(n));
                    }
                    SlashCommand::StepLlm => {
                        self.pending_action.set(SessionAction::StepLlm);
                    }
                    SlashCommand::Go => {
                        self.pending_action.set(SessionAction::Go);
                    }
                    SlashCommand::TimeToggle => {
                        self.pending_action.set(SessionAction::TimeToggle);
                    }
                    SlashCommand::GraphInspect(scope) => {
                        self.graph_inspect(ctx, &v, now, scope);
                    }
                    SlashCommand::GraphDropMem { start, end } => {
                        // Fix de auditoría Fase 2: el spawner solo **solicita**
                        // la mutación (no-op en `openonyx replay`, 0 I/O); en
                        // vivo `apply_session_action` aplica gate +
                        // bifurcación y reinyecta el sellado
                        // `/__onyx_det_graph` (`drop-mem`/`edit-mem`/`set-goal`
                        // reejecutado en-tick; `mem_seq` monótono, atributos
                        // reservados intactos — spec SDD-07 §7).
                        self.pending_action
                            .set(SessionAction::GraphDropMem { start, end });
                    }
                    SlashCommand::GraphEditMem { seq, text } => {
                        self.pending_action
                            .set(SessionAction::GraphEditMem { seq, text });
                    }
                    SlashCommand::GraphSetGoal(text) => {
                        self.pending_action
                            .set(SessionAction::GraphSetGoal { text });
                    }
                    SlashCommand::GraphInvalid(msg) => {
                        self.llm_note(ctx, &v, now, &format!("[/graph] {msg}"));
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

#[cfg(test)]
mod tests {
    use super::*;

    /// `SDD-10` Fase 4: parseo de `/spawn [--driver <id>] <tarea>`. El comando y el flag
    /// son case-insensitive (incluido `/SPAWN` **con** args), tolera espacios múltiples
    /// (tras `/spawn ` y tras `--driver`); el `id` y la **tarea** conservan la
    /// mayúscula/minúscula del usuario (a diferencia del resto de comandos, que solo
    /// comparan en minúsculas).
    #[test]
    fn parse_slash_spawn_variants() {
        // `/spawn` solo → sin driver ni goal (nota de uso en el dispatch).
        assert_eq!(
            parse_slash_command("/spawn"),
            Some(SlashCommand::SpawnSubagent {
                driver: None,
                goal: None
            })
        );
        // El comando es case-insensitive.
        assert_eq!(
            parse_slash_command("/SPAWN"),
            Some(SlashCommand::SpawnSubagent {
                driver: None,
                goal: None
            })
        );
        // Con tarea libre: se conserva la mayúscula del usuario.
        assert_eq!(
            parse_slash_command("/spawn Fix The Parser BUG"),
            Some(SlashCommand::SpawnSubagent {
                driver: None,
                goal: Some("Fix The Parser BUG".to_string())
            })
        );
        // `--driver <id>` + tarea.
        assert_eq!(
            parse_slash_command("/spawn --driver opencode refactor the parser"),
            Some(SlashCommand::SpawnSubagent {
                driver: Some("opencode".to_string()),
                goal: Some("refactor the parser".to_string())
            })
        );
        // Flag case-insensitive; el `id` conserva la case del usuario.
        assert_eq!(
            parse_slash_command("/spawn --Driver OPCODE Fix It"),
            Some(SlashCommand::SpawnSubagent {
                driver: Some("OPCODE".to_string()),
                goal: Some("Fix It".to_string())
            })
        );
        // `--driver` sin id → driver `None`, sin goal.
        assert_eq!(
            parse_slash_command("/spawn --driver"),
            Some(SlashCommand::SpawnSubagent {
                driver: None,
                goal: None
            })
        );
        // `--driver <id>` sin tarea → driver `Some`, goal `None`.
        assert_eq!(
            parse_slash_command("/spawn --driver opencode"),
            Some(SlashCommand::SpawnSubagent {
                driver: Some("opencode".to_string()),
                goal: None
            })
        );
        // Comando case-insensitive **con** args: la tarea conserva la case del usuario.
        assert_eq!(
            parse_slash_command("/SPAWN Fix It"),
            Some(SlashCommand::SpawnSubagent {
                driver: None,
                goal: Some("Fix It".to_string())
            })
        );
        // Espacios múltiples tras `/spawn` → se toleran.
        assert_eq!(
            parse_slash_command("/spawn  --driver opencode task"),
            Some(SlashCommand::SpawnSubagent {
                driver: Some("opencode".to_string()),
                goal: Some("task".to_string())
            })
        );
        // Espacios múltiples tras `--driver` → el id y la tarea se separan igualmente.
        assert_eq!(
            parse_slash_command("/spawn --driver  opencode task"),
            Some(SlashCommand::SpawnSubagent {
                driver: Some("opencode".to_string()),
                goal: Some("task".to_string())
            })
        );
    }

    /// `SDD-03` Fase 2 (Fase 3: palabra case-insensitive): parseo de los
    /// comandos de cambio de LLM en caliente. La **palabra** del comando matchea
    /// case-insensitive (como `is_immediate_slash_cmd` de TUI/Web y `/spawn`);
    /// el **argumento** (si existe) conserva la mayúscula del usuario.
    #[test]
    fn parse_slash_llm_variants() {
        // Sin argumento → `None` (consulta de estado / nota de uso).
        assert_eq!(
            parse_slash_command("/profile"),
            Some(SlashCommand::Profile(None))
        );
        assert_eq!(
            parse_slash_command("/provider"),
            Some(SlashCommand::Provider(None))
        );
        assert_eq!(
            parse_slash_command("/model"),
            Some(SlashCommand::Model(None))
        );
        assert_eq!(
            parse_slash_command("/effort"),
            Some(SlashCommand::Effort(None))
        );
        // Con argumento → se conserva la case del usuario.
        assert_eq!(
            parse_slash_command("/profile FastModel"),
            Some(SlashCommand::Profile(Some("FastModel".to_string())))
        );
        assert_eq!(
            parse_slash_command("/provider openai"),
            Some(SlashCommand::Provider(Some("openai".to_string())))
        );
        assert_eq!(
            parse_slash_command("/model gpt-4o-mini"),
            Some(SlashCommand::Model(Some("gpt-4o-mini".to_string())))
        );
        assert_eq!(
            parse_slash_command("/effort high"),
            Some(SlashCommand::Effort(Some("high".to_string())))
        );
        // `auto`/`off`/`none` son argumentos válidos de `/effort` (el handler los
        // normaliza a `None`); aquí solo se comprueba el parseo.
        assert_eq!(
            parse_slash_command("/effort off"),
            Some(SlashCommand::Effort(Some("off".to_string())))
        );
        // Argumento con espacios múltiples → se trimean.
        assert_eq!(
            parse_slash_command("/profile   spaced   "),
            Some(SlashCommand::Profile(Some("spaced".to_string())))
        );
        // Argumento vacío → equivale al comando sin argumento.
        assert_eq!(
            parse_slash_command("/effort   "),
            Some(SlashCommand::Effort(None))
        );
        // Comandos ajenos no se ven afectados.
        assert_eq!(
            parse_slash_command("/mode build"),
            Some(SlashCommand::Mode(Some("build".to_string())))
        );
        assert_eq!(parse_slash_command("/profil"), None);
        assert_eq!(parse_slash_command("/models"), None);
        // Palabra case-insensitive (coherente con `is_immediate_slash_cmd` de
        // TUI/Web); el argumento conserva la case del usuario.
        assert_eq!(
            parse_slash_command("/PROFILE"),
            Some(SlashCommand::Profile(None))
        );
        assert_eq!(
            parse_slash_command("/Provider OPENAI"),
            Some(SlashCommand::Provider(Some("OPENAI".to_string())))
        );
        assert_eq!(
            parse_slash_command("/MODEL Gemini-4-Helium"),
            Some(SlashCommand::Model(Some("Gemini-4-Helium".to_string())))
        );
        assert_eq!(
            parse_slash_command("/EFFORT High"),
            Some(SlashCommand::Effort(Some("High".to_string())))
        );
    }
}
