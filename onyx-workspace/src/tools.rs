use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use regex::{Regex, RegexBuilder};
use serde::Deserialize;
use serde_json::{Value as JsonValue, json};

use crate::impact::{
    ImpactAssessment, apply_impact_escalation, evaluate_mutation_impact,
    evaluate_unified_diff_impact,
};
use crate::policy::{PermissionDecision, WorkspacePolicy, compile_glob_regex};
use crate::sandbox::{
    ProcessJob, clean_process_path, configure_process_isolation, ensure_sandbox_tmp_dir,
    prepare_sandboxed_shell_command,
};
use crate::sprout::SproutSupervisor;
use crate::state::WorkspaceMode;
use std::borrow::Cow;

/// Descripción declarativa de una herramienta del workspace para inyectar en LLMs.
///
/// `name` y `description` son `Cow<'static, str>` para que `mcp.rs` pueda
/// construir schemas dinámicos (`Owned`) sin clonaciones por llamada al
/// proveedor (SDD-04 Fase 3B); los schemas estáticos siguen siendo
/// `Borrowed` a coste cero.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolSchema {
    pub name: Cow<'static, str>,
    pub description: Cow<'static, str>,
    pub parameters: JsonValue,
    pub requires_approval: bool,
}

/// `SDD-04` 4C-audit: metadatos de una lectura registrada en `seen_reads`.
/// `exec_seq` es la secuencia de ejecución de tools en el momento de la
/// lectura (ancora la ventana de deduplicación); `stubbed` indica que la
/// última "lectura" devolvió un stub `@TOON` (el modelo no ha visto el
/// contenido desde entonces).
#[derive(Clone, Debug)]
struct SeenRead {
    digest: String,
    tick: u64,
    exec_seq: u64,
    stubbed: bool,
}

/// `SDD-04` 4C-audit: ventana de deduplicación de `fs_read`: re-lecturas del
/// mismo archivo/rango con digest igual dentro de `FS_READ_DEDUP_WINDOW`
/// ejecuciones de tool devuelven el stub `@TOON` (rompe bucles de re-lectura
/// sin perder visibilidad del contenido, que se garantiza alternando
/// stub/literal).
const FS_READ_DEDUP_WINDOW: u64 = 8;

/// `SDD-04` 4C-audit: máximo de caracteres por línea en la salida de
/// `code_search`; las líneas más largas se recortan en el carácter 240
/// (frontera UTF-8) y se añade `...` (evita inflar el contexto con líneas
/// minificadas).
const CODE_SEARCH_MAX_LINE_CHARS: usize = 240;

/// `SDD-04` 4C-audit: recorta una línea de `code_search` a
/// `CODE_SEARCH_MAX_LINE_CHARS` caracteres (frontera UTF-8, `char_indices`)
/// y añade `...` si se trunca.
fn clip_search_line(line: &str) -> String {
    if line.chars().count() <= CODE_SEARCH_MAX_LINE_CHARS {
        line.to_string()
    } else {
        let cut = line
            .char_indices()
            .nth(CODE_SEARCH_MAX_LINE_CHARS)
            .map(|(i, _)| i)
            .unwrap_or(line.len());
        format!("{}...", &line[..cut])
    }
}

type SeenReadsMap = Arc<Mutex<BTreeMap<(String, usize, usize), SeenRead>>>;
#[cfg(feature = "scent")]
type SeenOutlinesMap = Arc<Mutex<BTreeMap<String, String>>>;

/// Ejecutor de herramientas de código sujeto a `WorkspacePolicy`.
#[derive(Clone, Debug)]
pub struct WorkspaceTools {
    policy: WorkspacePolicy,
    seen_reads: SeenReadsMap,
    read_seq: Arc<AtomicU64>,
    /// `SDD-04` 4C-audit: secuencia global de ejecuciones de tool (se bumpa
    /// en `execute_with_tick`); ancora la ventana `FS_READ_DEDUP_WINDOW`.
    tool_exec_seq: Arc<AtomicU64>,
    sprout: SproutSupervisor,
    cancel_flag: Option<Arc<AtomicBool>>,
    #[cfg(feature = "scent")]
    scent_index: Arc<Mutex<onyx_scent::WorkspaceIndex>>,
    #[cfg(feature = "scent")]
    seen_outlines: SeenOutlinesMap,
    #[cfg(feature = "mcp")]
    mcp: Option<Arc<crate::mcp::McpRegistry>>,
}

/// Indica si el nombre de herramienta corresponde al protocolo de subagentes asíncronos (`SDD-09`).
pub fn is_async_subagent_tool(tool: &str) -> bool {
    matches!(
        tool,
        "spawn_subagent" | "subagent_status" | "subagent_merge" | "subagent_discard"
    )
}

/// Extrae el campo `driver` (opcional) de los argumentos JSON de `spawn_subagent` (`SDD-10` Fase 1).
/// Devuelve `None` si el JSON no parsea, no hay campo `driver`, o el valor no es un string no vacío.
pub fn spawn_subagent_driver_from_args(args_json: &str) -> Option<String> {
    let raw = if args_json.trim().is_empty() {
        "{}"
    } else {
        args_json
    };
    let value: JsonValue = serde_json::from_str(raw).ok()?;
    value
        .get("driver")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|d| !d.is_empty())
        .map(str::to_string)
}

/// Indica si `spawn_agent` (`SDD-05` REQ-05.5) debe ejecutarse en un *worktree* Git
/// aislado: solo `true` si el campo `isolated` es exactamente el booleano JSON `true`.
/// Argumentos vacíos o JSON inválido → `false` (comportamiento clásico, sin *worktree*).
pub fn spawn_agent_is_isolated(args_json: &str) -> bool {
    let raw = if args_json.trim().is_empty() {
        "{}"
    } else {
        args_json
    };
    match serde_json::from_str::<JsonValue>(raw) {
        Ok(value) => value
            .get("isolated")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        Err(_) => false,
    }
}

/// `SDD-07` Fase 2: resultado de una ejecución shell verificada por el usuario
/// en el borde de sesión (`! <cmd>`): pipes ya `scrub_secrets`, cap
/// `policy.max_output_bytes`, y `timed_out` en vez de `Err` para el timeout
/// (el resultado sigue siendo sellable, no un fallo de transporte).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetShellExecResult {
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub stdout: String,
    pub stderr: String,
}

impl WorkspaceTools {
    pub fn new(policy: WorkspacePolicy) -> Self {
        let sprout = SproutSupervisor::new(policy.clone());
        Self {
            policy,
            seen_reads: Arc::new(Mutex::new(BTreeMap::new())),
            read_seq: Arc::new(AtomicU64::new(1)),
            tool_exec_seq: Arc::new(AtomicU64::new(1)),
            sprout,
            cancel_flag: None,
            #[cfg(feature = "scent")]
            scent_index: Arc::new(Mutex::new(onyx_scent::WorkspaceIndex::new())),
            #[cfg(feature = "scent")]
            seen_outlines: Arc::new(Mutex::new(BTreeMap::new())),
            #[cfg(feature = "mcp")]
            mcp: None,
        }
    }

    /// Adjunta el registro de servidores MCP (`SDD-04 REQ-04.3`/`REQ-04.4`): los schemas
    /// `mcp__*` se sirven vía [`Self::all_tool_schemas`] y el despacho ocurre en
    /// [`Self::execute_with_tick`] antes del comodín `unknown tool`.
    #[cfg(feature = "mcp")]
    pub fn with_mcp_registry(mut self, registry: Arc<crate::mcp::McpRegistry>) -> Self {
        self.mcp = Some(registry);
        self
    }

    /// Schemas estáticos + dinámicos de MCP (`mcp__<server>__<tool>`, siempre con
    /// `requires_approval = true`).
    #[cfg(feature = "mcp")]
    pub fn all_tool_schemas(
        &self,
        include_subagents: bool,
        driver_ids: &[String],
    ) -> Vec<ToolSchema> {
        let mut out = Self::schemas_with_subagents_and_drivers(include_subagents, driver_ids);
        if let Some(registry) = &self.mcp {
            out.extend(registry.tool_schemas());
        }
        out
    }

    pub fn with_cancel_flag(mut self, cancel_flag: Arc<AtomicBool>) -> Self {
        self.cancel_flag = Some(cancel_flag);
        self
    }

    /// Bandera de cancelación compartida (p. ej. para propagarla a *worktrees* aislados
    /// de hijos de `spawn_agent(isolated: true)`); `None` si no hay cancelación activa.
    pub fn cancel_flag(&self) -> Option<Arc<AtomicBool>> {
        self.cancel_flag.clone()
    }

    fn is_cancelled(&self) -> bool {
        self.cancel_flag
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::Acquire))
    }

    pub fn with_sprout(mut self, sprout: SproutSupervisor) -> Self {
        self.sprout = sprout;
        self
    }

    pub fn sprout(&self) -> &SproutSupervisor {
        &self.sprout
    }

    pub fn policy(&self) -> &WorkspacePolicy {
        &self.policy
    }

    /// Evalúa en best-effort el impacto AST de una mutación `fs_edit`/`fs_write` (`REQ-MIX1.2`).
    pub fn assess_mutation_impact(&self, tool: &str, args_json: &str) -> Option<ImpactAssessment> {
        #[cfg(feature = "scent")]
        {
            evaluate_mutation_impact(&self.policy, tool, args_json, &self.scent_index)
        }
        #[cfg(not(feature = "scent"))]
        {
            evaluate_mutation_impact(&self.policy, tool, args_json)
        }
    }

    /// Evalúa en best-effort el impacto AST (`onyx-scent`) de un diff unificado completo
    /// (p. ej. el resultado de un merge 3-way de subagente en `subagent_merge`).
    pub fn assess_unified_diff_impact(&self, unified_diff: &str) -> Option<ImpactAssessment> {
        #[cfg(feature = "scent")]
        {
            evaluate_unified_diff_impact(&self.policy, unified_diff, &self.scent_index)
        }
        #[cfg(not(feature = "scent"))]
        {
            evaluate_unified_diff_impact(&self.policy, unified_diff)
        }
    }

    /// Evalúa `WorkspaceMode` + `PermissionRules` estáticas + escalado monótono *Scent-Heated Approval*
    /// (`Allow → Ask`, nunca debilitando `Deny` ni `Ask`).
    ///
    /// `SDD-10`: `spawn_subagent` con un driver externo no-nativo se decide por el gate
    /// [`WorkspacePolicy::external_driver_gate`] (trust + regla `allow`/`deny`/`ask` por driver),
    /// sin aplicar ni el escalado de impacto ni el fallback estático de no-mutantes; y en
    /// modo `Plan`/`Explore` (`!mode.allows_mutation()`) queda **siempre** `Deny`, porque un
    /// harness externo ejecuta procesos y red fuera del control de `WorkspaceToolBridge`.
    pub fn evaluate_permission_with_impact(
        &self,
        mode: WorkspaceMode,
        tool: &str,
        args_json: &str,
    ) -> (PermissionDecision, Option<ImpactAssessment>) {
        if tool == "spawn_subagent" {
            if let Some(driver) = spawn_subagent_driver_from_args(args_json) {
                if !driver.eq_ignore_ascii_case("native") {
                    if !mode.allows_mutation() {
                        return (PermissionDecision::Deny, None);
                    }
                    let (decision, _reason) = self.policy.external_driver_gate(&driver);
                    return (decision, None);
                }
            }
        }
        let static_decision = self
            .policy
            .evaluate_permission_in_mode(mode, tool, args_json);
        if static_decision == PermissionDecision::Deny {
            return (PermissionDecision::Deny, None);
        }
        let assessment = self.assess_mutation_impact(tool, args_json);
        let final_decision = apply_impact_escalation(static_decision, assessment.as_ref());
        (final_decision, assessment)
    }

    pub fn all_tool_names() -> &'static [&'static str] {
        #[cfg(feature = "scent")]
        {
            &[
                "fs_read",
                "fs_list",
                "fs_write",
                "fs_edit",
                "code_search",
                "glob_files",
                "git_status",
                "git_diff",
                "load_skill",
                "plan_update",
                "shell_exec",
                "service_status",
                "service_start",
                "service_stop",
                "service_logs",
                "service_exec",
                "scent_outline",
                "scent_impact",
                "scent_references",
            ]
        }
        #[cfg(not(feature = "scent"))]
        {
            &[
                "fs_read",
                "fs_list",
                "fs_write",
                "fs_edit",
                "code_search",
                "glob_files",
                "git_status",
                "git_diff",
                "load_skill",
                "plan_update",
                "shell_exec",
                "service_status",
                "service_start",
                "service_stop",
                "service_logs",
                "service_exec",
            ]
        }
    }

    pub fn all_tool_names_with_subagents() -> &'static [&'static str] {
        #[cfg(feature = "scent")]
        {
            &[
                "fs_read",
                "fs_list",
                "fs_write",
                "fs_edit",
                "code_search",
                "glob_files",
                "git_status",
                "git_diff",
                "load_skill",
                "plan_update",
                "shell_exec",
                "service_status",
                "service_start",
                "service_stop",
                "service_logs",
                "service_exec",
                "scent_outline",
                "scent_impact",
                "scent_references",
                "spawn_agent",
                "spawn_subagent",
                "subagent_status",
                "subagent_merge",
                "subagent_discard",
            ]
        }
        #[cfg(not(feature = "scent"))]
        {
            &[
                "fs_read",
                "fs_list",
                "fs_write",
                "fs_edit",
                "code_search",
                "glob_files",
                "git_status",
                "git_diff",
                "load_skill",
                "plan_update",
                "shell_exec",
                "service_status",
                "service_start",
                "service_stop",
                "service_logs",
                "service_exec",
                "spawn_agent",
                "spawn_subagent",
                "subagent_status",
                "subagent_merge",
                "subagent_discard",
            ]
        }
    }

    pub fn async_subagent_tool_names() -> &'static [&'static str] {
        &[
            "spawn_subagent",
            "subagent_status",
            "subagent_merge",
            "subagent_discard",
        ]
    }

    pub fn approval_required_names() -> &'static [&'static str] {
        &[
            "fs_write",
            "fs_edit",
            "shell_exec",
            "service_start",
            "service_stop",
            "service_exec",
            "subagent_merge",
        ]
    }

    pub fn subagent_schema() -> ToolSchema {
        ToolSchema {
            name: "spawn_agent".into(),
            description: "Delegate a scoped subtask to a child subagent and suspend until the child completes.".into(),
            parameters: json!({
                "type": "OBJECT",
                "properties": {
                    "name": { "type": "STRING", "description": "Optional short label for the subagent." },
                    "goal": { "type": "STRING", "description": "Clear, bounded goal for the child subagent." },
                    "isolated": { "type": "BOOLEAN", "description": "When true, run the child subagent inside an isolated Git worktree so parallel edits never touch the root working tree until merged (default false)." }
                },
                "required": ["goal"]
            }),
            requires_approval: false,
        }
    }

    pub fn async_subagent_schemas() -> Vec<ToolSchema> {
        Self::async_subagent_schemas_with_drivers(&[])
    }

    /// Variantes de `async_subagent_schemas` con la lista dinámica de drivers de harness CLI
    /// externos habilitados (`SDD-10` Fase 1): el parámetro opcional `driver` de
    /// `spawn_subagent` describe `native` (default) y los ids configurados.
    pub fn async_subagent_schemas_with_drivers(driver_ids: &[String]) -> Vec<ToolSchema> {
        let available = driver_ids.join(", ");
        let driver_desc = if available.is_empty() {
            "Optional execution backend for the subagent. `native` (default) runs the in-process OnYX engine; an external CLI harness id (e.g. `opencode`) delegates the goal to that external tool inside the same isolated Git worktree and requires explicit approval or an `allow spawn_subagent(<driver>)` rule.".to_string()
        } else {
            format!(
                "Optional execution backend for the subagent. `native` (default) runs the in-process OnYX engine; an external CLI harness id (available: {available}) delegates the goal to that external tool inside the same isolated Git worktree and requires explicit approval or an `allow spawn_subagent(<driver>)` rule."
            )
        };
        vec![
            ToolSchema {
                name: "spawn_subagent".into(),
                description: "Launch an asynchronous subagent in an isolated Git worktree without blocking the root agent. Returns `job_id` immediately.".into(),
                parameters: json!({
                    "type": "OBJECT",
                    "properties": {
                        "name": { "type": "STRING", "description": "Optional short label for the background subagent." },
                        "goal": { "type": "STRING", "description": "Clear, bounded goal to execute inside the isolated worktree." },
                        "max_steps": { "type": "INTEGER", "description": "Optional max reasoning steps for the subagent (default 32, max 128)." },
                        "driver": { "type": "STRING", "description": driver_desc }
                    },
                    "required": ["goal"]
                }),
                requires_approval: false,
            },
            ToolSchema {
                name: "subagent_status".into(),
                description: "Inspect the status, touched files, and summary of one (`job_id`) or all asynchronous worktree subagents.".into(),
                parameters: json!({
                    "type": "OBJECT",
                    "properties": {
                        "job_id": { "type": "STRING", "description": "Optional subagent job_id to inspect; omit to list all jobs." }
                    }
                }),
                requires_approval: false,
            },
            ToolSchema {
                name: "subagent_merge".into(),
                description: "Perform a 3-way Git merge-tree reconciliation of a finished async subagent's worktree into the root working tree without moving HEAD, then clean up the worktree and temporary branch.".into(),
                parameters: json!({
                    "type": "OBJECT",
                    "properties": {
                        "job_id": { "type": "STRING", "description": "Finished subagent job_id to reconcile and merge." }
                    },
                    "required": ["job_id"]
                }),
                requires_approval: true,
            },
            ToolSchema {
                name: "subagent_discard".into(),
                description: "Cancel (if running) and permanently discard an async subagent's worktree and temporary branch, terminating any child processes cleanly first.".into(),
                parameters: json!({
                    "type": "OBJECT",
                    "properties": {
                        "job_id": { "type": "STRING", "description": "Subagent job_id to cancel and discard." }
                    },
                    "required": ["job_id"]
                }),
                requires_approval: false,
            },
        ]
    }

    pub fn schemas_with_subagents(include_subagents: bool) -> Vec<ToolSchema> {
        Self::schemas_with_subagents_and_drivers(include_subagents, &[])
    }

    /// Schemas completos con la lista dinámica de drivers externos habilitados (`SDD-10` Fase 1).
    pub fn schemas_with_subagents_and_drivers(
        include_subagents: bool,
        driver_ids: &[String],
    ) -> Vec<ToolSchema> {
        let mut out = Self::schemas();
        if include_subagents {
            out.push(Self::subagent_schema());
            out.extend(Self::async_subagent_schemas_with_drivers(driver_ids));
        }
        out
    }

    pub fn schemas() -> Vec<ToolSchema> {
        #[cfg_attr(not(feature = "scent"), allow(unused_mut))]
        let mut out = vec![
            ToolSchema {
                name: "fs_read".into(),
                description: "Read a UTF-8 file from the workspace, optionally by 1-based inclusive line range.".into(),
                parameters: json!({
                    "type": "OBJECT",
                    "properties": {
                        "path": { "type": "STRING", "description": "Relative path inside the workspace." },
                        "start_line": { "type": "INTEGER", "description": "Optional 1-based start line." },
                        "end_line": { "type": "INTEGER", "description": "Optional 1-based inclusive end line." }
                    },
                    "required": ["path"]
                }),
                requires_approval: false,
            },
            ToolSchema {
                name: "fs_list".into(),
                description: "List files and directories inside the workspace, skipping ignored directories.".into(),
                parameters: json!({
                    "type": "OBJECT",
                    "properties": {
                        "path": { "type": "STRING", "description": "Optional relative directory path (defaults to '.')." },
                        "max_depth": { "type": "INTEGER", "description": "Optional maximum traversal depth (default 3)." }
                    }
                }),
                requires_approval: false,
            },
            ToolSchema {
                name: "fs_write".into(),
                description: "Create or overwrite a UTF-8 file in the workspace.".into(),
                parameters: json!({
                    "type": "OBJECT",
                    "properties": {
                        "path": { "type": "STRING", "description": "Relative path inside the workspace." },
                        "content": { "type": "STRING", "description": "Full file contents to write." }
                    },
                    "required": ["path", "content"]
                }),
                requires_approval: true,
            },
            ToolSchema {
                name: "fs_edit".into(),
                description: "Replace one unique substring (`old_text`/`new_text`) or multiple sequential hunks atomically (`edits`) in an existing file.".into(),
                parameters: json!({
                    "type": "OBJECT",
                    "properties": {
                        "path": { "type": "STRING", "description": "Relative path inside the workspace." },
                        "old_text": { "type": "STRING", "description": "Exact text to replace (single-hunk mode)." },
                        "new_text": { "type": "STRING", "description": "Replacement text (single-hunk mode)." },
                        "edits": {
                            "type": "ARRAY",
                            "description": "Optional list of sequential `{ old_text, new_text }` hunks applied atomically in order.",
                            "items": {
                                "type": "OBJECT",
                                "properties": {
                                    "old_text": { "type": "STRING", "description": "Exact text to replace." },
                                    "new_text": { "type": "STRING", "description": "Replacement text." }
                                },
                                "required": ["old_text", "new_text"]
                            }
                        }
                    },
                    "required": ["path"]
                }),
                requires_approval: true,
            },
            ToolSchema {
                name: "code_search".into(),
                description: "Search for a literal or regex pattern across workspace files with optional glob filters, case-insensitivity, and context lines.".into(),
                parameters: json!({
                    "type": "OBJECT",
                    "properties": {
                        "pattern": { "type": "STRING", "description": "Substring or regular expression to search for." },
                        "path": { "type": "STRING", "description": "Optional relative directory or file (defaults to '.')." },
                        "max_results": { "type": "INTEGER", "description": "Maximum matching lines to return (default 50, max 200)." },
                        "regex": { "type": "BOOLEAN", "description": "Interpret `pattern` as a regular expression (default false)." },
                        "case_insensitive": { "type": "BOOLEAN", "description": "Perform case-insensitive matching (default false)." },
                        "include_glob": { "type": "STRING", "description": "Optional glob filter to include matching relative file paths (e.g. `*.rs` or `crates/**/*.rs`)." },
                        "exclude_glob": { "type": "STRING", "description": "Optional glob filter to exclude matching relative file paths." },
                        "context_lines": { "type": "INTEGER", "description": "Optional context lines before and after each match (0..=5, default 0)." }
                    },
                    "required": ["pattern"]
                }),
                requires_approval: false,
            },
            ToolSchema {
                name: "glob_files".into(),
                description: "Find workspace files matching a glob pattern (e.g. `crates/**/*.rs` or `**/*SKILL.md`), skipping ignored directories without requiring approval.".into(),
                parameters: json!({
                    "type": "OBJECT",
                    "properties": {
                        "pattern": { "type": "STRING", "description": "Glob pattern to match relative file paths." },
                        "path": { "type": "STRING", "description": "Optional relative root directory to search under (defaults to '.')." },
                        "max_results": { "type": "INTEGER", "description": "Maximum file paths to return (default 100, max 500)." },
                        "sort_by_mtime": { "type": "BOOLEAN", "description": "Sort by most recently modified first instead of deterministic lexicographical order (default false)." }
                    },
                    "required": ["pattern"]
                }),
                requires_approval: false,
            },
            ToolSchema {
                name: "git_status".into(),
                description: "Show concise repository status (`git status --short --branch`) without requiring approval.".into(),
                parameters: json!({
                    "type": "OBJECT",
                    "properties": {}
                }),
                requires_approval: false,
            },
            ToolSchema {
                name: "git_diff".into(),
                description: "Inspect unified git diff (`git diff --no-color`) for the working tree or a relative path without requiring approval.".into(),
                parameters: json!({
                    "type": "OBJECT",
                    "properties": {
                        "path": { "type": "STRING", "description": "Optional relative file or directory path inside the workspace." },
                        "staged": { "type": "BOOLEAN", "description": "Optional flag to inspect staged changes (`--cached`)." },
                        "context_lines": { "type": "INTEGER", "description": "Optional unified context lines (default 3, max 20)." }
                    }
                }),
                requires_approval: false,
            },
            ToolSchema {
                name: "load_skill".into(),
                description: "Load an on-demand protocol or stack guide (e.g. `scent`, `rust`, or workspace SKILL.md) without requiring approval.".into(),
                parameters: json!({
                    "type": "OBJECT",
                    "properties": {
                        "name": { "type": "STRING", "description": "Skill name to load." }
                    },
                    "required": ["name"]
                }),
                requires_approval: false,
            },
            ToolSchema {
                name: "plan_update".into(),
                description: "Publish or update a structured multi-step progress checklist (`[x]`, `[>]`, `[ ]`) rendered in the TUI without requiring approval. Never call `plan_update` multiple times in a row without executing real workspace tools in between; for conceptual questions or answers that do not inspect or modify the workspace, omit `plan_update` or call it at most once.".into(),
                parameters: json!({
                    "type": "OBJECT",
                    "properties": {
                        "explanation": { "type": "STRING", "description": "Optional short summary of the current plan." },
                        "steps": {
                            "type": "ARRAY",
                            "description": "Ordered plan items.",
                            "items": {
                                "type": "OBJECT",
                                "properties": {
                                    "step": { "type": "STRING", "description": "Short task description." },
                                    "status": { "type": "STRING", "description": "Status: `pending`, `in_progress`, or `completed`." }
                                },
                                "required": ["step", "status"]
                            }
                        }
                    },
                    "required": ["steps"]
                }),
                requires_approval: false,
            },
            ToolSchema {
                name: "shell_exec".into(),
                description: "Execute a shell command in the workspace root directory.".into(),
                parameters: json!({
                    "type": "OBJECT",
                    "properties": {
                        "command": { "type": "STRING", "description": "Shell command to execute." }
                    },
                    "required": ["command"]
                }),
                requires_approval: true,
            },
            ToolSchema {
                name: "service_status".into(),
                description: "Return a compact @TOON v1 status table of all local processes and containers declared in `sprout.toml` without requiring approval.".into(),
                parameters: json!({
                    "type": "OBJECT",
                    "properties": {}
                }),
                requires_approval: false,
            },
            ToolSchema {
                name: "service_start".into(),
                description: "Start a local process or container service (`id`) or all enabled services (`all: true`) from `sprout.toml` in topological DAG order, waiting for readiness probes (`ready_port`, `ready_log`, `ready_cmd`).".into(),
                parameters: json!({
                    "type": "OBJECT",
                    "properties": {
                        "id": { "type": "STRING", "description": "Optional service id from `sprout.toml` (or `\"all\"`)." },
                        "all": { "type": "BOOLEAN", "description": "Start all enabled services in topological order." },
                        "wait_ready_secs": { "type": "INTEGER", "description": "Optional readiness wait timeout in seconds (1..=300, default 30)." }
                    }
                }),
                requires_approval: true,
            },
            ToolSchema {
                name: "service_stop".into(),
                description: "Stop one service (`id`) or all active services (`all: true`) from `sprout.toml`, running `stop_cmd`/`down` and terminating their process trees cleanly.".into(),
                parameters: json!({
                    "type": "OBJECT",
                    "properties": {
                        "id": { "type": "STRING", "description": "Optional service id to stop." },
                        "all": { "type": "BOOLEAN", "description": "Stop all active services in reverse topological order." }
                    }
                }),
                requires_approval: true,
            },
            ToolSchema {
                name: "service_logs".into(),
                description: "Read recent lines from a service's in-memory log ring-buffer (optionally filtered by literal text or regex, scrubbed for secrets) without requiring approval.".into(),
                parameters: json!({
                    "type": "OBJECT",
                    "properties": {
                        "id": { "type": "STRING", "description": "Service id from `sprout.toml`." },
                        "tail": { "type": "INTEGER", "description": "Optional number of trailing lines to return (1..=500, default 80)." },
                        "pattern": { "type": "STRING", "description": "Optional substring or regex filter." },
                        "regex": { "type": "BOOLEAN", "description": "Interpret `pattern` as a regular expression (default false)." }
                    },
                    "required": ["id"]
                }),
                requires_approval: false,
            },
            ToolSchema {
                name: "service_exec".into(),
                description: "Execute a command inside a `kind = \"container\"` service via `docker exec` / `nerdctl exec` with sanitized environment, bounded timeout, and secret scrubbing.".into(),
                parameters: json!({
                    "type": "OBJECT",
                    "properties": {
                        "id": { "type": "STRING", "description": "Container service id from `sprout.toml`." },
                        "command": { "type": "STRING", "description": "Shell command to run inside the container." },
                        "timeout_secs": { "type": "INTEGER", "description": "Optional command timeout in seconds (1..=300, default 30)." }
                    },
                    "required": ["id", "command"]
                }),
                requires_approval: true,
            },
        ];
        #[cfg(feature = "scent")]
        {
            out.push(ToolSchema {
                name: "scent_outline".into(),
                description: "Return compact @TOON v1 AST outlines (public signatures and 1-based line ranges) for Rust, Python, Java, TypeScript, JavaScript, Go, or C/C++ files.".into(),
                parameters: json!({
                    "type": "OBJECT",
                    "properties": {
                        "path": { "type": "STRING", "description": "Optional relative file or directory inside the workspace (defaults to '.')." },
                        "public_only": { "type": "BOOLEAN", "description": "Optional filter to emit only public symbols." },
                        "exclude_tests": { "type": "BOOLEAN", "description": "Optional filter to omit test modules/functions (scope:test)." },
                        "cursor": { "type": "INTEGER", "description": "Optional 0-based row cursor for pagination." },
                        "max_rows": { "type": "INTEGER", "description": "Optional maximum TOON rows per page (1..=500, default 250)." }
                    }
                }),
                requires_approval: false,
            });
            out.push(ToolSchema {
                name: "scent_impact".into(),
                description: "Trace semantic AST call impact around a target symbol as ranked @TOON v1 tuples (epicenter, callers, callees, line ranges).".into(),
                parameters: json!({
                    "type": "OBJECT",
                    "properties": {
                        "target": { "type": "STRING", "description": "Symbol name, Type::method, or file::symbol to analyze." },
                        "max_hops": { "type": "INTEGER", "description": "Optional diffusion hop radius (1..=6, default 3)." },
                        "public_only": { "type": "BOOLEAN", "description": "Optional filter to emit only public neighbor symbols." },
                        "exclude_tests": { "type": "BOOLEAN", "description": "Optional filter to omit test neighbors (scope:test)." },
                        "cursor": { "type": "INTEGER", "description": "Optional 0-based row cursor for pagination." },
                        "max_rows": { "type": "INTEGER", "description": "Optional maximum TOON rows per page (1..=500, default 250)." }
                    },
                    "required": ["target"]
                }),
                requires_approval: false,
            });
            out.push(ToolSchema {
                name: "scent_references".into(),
                description: "Resolve exact AST definitions and direct cross-references (callers, callees, implementors, type users) for a symbol as @TOON v1 tuples.".into(),
                parameters: json!({
                    "type": "OBJECT",
                    "properties": {
                        "target": { "type": "STRING", "description": "Symbol name, Type::method, or file::symbol to resolve." },
                        "public_only": { "type": "BOOLEAN", "description": "Optional filter to emit only public referencing symbols." },
                        "exclude_tests": { "type": "BOOLEAN", "description": "Optional filter to omit test references (scope:test)." },
                        "cursor": { "type": "INTEGER", "description": "Optional 0-based row cursor for pagination." },
                        "max_rows": { "type": "INTEGER", "description": "Optional maximum TOON rows per page (1..=500, default 250)." }
                    },
                    "required": ["target"]
                }),
                requires_approval: false,
            });
        }
        out
    }

    pub fn execute(&self, tool: &str, args_json: &str) -> Result<String, String> {
        self.execute_with_tick(tool, args_json, None)
    }

    pub fn execute_with_tick(
        &self,
        tool: &str,
        args_json: &str,
        tick_hint: Option<u64>,
    ) -> Result<String, String> {
        // `SDD-04` 4C-audit: secuencia global de ejecuciones de tool; ancora
        // la ventana de deduplicación de `fs_read` (`FS_READ_DEDUP_WINDOW`).
        self.tool_exec_seq.fetch_add(1, Ordering::Relaxed);
        let raw = if args_json.trim().is_empty() {
            "{}"
        } else {
            args_json
        };
        match tool {
            "fs_read" => {
                let args: FsReadArgs =
                    serde_json::from_str(raw).map_err(|e| format!("invalid fs_read args: {e}"))?;
                self.fs_read(args, tick_hint)
            }
            "fs_list" => {
                let args: FsListArgs =
                    serde_json::from_str(raw).map_err(|e| format!("invalid fs_list args: {e}"))?;
                self.fs_list(args)
            }
            "fs_write" => {
                let args: FsWriteArgs =
                    serde_json::from_str(raw).map_err(|e| format!("invalid fs_write args: {e}"))?;
                self.fs_write(args)
            }
            "fs_edit" => {
                let args: FsEditArgs =
                    serde_json::from_str(raw).map_err(|e| format!("invalid fs_edit args: {e}"))?;
                self.fs_edit(args)
            }
            "code_search" => {
                let args: CodeSearchArgs = serde_json::from_str(raw)
                    .map_err(|e| format!("invalid code_search args: {e}"))?;
                self.code_search(args)
            }
            "glob_files" => {
                let args: GlobFilesArgs = serde_json::from_str(raw)
                    .map_err(|e| format!("invalid glob_files args: {e}"))?;
                self.glob_files(args)
            }
            "git_status" => self.git_status(),
            "git_diff" => {
                let args: GitDiffArgs =
                    serde_json::from_str(raw).map_err(|e| format!("invalid git_diff args: {e}"))?;
                self.git_diff(args)
            }
            "load_skill" => {
                let args: LoadSkillArgs = serde_json::from_str(raw)
                    .map_err(|e| format!("invalid load_skill args: {e}"))?;
                self.load_skill(args)
            }
            "plan_update" => self.plan_update(raw),
            "shell_exec" => {
                let args: ShellExecArgs = serde_json::from_str(raw)
                    .map_err(|e| format!("invalid shell_exec args: {e}"))?;
                self.shell_exec(args)
            }
            "service_status" => self.sprout.status_toon(),
            "service_start" => {
                let args: ServiceStartArgs = serde_json::from_str(raw)
                    .map_err(|e| format!("invalid service_start args: {e}"))?;
                if args.all
                    || args
                        .id
                        .as_deref()
                        .is_some_and(|s| s.trim().eq_ignore_ascii_case("all"))
                {
                    self.sprout.start_all(args.wait_ready_secs)
                } else if let Some(id) = args.id.as_deref().map(str::trim).filter(|s| !s.is_empty())
                {
                    self.sprout.start(id, args.wait_ready_secs)
                } else {
                    Err("service_start requires `id` or `all: true`".to_string())
                }
            }
            "service_stop" => {
                let args: ServiceStopArgs = serde_json::from_str(raw)
                    .map_err(|e| format!("invalid service_stop args: {e}"))?;
                self.sprout.stop(args.id.as_deref(), args.all)
            }
            "service_logs" => {
                let args: ServiceLogsArgs = serde_json::from_str(raw)
                    .map_err(|e| format!("invalid service_logs args: {e}"))?;
                self.sprout
                    .logs(&args.id, args.tail, args.pattern.as_deref(), args.regex)
            }
            "service_exec" => {
                let args: ServiceExecArgs = serde_json::from_str(raw)
                    .map_err(|e| format!("invalid service_exec args: {e}"))?;
                self.sprout
                    .exec_in_container(&args.id, &args.command, args.timeout_secs)
            }
            #[cfg(feature = "scent")]
            "scent_outline" => {
                let args: ScentOutlineArgs = serde_json::from_str(raw)
                    .map_err(|e| format!("invalid scent_outline args: {e}"))?;
                self.scent_outline(args)
            }
            #[cfg(feature = "scent")]
            "scent_impact" => {
                let args: ScentImpactArgs = serde_json::from_str(raw)
                    .map_err(|e| format!("invalid scent_impact args: {e}"))?;
                self.scent_impact(args)
            }
            #[cfg(feature = "scent")]
            "scent_references" => {
                let args: ScentReferencesArgs = serde_json::from_str(raw)
                    .map_err(|e| format!("invalid scent_references args: {e}"))?;
                self.scent_references(args)
            }
            // `SDD-04 REQ-04.3`: herramientas MCP (`mcp__<server>__<tool>`), despachadas
            // antes del comodín `unknown tool`; la salida ya pasó por `truncate_utf8` +
            // `scrub_secrets` dentro de `McpRegistry::call`.
            #[cfg(feature = "mcp")]
            _ if tool.starts_with("mcp__") => {
                let registry = self.mcp.as_ref().ok_or_else(|| {
                    format!("MCP tool `{tool}` requested but no MCP registry is attached (SDD-04)")
                })?;
                let args: JsonValue = serde_json::from_str(raw)
                    .map_err(|e| format!("invalid MCP tool args for `{tool}`: {e}"))?;
                registry.call(tool, &args).map_err(|e| e.to_string())
            }
            _ => Err(format!("unknown workspace tool `{tool}`")),
        }
    }

    fn fs_read(&self, args: FsReadArgs, tick_hint: Option<u64>) -> Result<String, String> {
        let path = self.policy.resolve_existing(&args.path)?;
        let rel = path
            .strip_prefix(&self.policy.root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        let raw_bytes =
            fs::read(&path).map_err(|e| format!("failed to read `{}`: {e}", args.path))?;
        if raw_bytes.starts_with(b"ONYXTR2") || raw_bytes.starts_with(b"ONYXTR1") {
            return Err(format!(
                "`{}` is a binary OnYX transcript file; inspect it with `openonyx replay {}` or `openonyx doctor --transcript {}`.",
                args.path, args.path, args.path
            ));
        }
        if raw_bytes.starts_with(b"ONYXPK1") {
            return Err(format!(
                "`{}` is a binary OnYX snapshot pack; inspect it with `openonyx inspect <dir> --list`.",
                args.path
            ));
        }
        if raw_bytes.starts_with(b"ONYXAG") {
            return Err(format!(
                "`{}` is a binary OnYX snapshot file; inspect it with `openonyx inspect <dir> --tick <n>`.",
                args.path
            ));
        }
        let content = String::from_utf8(raw_bytes)
            .map_err(|e| format!("failed to read `{}` as UTF-8 text: {e}", args.path))?;
        let digest = fnv1a_hex(content.as_bytes());
        let lines: Vec<&str> = content.lines().collect();
        let total = lines.len();
        let start = args.start_line.unwrap_or(1).max(1);
        let end = args.end_line.unwrap_or(total).min(total);
        if start > total && total > 0 {
            return Ok(format!(
                "(file has {total} lines; start_line {start} is past EOF)"
            ));
        }

        let key = (rel.clone(), start, end);
        // `SDD-04` 4C-audit: el stub `@TOON` solo es válido cuando el digest
        // coincide, la lectura está dentro de la ventana de deduplicación
        // (menos de `FS_READ_DEDUP_WINDOW` ejecuciones de tool desde la
        // última lectura con contenido) y la lectura previa no era ella
        // misma un stub. Expirada o ya-stub → contenido completo y
        // re-ancraje (`stubbed = false`).
        let now_seq = self.tool_exec_seq.load(Ordering::Relaxed);
        let stub_reply = if let Ok(mut guard) = self.seen_reads.lock() {
            let mut reply: Option<String> = None;
            if let Some(prev) = guard.get_mut(&key) {
                if prev.digest == digest
                    && now_seq.saturating_sub(prev.exec_seq) < FS_READ_DEDUP_WINDOW
                    && !prev.stubbed
                {
                    prev.stubbed = true;
                    let tick_label = tick_hint.unwrap_or(prev.tick);
                    reply = Some(format!(
                        "@TOON v1\n# cols: subject | predicate | object | meta\n{rel} | unchanged_since_tick:{tick_label} | lines:{start}-{end} | digest:{digest}\n# note: already read and unchanged; do not re-read unless edited\n# total: 1\n"
                    ));
                } else {
                    let tick_val =
                        tick_hint.unwrap_or_else(|| self.read_seq.fetch_add(1, Ordering::Relaxed));
                    prev.digest = digest.clone();
                    prev.tick = tick_val;
                    prev.exec_seq = now_seq;
                    prev.stubbed = false;
                }
            } else if let Some(((_, prev_s, prev_e), prev)) =
                guard.iter_mut().find(|((p, ps, pe), prev)| {
                    p == &rel
                        && !(*ps == start && *pe == end) // la clave exacta la gestiona la rama 1
                        && prev.digest == digest
                        && now_seq.saturating_sub(prev.exec_seq) < FS_READ_DEDUP_WINDOW
                        && !prev.stubbed
                        && *ps <= start
                        && end <= *pe
                })
            {
                prev.stubbed = true;
                let tick_label = tick_hint.unwrap_or(prev.tick);
                reply = Some(format!(
                    "@TOON v1\n# cols: subject | predicate | object | meta\n{rel} | unchanged_since_tick:{tick_label} | lines:{start}-{end} | covered_by:{prev_s}-{prev_e} digest:{digest}\n# note: range already covered by prior read ({prev_s}-{prev_e}); do not re-read unless edited\n# total: 1\n"
                ));
            } else {
                let tick_val =
                    tick_hint.unwrap_or_else(|| self.read_seq.fetch_add(1, Ordering::Relaxed));
                guard.insert(
                    key,
                    SeenRead {
                        digest: digest.clone(),
                        tick: tick_val,
                        exec_seq: now_seq,
                        stubbed: false,
                    },
                );
            }
            reply
        } else {
            None
        };
        if let Some(reply) = stub_reply {
            return Ok(reply);
        }

        let mut out = String::new();
        for (idx, line) in lines
            .iter()
            .enumerate()
            .skip(start.saturating_sub(1))
            .take(end.saturating_sub(start.saturating_sub(1)))
        {
            out.push_str(&format!("{}: {line}\n", idx + 1));
            if out.len() >= self.policy.max_read_bytes {
                out.push_str("\n...[truncated by max_read_bytes]");
                break;
            }
        }
        Ok(out)
    }

    pub fn clear_seen_reads(&self) {
        if let Ok(mut guard) = self.seen_reads.lock() {
            guard.clear();
        }
        #[cfg(feature = "scent")]
        if let Ok(mut guard) = self.seen_outlines.lock() {
            guard.clear();
        }
    }

    fn invalidate_seen_file(&self, canonical_path: &Path) {
        let rel = canonical_path
            .strip_prefix(&self.policy.root)
            .unwrap_or(canonical_path)
            .to_string_lossy()
            .replace('\\', "/");
        if let Ok(mut guard) = self.seen_reads.lock() {
            guard.retain(|(p, _, _), _| p != &rel);
        }
        #[cfg(feature = "scent")]
        if let Ok(mut guard) = self.seen_outlines.lock() {
            // Any file edit can invalidate both file-level and directory-level outlines.
            guard.clear();
        }
    }

    fn fs_list(&self, args: FsListArgs) -> Result<String, String> {
        let base = args.path.as_deref().unwrap_or(".");
        let start = self.policy.resolve_existing(base)?;
        let max_depth = args.max_depth.unwrap_or(3).min(10);
        let mut entries = Vec::new();
        self.walk_list(&start, 0, max_depth, &mut entries);
        entries.sort();
        Ok(truncate_utf8(
            &entries.join("\n"),
            self.policy.max_output_bytes,
        ))
    }

    fn walk_list(&self, dir: &Path, depth: usize, max_depth: usize, out: &mut Vec<String>) {
        if depth > max_depth {
            return;
        }
        let Ok(read_dir) = fs::read_dir(dir) else {
            return;
        };
        let mut items: Vec<PathBuf> = read_dir.flatten().map(|e| e.path()).collect();
        items.sort();
        for path in items {
            if self.policy.is_ignored(&path) {
                continue;
            }
            let Ok(meta) = fs::symlink_metadata(&path) else {
                continue;
            };
            if meta.file_type().is_symlink() {
                continue;
            }
            let rel = path
                .strip_prefix(&self.policy.root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            if meta.is_dir() {
                out.push(format!("{rel}/"));
                if depth < max_depth {
                    self.walk_list(&path, depth + 1, max_depth, out);
                }
            } else {
                out.push(rel);
            }
        }
    }

    fn fs_write(&self, args: FsWriteArgs) -> Result<String, String> {
        self.policy.check_control_plane_write(&args.path)?;
        let path = self.policy.resolve_for_write(&args.path)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("failed to create parent dir: {e}"))?;
        }
        fs::write(&path, args.content.as_bytes())
            .map_err(|e| format!("failed to write `{}`: {e}", args.path))?;
        self.invalidate_seen_file(&path);
        let mut obs = format!("wrote {} bytes to {}", args.content.len(), args.path);
        // `SDD-05` REQ-05.4: solo tras escritura exitosa (nunca si falló arriba).
        if let Some(block) = self.verify_after_edit(&path, None) {
            obs.push_str(&block);
        }
        Ok(obs)
    }

    fn fs_edit(&self, args: FsEditArgs) -> Result<String, String> {
        self.policy.check_control_plane_write(&args.path)?;
        let hunks = extract_edit_hunks(&args)?;
        let path = self.policy.resolve_existing(&args.path)?;
        let content = fs::read_to_string(&path)
            .map_err(|e| format!("failed to read `{}`: {e}", args.path))?;

        // `SDD-05` REQ-05.4: rangos 1-based inclusivos de cada hunk sobre el contenido
        // original (radio de impacto AST del Verify-on-Edit).
        let edited_ranges: Vec<(usize, usize)> = hunks
            .iter()
            .filter_map(|h| snippet_line_range(&content, &h.old_text))
            .collect();

        let mut working = content;
        let mut trimmed_count = 0usize;
        for (idx, hunk) in hunks.iter().enumerate() {
            let (updated, used_trimmed) =
                apply_single_edit_in_memory(&working, &hunk.old_text, &hunk.new_text).map_err(
                    |reason| {
                        if hunks.len() > 1 {
                            format!("in `{}` (hunk {}): {reason}", args.path, idx + 1)
                        } else {
                            format!("in `{}`: {reason}", args.path)
                        }
                    },
                )?;
            if used_trimmed {
                trimmed_count += 1;
            }
            working = updated;
        }

        fs::write(&path, working.as_bytes())
            .map_err(|e| format!("failed to write `{}`: {e}", args.path))?;
        self.invalidate_seen_file(&path);
        let mut obs = if trimmed_count > 0 {
            format!(
                "edited `{}` ({} replacement(s), {} line-trimmed)",
                args.path,
                hunks.len(),
                trimmed_count
            )
        } else {
            format!("edited `{}` ({} replacement(s))", args.path, hunks.len())
        };
        // `SDD-05` REQ-05.4: solo tras edición exitosa (nunca si falló arriba).
        if let Some(block) = self.verify_after_edit(&path, Some(&edited_ranges)) {
            obs.push_str(&block);
        }
        Ok(obs)
    }

    /// Bucle Verify-on-Edit tras una mutación exitosa (`SDD-05` REQ-05.4). Devuelve el
    /// bloque `[verify: ...]` a adjuntar a la observación, o `None` si `[verify]` está
    /// apagado, no hay reglas o ninguna empareja (la salida del tool queda intacta).
    fn verify_after_edit(
        &self,
        abs_path: &std::path::Path,
        #[cfg_attr(not(feature = "scent"), allow(unused_variables))] line_ranges: Option<
            &[(usize, usize)],
        >,
    ) -> Option<String> {
        if !self.policy.verify.on_edit || self.policy.verify.rules.is_empty() {
            return None;
        }
        let rel_path = abs_path
            .strip_prefix(&self.policy.root)
            .ok()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        if rel_path.is_empty() {
            return None;
        }

        // Radio de impacto AST: símbolos `scope:test` alcanzables en ≤ 2 hops desde el
        // archivo/líneas editados. Sin feature `scent` (o índice no disponible) se
        // degrada limpiamente a matching solo por el rel_path editado.
        let impacted_tests: Vec<String> = {
            #[cfg(feature = "scent")]
            {
                let mut guard = self.scent_index.lock().ok()?;
                let _ = guard.index_workspace_with_cache(&self.policy.root);
                guard.impacted_tests_for_path(
                    &rel_path,
                    line_ranges,
                    crate::verify::IMPACTED_TEST_MAX_HOPS,
                )
            }
            #[cfg(not(feature = "scent"))]
            {
                Vec::new()
            }
        };

        crate::verify::run_verify_on_edit(&self.policy, &rel_path, &impacted_tests, || {
            self.is_cancelled()
        })
    }

    fn load_skill(&self, args: LoadSkillArgs) -> Result<String, String> {
        let name = args.name.trim();
        if name.is_empty()
            || name.contains('/')
            || name.contains('\\')
            || name.contains("..")
            || name.contains('\0')
        {
            return Err(format!("invalid skill name `{name}`"));
        }

        for dir in skill_search_dirs(&self.policy.root) {
            let folder_skill = dir.join(name).join("SKILL.md");
            if let Ok(raw) = fs::read_to_string(&folder_skill) {
                let cleaned = scrub_secrets(&raw);
                return Ok(truncate_utf8(&cleaned, self.policy.max_read_bytes));
            }
            let flat_skill = dir.join(format!("{name}.md"));
            if let Ok(raw) = fs::read_to_string(&flat_skill) {
                let cleaned = scrub_secrets(&raw);
                return Ok(truncate_utf8(&cleaned, self.policy.max_read_bytes));
            }
        }

        if let Some(builtin) = builtin_skill_body(name) {
            return Ok(builtin.to_string());
        }

        Err(format!(
            "skill `{name}` not found (checked workspace `.onyx/skills`, `skills`, `~/.onyx/skills`, and built-ins `scent`, `rust`)"
        ))
    }

    fn plan_update(&self, raw_args: &str) -> Result<String, String> {
        format_plan_steps(raw_args)
    }

    fn code_search(&self, args: CodeSearchArgs) -> Result<String, String> {
        if args.pattern.is_empty() {
            return Err("pattern cannot be empty".to_string());
        }
        let base = args.path.as_deref().unwrap_or(".");
        let start = self.policy.resolve_existing(base)?;
        let max_results = args.max_results.unwrap_or(50).clamp(1, 200);
        let context_lines = args.context_lines.unwrap_or(0).min(5);

        let matcher_src = if args.regex {
            args.pattern.clone()
        } else {
            regex::escape(&args.pattern)
        };
        let matcher = RegexBuilder::new(&matcher_src)
            .case_insensitive(args.case_insensitive)
            .build()
            .map_err(|e| format!("invalid regex pattern `{}`: {e}", args.pattern))?;

        let include_re = match args.include_glob.as_deref().map(str::trim) {
            Some(g) if !g.is_empty() => Some(compile_glob_regex(g, false)?),
            _ => None,
        };
        let exclude_re = match args.exclude_glob.as_deref().map(str::trim) {
            Some(g) if !g.is_empty() => Some(compile_glob_regex(g, false)?),
            _ => None,
        };

        // `SDD-04` 4C-audit: las exclusiones por defecto solo se aplican
        // cuando se busca en todo el workspace (`path` ausente o `"."`);
        // con un `path` explícito se busca exactamente lo pedido (p. ej.
        // `path: ".onyx"`).
        let default_excludes = args
            .path
            .as_deref()
            .map(|p| p.trim() == ".")
            .unwrap_or(true);
        let opts = SearchOptions {
            matcher: &matcher,
            include_re: include_re.as_ref(),
            exclude_re: exclude_re.as_ref(),
            context_lines,
            max_results,
            default_excludes,
        };
        let mut match_count = 0usize;
        let mut blocks: Vec<String> = Vec::new();
        self.walk_search(&start, &opts, &mut match_count, &mut blocks);
        if blocks.is_empty() {
            Ok("no matches".to_string())
        } else {
            let joined = if context_lines > 0 {
                blocks.join("\n--\n")
            } else {
                blocks.join("\n")
            };
            let cleaned = scrub_secrets(&joined);
            let mut out = truncate_utf8(&cleaned, self.policy.max_output_bytes);
            if match_count >= max_results {
                // `SDD-04` 4C-audit: aviso accionable cuando el resultado se
                // recortó en `max_results`.
                out.push_str(&format!(
                    "\n# truncated: reached max_results ({max_results}); narrow with path or include_glob."
                ));
            }
            Ok(out)
        }
    }

    fn walk_search(
        &self,
        path: &Path,
        opts: &SearchOptions<'_>,
        match_count: &mut usize,
        blocks: &mut Vec<String>,
    ) {
        if *match_count >= opts.max_results || self.policy.is_ignored(path) {
            return;
        }
        let Ok(meta) = fs::symlink_metadata(path) else {
            return;
        };
        if meta.file_type().is_symlink() {
            return;
        }
        // `SDD-04` 4C-audit: exclusiones por defecto del workspace (lockfiles
        // y directorios internos) solo si la raíz de búsqueda era el
        // workspace; un `path` explícito las ignora.
        if opts.default_excludes {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if meta.is_file() {
                if matches!(
                    name,
                    "Cargo.lock" | "package-lock.json" | "pnpm-lock.yaml" | "yarn.lock"
                ) {
                    return;
                }
            } else if matches!(name, ".onyx" | ".agents") {
                return;
            }
        }
        if meta.is_file() {
            if meta.len() > 1024 * 1024 {
                return;
            }
            let rel = path
                .strip_prefix(&self.policy.root)
                .unwrap_or(path)
                .to_string_lossy()
                .replace('\\', "/");
            if let Some(inc) = opts.include_re {
                if !inc.is_match(&rel) {
                    return;
                }
            }
            if let Some(exc) = opts.exclude_re {
                if exc.is_match(&rel) {
                    return;
                }
            }
            let Ok(text) = fs::read_to_string(path) else {
                return;
            };
            let lines: Vec<&str> = text.lines().collect();
            let mut matched_indices: Vec<usize> = Vec::new();
            for (idx, line) in lines.iter().enumerate() {
                if opts.matcher.is_match(line) {
                    matched_indices.push(idx);
                    *match_count += 1;
                    if *match_count >= opts.max_results {
                        break;
                    }
                }
            }
            if matched_indices.is_empty() {
                return;
            }
            if opts.context_lines == 0 {
                for idx in matched_indices {
                    // `SDD-04` 4C-audit: recorte de líneas > 240 chars.
                    blocks.push(format!(
                        "{rel}:{}: {}",
                        idx + 1,
                        clip_search_line(lines[idx])
                    ));
                }
            } else {
                // Agrupa rangos solapados [idx - ctx ..= idx + ctx] en bloques separados por `--`.
                let mut ranges: Vec<(usize, usize)> = Vec::new();
                for &idx in &matched_indices {
                    let start = idx.saturating_sub(opts.context_lines);
                    let end = (idx + opts.context_lines).min(lines.len().saturating_sub(1));
                    if let Some(last) = ranges.last_mut() {
                        if start <= last.1.saturating_add(1) {
                            last.1 = last.1.max(end);
                            continue;
                        }
                    }
                    ranges.push((start, end));
                }
                for (start, end) in ranges {
                    let mut hunk = Vec::with_capacity(end - start + 1);
                    for (line_idx, line) in lines.iter().enumerate().take(end + 1).skip(start) {
                        // `SDD-04` 4C-audit: recorte de líneas > 240 chars.
                        let shown = clip_search_line(line);
                        let is_match = matched_indices.binary_search(&line_idx).is_ok();
                        if is_match {
                            hunk.push(format!("{rel}:{}: {shown}", line_idx + 1));
                        } else {
                            hunk.push(format!("{rel}-{}- {shown}", line_idx + 1));
                        }
                    }
                    blocks.push(hunk.join("\n"));
                }
            }
            return;
        }
        if let Ok(read_dir) = fs::read_dir(path) {
            let mut items: Vec<PathBuf> = read_dir.flatten().map(|e| e.path()).collect();
            items.sort();
            for item in items {
                self.walk_search(&item, opts, match_count, blocks);
                if *match_count >= opts.max_results {
                    break;
                }
            }
        }
    }

    fn glob_files(&self, args: GlobFilesArgs) -> Result<String, String> {
        let raw_pattern = args.pattern.trim();
        if raw_pattern.is_empty() {
            return Err("glob pattern cannot be empty".to_string());
        }
        if raw_pattern.contains('\0') {
            return Err("glob pattern contains NUL byte".to_string());
        }
        let norm_pattern = raw_pattern.replace('\\', "/");
        if Path::new(raw_pattern).is_absolute()
            || norm_pattern.starts_with('/')
            || norm_pattern.split('/').any(|seg| seg == "..")
        {
            return Err(format!(
                "glob pattern `{raw_pattern}` escapes workspace root"
            ));
        }

        let base = args.path.as_deref().unwrap_or(".");
        let start = self.policy.resolve_existing(base)?;
        let glob_re = compile_glob_regex(raw_pattern, false)?;
        let max_results = args.max_results.unwrap_or(100).clamp(1, 500);

        let mut matched: Vec<(String, SystemTime)> = Vec::new();
        self.walk_glob(&start, &start, &glob_re, &mut matched);

        if args.sort_by_mtime {
            matched.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        } else {
            matched.sort_by(|a, b| a.0.cmp(&b.0));
        }

        if matched.is_empty() {
            return Ok("no files matched".to_string());
        }

        let total = matched.len();
        let mut lines: Vec<String> = matched
            .into_iter()
            .take(max_results)
            .map(|(rel, _)| rel)
            .collect();
        if total > max_results {
            lines.push(format!(
                "... (truncated to {max_results} of {total} matching files)"
            ));
        }
        Ok(truncate_utf8(
            &lines.join("\n"),
            self.policy.max_output_bytes,
        ))
    }

    fn walk_glob(
        &self,
        start: &Path,
        path: &Path,
        glob_re: &Regex,
        out: &mut Vec<(String, SystemTime)>,
    ) {
        if self.policy.is_ignored(path) {
            return;
        }
        let Ok(meta) = fs::symlink_metadata(path) else {
            return;
        };
        if meta.file_type().is_symlink() {
            return;
        }
        if meta.is_file() {
            let rel_root = path
                .strip_prefix(&self.policy.root)
                .unwrap_or(path)
                .to_string_lossy()
                .replace('\\', "/");
            let rel_start = path
                .strip_prefix(start)
                .unwrap_or(path)
                .to_string_lossy()
                .replace('\\', "/");
            if glob_re.is_match(&rel_start) || glob_re.is_match(&rel_root) {
                let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                out.push((rel_root, mtime));
            }
            return;
        }
        if let Ok(read_dir) = fs::read_dir(path) {
            let mut items: Vec<PathBuf> = read_dir.flatten().map(|e| e.path()).collect();
            items.sort();
            for item in items {
                self.walk_glob(start, &item, glob_re, out);
            }
        }
    }

    fn git_status(&self) -> Result<String, String> {
        let mut cmd = Command::new("git");
        cmd.args(["status", "--short", "--branch"]);
        let (code, stdout, stderr) = self.exec_bounded_raw(cmd, "git status")?;
        if code != 0 {
            return Err(format!("git status failed (exit {code}): {stderr}"));
        }
        if stdout.trim().is_empty() {
            Ok("(clean working tree)".to_string())
        } else {
            Ok(truncate_utf8(&stdout, self.policy.max_output_bytes))
        }
    }

    fn git_diff(&self, args: GitDiffArgs) -> Result<String, String> {
        let ctx = args.context_lines.unwrap_or(3).min(20);
        let mut cmd = Command::new("git");
        cmd.arg("diff").arg("--no-color").arg(format!("-U{ctx}"));
        if args.staged.unwrap_or(false) {
            cmd.arg("--cached");
        }
        if let Some(raw_path) = args
            .path
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            let canon = self.policy.resolve_existing(raw_path)?;
            let rel = canon
                .strip_prefix(&self.policy.root)
                .map_err(|_| format!("path `{raw_path}` escapes workspace root"))?
                .to_string_lossy()
                .replace('\\', "/");
            if !rel.is_empty() {
                cmd.arg("--").arg(rel);
            }
        }
        let (code, stdout, stderr) = self.exec_bounded_raw(cmd, "git diff")?;
        if code != 0 {
            return Err(format!("git diff failed (exit {code}): {stderr}"));
        }
        if stdout.trim().is_empty() {
            Ok("(no diff)".to_string())
        } else {
            Ok(truncate_utf8(&stdout, self.policy.max_output_bytes))
        }
    }

    fn shell_exec(&self, args: ShellExecArgs) -> Result<String, String> {
        if args.command.trim().is_empty() {
            return Err("command cannot be empty".to_string());
        }
        let sandbox_tmp = ensure_sandbox_tmp_dir(&self.policy.root)?;
        let cmd = prepare_sandboxed_shell_command(
            &self.policy.sandbox,
            &self.policy.root,
            &sandbox_tmp,
            &args.command,
        )?;
        let (code, stdout, stderr) = self.exec_bounded_raw(cmd, "shell command")?;
        self.clear_seen_reads();
        let combined = format!("exit_code: {code}\nstdout:\n{stdout}\nstderr:\n{stderr}");
        Ok(truncate_utf8(&combined, self.policy.max_output_bytes))
    }

    fn exec_bounded_raw(
        &self,
        mut cmd: Command,
        label: &str,
    ) -> Result<(i32, String, String), String> {
        cmd.current_dir(clean_process_path(&self.policy.root))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        configure_process_isolation(&mut cmd, false);

        let mut child = cmd
            .spawn()
            .map_err(|e| format!("failed to spawn {label}: {e}"))?;
        let mut job = ProcessJob::assign(&child, self.policy.sandbox.limits());
        let out_handle = read_pipe_capped(child.stdout.take(), self.policy.max_output_bytes);
        let err_handle = read_pipe_capped(child.stderr.take(), self.policy.max_output_bytes);
        let deadline = Instant::now() + self.policy.command_timeout;

        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {
                    let cancelled = self.is_cancelled();
                    if cancelled || Instant::now() >= deadline {
                        if let Some(ref mut j) = job {
                            j.terminate();
                        } else {
                            #[cfg(windows)]
                            {
                                let _ = Command::new("taskkill")
                                    .args(["/F", "/T", "/PID", &child.id().to_string()])
                                    .stdout(Stdio::null())
                                    .stderr(Stdio::null())
                                    .status();
                            }
                        }
                        let _ = child.kill();
                        let _ = child.wait();
                        drop(job);
                        let _ = out_handle.join();
                        let _ = err_handle.join();
                        if cancelled {
                            return Err(format!("{label} cancelled"));
                        }
                        return Err(format!(
                            "{label} timed out after {:?}",
                            self.policy.command_timeout
                        ));
                    }
                    thread::sleep(Duration::from_millis(20));
                }
                Err(e) => {
                    if let Some(ref mut j) = job {
                        j.terminate();
                    }
                    drop(job);
                    return Err(format!("wait failed: {e}"));
                }
            }
        };

        // Termina explícitamente el `ProcessJob` RAII antes de devolver para que ningún
        // subproceso nieto huérfano retenga handles en el worktree (crítico en Windows).
        if let Some(ref mut j) = job {
            j.terminate();
        }
        drop(job);
        let stdout = scrub_secrets(&out_handle.join().unwrap_or_default());
        let stderr = scrub_secrets(&err_handle.join().unwrap_or_default());
        Ok((status.code().unwrap_or(-1), stdout, stderr))
    }

    /// `SDD-07` Fase 2: ejecuta un comando shell verificado por el usuario en el
    /// borde de sesión (`! <cmd>`). Misma pipeline que `shell_exec` (sandbox
    /// `prepare_sandboxed_shell_command`, `ProcessJob`, `command_timeout`), pero
    /// sin escribir en el grafo ni consumir un turno de LLM: el resultado se
    /// devuelve para sellarlo en `ONYXTR2` (`/__onyx_det_shell <json>`).
    pub fn exec_det_shell(&self, command: &str) -> Result<DetShellExecResult, String> {
        if command.trim().is_empty() {
            return Err("command cannot be empty".to_string());
        }
        let sandbox_tmp = ensure_sandbox_tmp_dir(&self.policy.root)?;
        let mut cmd = prepare_sandboxed_shell_command(
            &self.policy.sandbox,
            &self.policy.root,
            &sandbox_tmp,
            command,
        )?;
        cmd.current_dir(clean_process_path(&self.policy.root))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        configure_process_isolation(&mut cmd, false);

        let mut child = cmd
            .spawn()
            .map_err(|e| format!("failed to spawn shell command: {e}"))?;
        let mut job = ProcessJob::assign(&child, self.policy.sandbox.limits());
        let out_handle = read_pipe_capped(child.stdout.take(), self.policy.max_output_bytes);
        let err_handle = read_pipe_capped(child.stderr.take(), self.policy.max_output_bytes);
        let deadline = Instant::now() + self.policy.command_timeout;

        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {
                    if Instant::now() >= deadline {
                        if let Some(ref mut j) = job {
                            j.terminate();
                        } else {
                            #[cfg(windows)]
                            {
                                let _ = Command::new("taskkill")
                                    .args(["/F", "/T", "/PID", &child.id().to_string()])
                                    .stdout(Stdio::null())
                                    .stderr(Stdio::null())
                                    .status();
                            }
                        }
                        let _ = child.kill();
                        let _ = child.wait();
                        drop(job);
                        let _ = out_handle.join();
                        let _ = err_handle.join();
                        return Ok(DetShellExecResult {
                            exit_code: None,
                            timed_out: true,
                            stdout: String::new(),
                            stderr: format!(
                                "shell command timed out after {:?}",
                                self.policy.command_timeout
                            ),
                        });
                    }
                    thread::sleep(Duration::from_millis(20));
                }
                Err(e) => {
                    if let Some(ref mut j) = job {
                        j.terminate();
                    }
                    drop(job);
                    return Err(format!("wait failed: {e}"));
                }
            }
        };

        // Termina explícitamente el `ProcessJob` RAII antes de devolver (misma
        // disciplina que `exec_bounded_raw`).
        if let Some(ref mut j) = job {
            j.terminate();
        }
        drop(job);
        let stdout = scrub_secrets(&out_handle.join().unwrap_or_default());
        let stderr = scrub_secrets(&err_handle.join().unwrap_or_default());
        Ok(DetShellExecResult {
            exit_code: status.code(),
            timed_out: false,
            stdout,
            stderr,
        })
    }

    #[cfg(feature = "scent")]
    fn scent_outline(&self, args: ScentOutlineArgs) -> Result<String, String> {
        let base = args.path.as_deref().unwrap_or(".");
        let resolved = self.policy.resolve_existing(base)?;
        let opts = onyx_scent::ToonPageOptions {
            cursor: args.cursor.unwrap_or(0),
            max_rows: args
                .max_rows
                .unwrap_or(onyx_scent::DEFAULT_MAX_ROWS)
                .clamp(1, 500),
            public_only: args.public_only.unwrap_or(false),
            exclude_tests: args.exclude_tests.unwrap_or(false),
            ..onyx_scent::ToonPageOptions::default()
        };
        let cache_key = format!(
            "{}:{}:{}:{}:{}",
            resolved.display(),
            opts.cursor,
            opts.max_rows,
            opts.public_only,
            opts.exclude_tests
        );
        if let Ok(guard) = self.seen_outlines.lock() {
            if let Some(prev) = guard.get(&cache_key) {
                return Ok(format!(
                    "[cached scent_outline — already retrieved above; do not call again without editing files]\n{prev}"
                ));
            }
        }
        let mut idx = self
            .scent_index
            .lock()
            .map_err(|_| "scent index lock poisoned".to_string())?;
        idx.index_target_with_cache(&self.policy.root, Some(&resolved))
            .map_err(|e| e.to_string())?;
        let out = truncate_utf8(&idx.outline_toon(opts), self.policy.max_output_bytes);
        if let Ok(mut guard) = self.seen_outlines.lock() {
            guard.insert(cache_key, out.clone());
        }
        Ok(out)
    }

    #[cfg(feature = "scent")]
    fn scent_impact(&self, args: ScentImpactArgs) -> Result<String, String> {
        let mut idx = self
            .scent_index
            .lock()
            .map_err(|_| "scent index lock poisoned".to_string())?;
        idx.index_workspace_with_cache(&self.policy.root)
            .map_err(|e| e.to_string())?;
        let hops = args.max_hops.unwrap_or(3).clamp(1, 6);
        let opts = onyx_scent::ToonPageOptions {
            cursor: args.cursor.unwrap_or(0),
            max_rows: args
                .max_rows
                .unwrap_or(onyx_scent::DEFAULT_MAX_ROWS)
                .clamp(1, 500),
            public_only: args.public_only.unwrap_or(false),
            exclude_tests: args.exclude_tests.unwrap_or(false),
            ..onyx_scent::ToonPageOptions::default()
        };
        let toon = idx
            .impact_toon(
                &args.target,
                &onyx_scent::DistillerConfig {
                    iterations: hops,
                    ..onyx_scent::DistillerConfig::default()
                },
                opts,
            )
            .map_err(|e| e.to_string())?;
        Ok(truncate_utf8(&toon, self.policy.max_output_bytes))
    }

    #[cfg(feature = "scent")]
    fn scent_references(&self, args: ScentReferencesArgs) -> Result<String, String> {
        let mut idx = self
            .scent_index
            .lock()
            .map_err(|_| "scent index lock poisoned".to_string())?;
        idx.index_workspace_with_cache(&self.policy.root)
            .map_err(|e| e.to_string())?;
        let opts = onyx_scent::ToonPageOptions {
            cursor: args.cursor.unwrap_or(0),
            max_rows: args
                .max_rows
                .unwrap_or(onyx_scent::DEFAULT_MAX_ROWS)
                .clamp(1, 500),
            public_only: args.public_only.unwrap_or(false),
            exclude_tests: args.exclude_tests.unwrap_or(false),
            ..onyx_scent::ToonPageOptions::default()
        };
        let toon = idx
            .references_toon(&args.target, opts)
            .map_err(|e| e.to_string())?;
        Ok(truncate_utf8(&toon, self.policy.max_output_bytes))
    }
}

pub(crate) fn read_pipe_capped<R: Read + Send + 'static>(
    pipe: Option<R>,
    cap: usize,
) -> thread::JoinHandle<String> {
    thread::spawn(move || {
        let Some(mut reader) = pipe else {
            return String::new();
        };
        let mut collected = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            match reader.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if collected.len() < cap {
                        let take = (cap - collected.len()).min(n);
                        collected.extend_from_slice(&chunk[..take]);
                    }
                }
            }
        }
        String::from_utf8_lossy(&collected).into_owned()
    })
}

pub(crate) fn truncate_utf8(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n...[truncated]", &s[..end])
}

#[derive(Deserialize)]
struct FsReadArgs {
    path: String,
    start_line: Option<usize>,
    end_line: Option<usize>,
}

#[derive(Deserialize)]
struct FsListArgs {
    path: Option<String>,
    max_depth: Option<usize>,
}

#[derive(Deserialize)]
struct FsWriteArgs {
    path: String,
    content: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct FsEditHunk {
    pub old_text: String,
    pub new_text: String,
}

#[derive(Deserialize)]
struct FsEditArgs {
    path: String,
    #[serde(default)]
    old_text: Option<String>,
    #[serde(default)]
    new_text: Option<String>,
    #[serde(default)]
    edits: Option<Vec<FsEditHunk>>,
}

#[derive(Deserialize)]
struct LoadSkillArgs {
    name: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct PlanStepItem {
    pub step: String,
    pub status: String,
}

#[derive(Deserialize)]
struct PlanUpdateArgs {
    #[serde(default)]
    explanation: Option<String>,
    steps: Vec<PlanStepItem>,
}

struct SearchOptions<'a> {
    matcher: &'a Regex,
    include_re: Option<&'a Regex>,
    exclude_re: Option<&'a Regex>,
    context_lines: usize,
    max_results: usize,
    /// `SDD-04` 4C-audit: exclusiones por defecto (lockfiles, `.onyx`,
    /// `.agents`) solo cuando la raíz de búsqueda es el workspace; un `path`
    /// explícito busca exactamente lo pedido.
    default_excludes: bool,
}

#[derive(Deserialize)]
struct CodeSearchArgs {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    max_results: Option<usize>,
    #[serde(default)]
    regex: bool,
    #[serde(default)]
    case_insensitive: bool,
    #[serde(default)]
    include_glob: Option<String>,
    #[serde(default)]
    exclude_glob: Option<String>,
    #[serde(default)]
    context_lines: Option<usize>,
}

#[derive(Deserialize)]
struct GlobFilesArgs {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    max_results: Option<usize>,
    #[serde(default)]
    sort_by_mtime: bool,
}

#[derive(Deserialize)]
struct ShellExecArgs {
    command: String,
}

#[derive(Deserialize, Default)]
struct ServiceStartArgs {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    all: bool,
    #[serde(default)]
    wait_ready_secs: Option<u64>,
}

#[derive(Deserialize, Default)]
struct ServiceStopArgs {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    all: bool,
}

#[derive(Deserialize)]
struct ServiceLogsArgs {
    id: String,
    #[serde(default)]
    tail: Option<usize>,
    #[serde(default)]
    pattern: Option<String>,
    #[serde(default)]
    regex: bool,
}

#[derive(Deserialize)]
struct ServiceExecArgs {
    id: String,
    command: String,
    #[serde(default)]
    timeout_secs: Option<u64>,
}

#[derive(Deserialize, Default)]
struct GitDiffArgs {
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    staged: Option<bool>,
    #[serde(default)]
    context_lines: Option<usize>,
}

#[cfg(feature = "scent")]
#[derive(Deserialize)]
struct ScentOutlineArgs {
    path: Option<String>,
    public_only: Option<bool>,
    exclude_tests: Option<bool>,
    cursor: Option<usize>,
    max_rows: Option<usize>,
}

#[cfg(feature = "scent")]
#[derive(Deserialize)]
struct ScentImpactArgs {
    target: String,
    max_hops: Option<u8>,
    public_only: Option<bool>,
    exclude_tests: Option<bool>,
    cursor: Option<usize>,
    max_rows: Option<usize>,
}

#[cfg(feature = "scent")]
#[derive(Deserialize)]
struct ScentReferencesArgs {
    target: String,
    public_only: Option<bool>,
    exclude_tests: Option<bool>,
    cursor: Option<usize>,
    max_rows: Option<usize>,
}

fn extract_edit_hunks(args: &FsEditArgs) -> Result<Vec<FsEditHunk>, String> {
    let mut hunks = Vec::new();
    match (&args.old_text, &args.new_text) {
        (Some(old_text), Some(new_text)) => hunks.push(FsEditHunk {
            old_text: old_text.clone(),
            new_text: new_text.clone(),
        }),
        (None, None) => {}
        _ => {
            return Err(
                "fs_edit requires both `old_text` and `new_text`, or `edits` array".to_string(),
            );
        }
    }
    if let Some(extra) = &args.edits {
        hunks.extend(extra.iter().cloned());
    }
    if hunks.is_empty() {
        return Err(
            "fs_edit requires at least one edit hunk (`old_text`/`new_text` or `edits`)"
                .to_string(),
        );
    }
    Ok(hunks)
}

/// Rango de líneas 1-based inclusivo `(start_line, end_line)` del primer `needle` dentro de
/// `content` (`None` si no aparece). `SDD-05` REQ-05.4: alimenta el radio de impacto AST
/// del Verify-on-Edit con las líneas realmente tocadas por un hunk de `fs_edit`.
fn snippet_line_range(content: &str, needle: &str) -> Option<(usize, usize)> {
    if needle.is_empty() {
        return None;
    }
    let byte_start = content.find(needle)?;
    let start_line = content[..byte_start].matches('\n').count() + 1;
    let end_line = start_line
        + content[byte_start..byte_start + needle.len()]
            .matches('\n')
            .count();
    Some((start_line, end_line))
}

fn apply_single_edit_in_memory(
    content: &str,
    old_text: &str,
    new_text: &str,
) -> Result<(String, bool), String> {
    if old_text.is_empty() {
        return Err("old_text cannot be empty".to_string());
    }
    let exact_count = content.matches(old_text).count();
    if exact_count == 1 {
        return Ok((content.replacen(old_text, new_text, 1), false));
    }
    if exact_count > 1 {
        return Err(format!(
            "old_text matched {exact_count} times; provide more context"
        ));
    }
    let updated = replace_line_trimmed_unique(content, old_text, new_text)?;
    Ok((updated, true))
}

/// Formatea un plan estructurado (`steps` con estados `completed`, `in_progress`, `pending`)
/// como lista `[x]` / `[>]` / `[ ]` apta para persistirse en `ws.plan.text` y mostrarse en TUI.
pub fn format_plan_steps(args_json: &str) -> Result<String, String> {
    let raw = if args_json.trim().is_empty() {
        "{}"
    } else {
        args_json
    };
    let args: PlanUpdateArgs =
        serde_json::from_str(raw).map_err(|e| format!("invalid plan_update args: {e}"))?;
    if args.steps.is_empty() {
        return Err("plan_update requires at least one step".to_string());
    }

    let mut lines = Vec::new();
    if let Some(expl) = args
        .explanation
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        lines.push(expl.to_string());
    }
    for item in &args.steps {
        let marker = match item.status.trim().to_ascii_lowercase().as_str() {
            "completed" | "done" => "[x]",
            "in_progress" | "active" => "[>]",
            _ => "[ ]",
        };
        lines.push(format!("{marker} {}", item.step.trim()));
    }
    Ok(lines.join("\n"))
}

fn skill_search_dirs(workspace_root: &Path) -> Vec<PathBuf> {
    let mut dirs = vec![
        workspace_root.join(".onyx").join("skills"),
        workspace_root.join("skills"),
    ];
    if let Some(home) = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")) {
        dirs.push(PathBuf::from(home).join(".onyx").join("skills"));
    }
    dirs
}

fn builtin_skill_body(name: &str) -> Option<&'static str> {
    match name {
        "scent" => Some(
            "# Skill: Scent Two-Lens AST Navigation\n\
1. Start broad with `scent_outline` (`exclude_tests: true` when mapping production architecture, `public_only: true` for public APIs, smaller `max_rows` like 80 if exploring large crates).\n\
2. Paginate whenever `# page: next_cursor=<N>` appears before assuming you have seen all symbols.\n\
3. Before modifying any shared type or function, run `scent_impact` (`max_hops: 2..=4`) to locate callers and callees by exact `<file>::<symbol>` names.\n\
4. Read only the relevant line ranges returned by `lines:start-end` via `fs_read(path, start_line, end_line)`.",
        ),
        "rust" => Some(
            "# Skill: OnYX Rust 1.85 Workspace Conventions\n\
1. Keep `onyx-core`, `onyx-soul`, and `onyx-soul-agents` at ZERO external dependencies.\n\
2. Maintain Rust 1.85.1 compatibility (no `&dyn Graph -> &dyn GraphView` trait upcasting; prefer `<G: GraphView + ?Sized>` or `<G: Graph + ?Sized>`).\n\
3. Keep `OnyxSystem`s pure and deterministic (zero disk, network, or wall-clock I/O; isolate I/O in `PlatformDriver` adapters).\n\
4. Verify edits with `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, and `cargo test --workspace`.",
        ),
        _ => None,
    }
}

/// Descubre las skills disponibles (locales `SKILL.md`/`*.md` y embebidas) sin inflar `systemInstruction`.
pub fn discover_skills_summary(workspace_root: &Path) -> String {
    let mut entries: BTreeMap<String, String> = BTreeMap::new();
    entries.insert(
        "scent".to_string(),
        "Two-lens AST navigation (`scent_outline` + `scent_impact` + bounded `fs_read`)"
            .to_string(),
    );
    entries.insert(
        "rust".to_string(),
        "OnYX Rust 1.85 conventions, 0-dep core boundary, and verification checks".to_string(),
    );

    for dir in skill_search_dirs(workspace_root) {
        let Ok(read_dir) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in read_dir.flatten() {
            let path = entry.path();
            let Some(name_os) = path.file_name() else {
                continue;
            };
            let raw_name = name_os.to_string_lossy();
            let skill_info = if path.is_dir() {
                let skill_md = path.join("SKILL.md");
                if skill_md.is_file() {
                    Some((raw_name.into_owned(), skill_md))
                } else {
                    None
                }
            } else if path.is_file() && raw_name.ends_with(".md") {
                Some((raw_name.trim_end_matches(".md").to_string(), path.clone()))
            } else {
                None
            };
            if let Some((name, file_path)) = skill_info {
                let summary = fs::read_to_string(&file_path)
                    .ok()
                    .and_then(|txt| extract_skill_description(&txt))
                    .unwrap_or_else(|| "Custom workspace skill".to_string());
                entries.entry(name).or_insert(summary);
            }
        }
    }

    let mut out = String::from(
        "Available on-demand skills (call `load_skill(name)` before specialized work):\n",
    );
    for (name, desc) in entries {
        out.push_str(&format!("- `{name}`: {desc}\n"));
    }
    out
}

fn extract_skill_description(raw: &str) -> Option<String> {
    for line in raw.lines().take(20) {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed == "---" || trimmed.starts_with('#') {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("description:") {
            let clean = rest.trim().trim_matches('"').trim_matches('\'');
            if !clean.is_empty() {
                return Some(scrub_secrets(clean));
            }
        } else if !trimmed.starts_with("name:") {
            return Some(scrub_secrets(trimmed));
        }
    }
    None
}

/// Genera un diff unificado compacto de vista previa antes de aprobar `fs_edit` o `fs_write`.
pub fn preview_tool_diff(
    root: &Path,
    tool: &str,
    args_json: &str,
    max_lines: usize,
) -> Option<String> {
    let cap = max_lines.max(4);
    let policy = WorkspacePolicy::new(root).ok()?;
    match tool {
        "fs_edit" => {
            let args: FsEditArgs = serde_json::from_str(args_json).ok()?;
            let hunks = extract_edit_hunks(&args).ok()?;
            let resolved = policy.resolve_existing(&args.path).ok()?;
            let mut current = fs::read_to_string(&resolved).ok()?;
            let rel = args.path.trim().replace('\\', "/");

            let mut all_lines: Vec<String> = Vec::new();
            for (idx, hunk) in hunks.iter().enumerate() {
                let (byte_start, byte_end, replacement_preview) =
                    find_edit_span_and_replacement(&current, &hunk.old_text, &hunk.new_text)?;
                let diff = format_span_diff(
                    &rel,
                    &current,
                    byte_start,
                    byte_end,
                    &replacement_preview,
                    usize::MAX,
                );
                for (lno, line) in diff.lines().enumerate() {
                    if idx > 0 && lno < 2 && (line.starts_with("--- ") || line.starts_with("+++ "))
                    {
                        continue;
                    }
                    all_lines.push(line.to_string());
                }
                let (updated, _) =
                    apply_single_edit_in_memory(&current, &hunk.old_text, &hunk.new_text).ok()?;
                current = updated;
            }
            if all_lines.len() > cap {
                let omitted = all_lines.len() - cap;
                all_lines.truncate(cap);
                all_lines.push(format!("... (+{omitted} more lines)"));
            }
            Some(all_lines.join("\n"))
        }
        "fs_write" => {
            let args: FsWriteArgs = serde_json::from_str(args_json).ok()?;
            let resolved = policy.resolve_for_write(&args.path).ok()?;
            let rel = args.path.trim().replace('\\', "/");
            if let Ok(existing) = fs::read_to_string(&resolved) {
                Some(format_span_diff(
                    &rel,
                    &existing,
                    0,
                    existing.len(),
                    &args.content,
                    cap,
                ))
            } else {
                let mut lines = vec![
                    "--- /dev/null".to_string(),
                    format!("+++ b/{rel}"),
                    format!("@@ -0,0 +1,{} @@", args.content.lines().count().max(1)),
                ];
                let new_lines: Vec<&str> = args.content.lines().collect();
                let show = new_lines.len().min(cap.saturating_sub(3));
                for l in &new_lines[..show] {
                    lines.push(format!("+{l}"));
                }
                if new_lines.len() > show {
                    lines.push(format!("... (+{} more lines)", new_lines.len() - show));
                }
                Some(lines.join("\n"))
            }
        }
        _ => None,
    }
}

fn format_span_diff(
    rel_path: &str,
    content: &str,
    byte_start: usize,
    byte_end: usize,
    new_text: &str,
    max_lines: usize,
) -> String {
    let line_start = content[..byte_start].rfind('\n').map_or(0, |idx| idx + 1);
    let line_end = if byte_end > line_start && content[..byte_end].ends_with('\n') {
        byte_end
    } else {
        content[byte_end..]
            .find('\n')
            .map_or(content.len(), |idx| byte_end + idx + 1)
    };
    let full_new = format!(
        "{}{}{}",
        &content[line_start..byte_start],
        new_text,
        &content[byte_end..line_end]
    );
    let prefix_lines: Vec<&str> = content[..line_start].lines().collect();
    let old_lines: Vec<&str> = content[line_start..line_end].lines().collect();
    let new_lines: Vec<&str> = full_new.lines().collect();
    let suffix_lines: Vec<&str> = content[line_end..].lines().collect();

    let ctx_before_count = prefix_lines.len().min(2);
    let ctx_after_count = suffix_lines.len().min(2);
    let start_line = prefix_lines.len().saturating_sub(ctx_before_count) + 1;
    let old_total = ctx_before_count + old_lines.len() + ctx_after_count;
    let new_total = ctx_before_count + new_lines.len() + ctx_after_count;

    let mut diff_lines = vec![
        format!("--- a/{rel_path}"),
        format!("+++ b/{rel_path}"),
        format!("@@ -{start_line},{old_total} +{start_line},{new_total} @@"),
    ];
    for l in &prefix_lines[prefix_lines.len() - ctx_before_count..] {
        diff_lines.push(format!(" {l}"));
    }
    for l in &old_lines {
        diff_lines.push(format!("-{l}"));
    }
    for l in &new_lines {
        diff_lines.push(format!("+{l}"));
    }
    for l in &suffix_lines[..ctx_after_count] {
        diff_lines.push(format!(" {l}"));
    }

    if diff_lines.len() > max_lines {
        let omitted = diff_lines.len() - max_lines;
        diff_lines.truncate(max_lines);
        diff_lines.push(format!("... (+{omitted} more lines)"));
    }
    diff_lines.join("\n")
}

fn find_edit_span_and_replacement(
    content: &str,
    old_text: &str,
    new_text: &str,
) -> Option<(usize, usize, String)> {
    let exact: Vec<(usize, &str)> = content.match_indices(old_text).collect();
    if exact.len() == 1 {
        let start = exact[0].0;
        return Some((start, start + old_text.len(), new_text.to_string()));
    }
    if exact.len() > 1 {
        return None;
    }
    let (byte_start, byte_end) = find_line_trimmed_unique_span(content, old_text).ok()?;
    let adjusted = reindent_trimmed_replacement(&content[byte_start..byte_end], old_text, new_text);
    Some((byte_start, byte_end, adjusted))
}

fn find_line_trimmed_unique_span(content: &str, old_text: &str) -> Result<(usize, usize), String> {
    let old_lines: Vec<&str> = old_text.lines().collect();
    if old_lines.is_empty() {
        return Err("old_text cannot be empty".to_string());
    }

    let mut line_spans: Vec<(usize, usize, &str)> = Vec::new();
    let mut offset = 0usize;
    for chunk in content.split_inclusive('\n') {
        let trimmed_line = chunk.strip_suffix('\n').unwrap_or(chunk);
        let trimmed_line = trimmed_line.strip_suffix('\r').unwrap_or(trimmed_line);
        line_spans.push((offset, offset + chunk.len(), trimmed_line));
        offset += chunk.len();
    }
    if offset < content.len() {
        let tail = &content[offset..];
        line_spans.push((offset, content.len(), tail));
    }

    let window_len = old_lines.len();
    if window_len > line_spans.len() {
        return Err("old_text not found".to_string());
    }

    let mut matches: Vec<(usize, usize)> = Vec::new();
    for start in 0..=(line_spans.len() - window_len) {
        let matched =
            (0..window_len).all(|i| line_spans[start + i].2.trim() == old_lines[i].trim());
        if matched {
            let byte_start = line_spans[start].0;
            let byte_end = line_spans[start + window_len - 1].1;
            matches.push((byte_start, byte_end));
        }
    }

    match matches.len() {
        0 => Err("old_text not found".to_string()),
        1 => Ok(matches[0]),
        n => Err(format!(
            "old_text matched {n} times (line-trimmed); provide more context"
        )),
    }
}

fn first_non_empty_indent(text: &str) -> &str {
    for line in text.lines() {
        if !line.trim().is_empty() {
            let len = line
                .bytes()
                .take_while(|b| *b == b' ' || *b == b'\t')
                .count();
            return &line[..len];
        }
    }
    ""
}

fn reindent_trimmed_replacement(matched_slice: &str, old_text: &str, new_text: &str) -> String {
    let actual_indent = first_non_empty_indent(matched_slice);
    let old_indent = first_non_empty_indent(old_text);
    let new_indent = first_non_empty_indent(new_text);

    if new_indent != old_indent || old_indent == actual_indent {
        return new_text.to_string();
    }

    let mut out = String::with_capacity(new_text.len() + 16);
    if let Some(delta) = actual_indent.strip_prefix(old_indent) {
        for chunk in new_text.split_inclusive('\n') {
            if chunk.trim().is_empty() {
                out.push_str(chunk);
            } else {
                out.push_str(delta);
                out.push_str(chunk);
            }
        }
        return out;
    }

    if let Some(strip) = old_indent.strip_prefix(actual_indent) {
        for chunk in new_text.split_inclusive('\n') {
            if chunk.trim().is_empty() {
                out.push_str(chunk);
            } else {
                out.push_str(chunk.strip_prefix(strip).unwrap_or(chunk));
            }
        }
        return out;
    }

    new_text.to_string()
}

fn replace_line_trimmed_unique(
    content: &str,
    old_text: &str,
    new_text: &str,
) -> Result<String, String> {
    let (byte_start, byte_end) = find_line_trimmed_unique_span(content, old_text)?;
    let adjusted_new =
        reindent_trimmed_replacement(&content[byte_start..byte_end], old_text, new_text);
    let mut out = String::with_capacity(content.len() + adjusted_new.len());
    out.push_str(&content[..byte_start]);
    out.push_str(&adjusted_new);
    if content[..byte_end].ends_with('\n') && !adjusted_new.ends_with('\n') {
        if content[..byte_end].ends_with("\r\n") {
            out.push_str("\r\n");
        } else {
            out.push('\n');
        }
    }
    out.push_str(&content[byte_end..]);
    Ok(out)
}

fn fnv1a_hex(bytes: &[u8]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// Filtra secretos conocidos y valores de variables de entorno sensibles antes de
/// exponer salidas de `shell_exec` al bus, transcripciones `.tr` o snapshots `.bin`.
pub fn scrub_secrets(input: &str) -> String {
    let mut out = input.to_string();

    // 1. Valores reales presentes en variables de entorno sensibles (>= 8 chars).
    for (k, v) in std::env::vars() {
        if v.len() < 8 {
            continue;
        }
        let upper = k.to_ascii_uppercase();
        if upper.contains("KEY")
            || upper.contains("TOKEN")
            || upper.contains("SECRET")
            || upper.contains("PASSWORD")
            || upper.contains("CREDENTIAL")
        {
            out = out.replace(&v, "[REDACTED]");
        }
    }

    // 2. Patrones comunes de tokens/claves literales en salida.
    for prefix in ["AIza", "sk-", "ghp_", "github_pat_", "xoxb-", "xoxp-"] {
        out = redact_token_prefix(&out, prefix, 16);
    }
    out = redact_bearer_tokens(&out);
    out
}

fn redact_token_prefix(input: &str, prefix: &str, min_tail: usize) -> String {
    let mut out = String::with_capacity(input.len());
    let mut idx = 0usize;
    while let Some(rel) = input[idx..].find(prefix) {
        let start = idx + rel;
        out.push_str(&input[idx..start]);
        let after_prefix = start + prefix.len();
        let tail_len = input[after_prefix..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
            .map(char::len_utf8)
            .sum::<usize>();
        if tail_len >= min_tail {
            out.push_str("[REDACTED]");
            idx = after_prefix + tail_len;
        } else {
            out.push_str(prefix);
            idx = after_prefix;
        }
    }
    out.push_str(&input[idx..]);
    out
}

fn redact_bearer_tokens(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut idx = 0usize;
    while let Some(rel) = input[idx..].find("Bearer ") {
        let start = idx + rel;
        out.push_str(&input[idx..start]);
        let token_start = start + "Bearer ".len();
        let token_len = input[token_start..]
            .chars()
            .take_while(|c| !c.is_whitespace() && *c != '"' && *c != '\'')
            .map(char::len_utf8)
            .sum::<usize>();
        if token_len >= 8 {
            out.push_str("Bearer [REDACTED]");
            idx = token_start + token_len;
        } else {
            out.push_str("Bearer ");
            idx = token_start;
        }
    }
    out.push_str(&input[idx..]);
    out
}

#[cfg(test)]
mod isolated_tests {
    use super::*;

    #[test]
    fn spawn_agent_schema_declares_isolated_boolean() {
        let schema = WorkspaceTools::subagent_schema();
        let props = &schema.parameters["properties"];
        assert!(props.get("isolated").is_some(), "missing `isolated`");
        assert_eq!(props["isolated"]["type"], "BOOLEAN");
        assert!(
            props["isolated"]["description"]
                .as_str()
                .unwrap()
                .contains("isolated Git worktree")
        );
        // `isolated` no es requerido: sigue siendo `goal` el único campo obligatorio.
        let required = schema.parameters["required"].as_array().unwrap();
        assert_eq!(required, &vec![json!("goal")]);
    }

    #[test]
    fn spawn_agent_is_isolated_parses() {
        assert!(!spawn_agent_is_isolated(""));
        assert!(!spawn_agent_is_isolated("{}"));
        assert!(!spawn_agent_is_isolated(r#"{"goal":"x"}"#));
        assert!(!spawn_agent_is_isolated(r#"{"goal":"x","isolated":false}"#));
        assert!(spawn_agent_is_isolated(r#"{"goal":"x","isolated":true}"#));
        assert!(!spawn_agent_is_isolated(
            r#"{"goal":"x","isolated":"true"}"#
        ));
        assert!(!spawn_agent_is_isolated(r#"{"goal":"x","isolated":1}"#));
        assert!(!spawn_agent_is_isolated("not json"));
        assert!(!spawn_agent_is_isolated(r#"{"goal":"#));
    }
}
