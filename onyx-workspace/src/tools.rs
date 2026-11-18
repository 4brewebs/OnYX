use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{Value as JsonValue, json};

use crate::policy::WorkspacePolicy;

/// Descripción declarativa de una herramienta del workspace para inyectar en LLMs.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolSchema {
    pub name: &'static str,
    pub description: &'static str,
    pub parameters: JsonValue,
    pub requires_approval: bool,
}

type SeenReadsMap = Arc<Mutex<BTreeMap<(String, usize, usize), (String, u64)>>>;

/// Ejecutor de herramientas de código sujeto a `WorkspacePolicy`.
#[derive(Clone, Debug)]
pub struct WorkspaceTools {
    policy: WorkspacePolicy,
    seen_reads: SeenReadsMap,
    read_seq: Arc<AtomicU64>,
    #[cfg(feature = "scent")]
    scent_index: Arc<Mutex<onyx_scent::WorkspaceIndex>>,
}

impl WorkspaceTools {
    pub fn new(policy: WorkspacePolicy) -> Self {
        Self {
            policy,
            seen_reads: Arc::new(Mutex::new(BTreeMap::new())),
            read_seq: Arc::new(AtomicU64::new(1)),
            #[cfg(feature = "scent")]
            scent_index: Arc::new(Mutex::new(onyx_scent::WorkspaceIndex::new())),
        }
    }

    pub fn policy(&self) -> &WorkspacePolicy {
        &self.policy
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
                "git_status",
                "git_diff",
                "load_skill",
                "plan_update",
                "shell_exec",
                "scent_outline",
                "scent_impact",
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
                "git_status",
                "git_diff",
                "load_skill",
                "plan_update",
                "shell_exec",
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
                "git_status",
                "git_diff",
                "load_skill",
                "plan_update",
                "shell_exec",
                "scent_outline",
                "scent_impact",
                "spawn_agent",
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
                "git_status",
                "git_diff",
                "load_skill",
                "plan_update",
                "shell_exec",
                "spawn_agent",
            ]
        }
    }

    pub fn approval_required_names() -> &'static [&'static str] {
        &["fs_write", "fs_edit", "shell_exec"]
    }

    pub fn subagent_schema() -> ToolSchema {
        ToolSchema {
            name: "spawn_agent",
            description: "Delegate a scoped subtask to a child subagent and suspend until the child completes.",
            parameters: json!({
                "type": "OBJECT",
                "properties": {
                    "name": { "type": "STRING", "description": "Optional short label for the subagent." },
                    "goal": { "type": "STRING", "description": "Clear, bounded goal for the child subagent." }
                },
                "required": ["goal"]
            }),
            requires_approval: false,
        }
    }

    pub fn schemas_with_subagents(include_subagents: bool) -> Vec<ToolSchema> {
        let mut out = Self::schemas();
        if include_subagents {
            out.push(Self::subagent_schema());
        }
        out
    }

    pub fn schemas() -> Vec<ToolSchema> {
        let mut out = vec![
            ToolSchema {
                name: "fs_read",
                description: "Read a UTF-8 file from the workspace, optionally by 1-based inclusive line range.",
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
                name: "fs_list",
                description: "List files and directories inside the workspace, skipping ignored directories.",
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
                name: "fs_write",
                description: "Create or overwrite a UTF-8 file in the workspace.",
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
                name: "fs_edit",
                description: "Replace one unique substring (`old_text`/`new_text`) or multiple sequential hunks atomically (`edits`) in an existing file.",
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
                name: "code_search",
                description: "Search for a text pattern across workspace files.",
                parameters: json!({
                    "type": "OBJECT",
                    "properties": {
                        "pattern": { "type": "STRING", "description": "Substring to search for." },
                        "path": { "type": "STRING", "description": "Optional relative directory or file (defaults to '.')." },
                        "max_results": { "type": "INTEGER", "description": "Maximum matching lines to return (default 50)." }
                    },
                    "required": ["pattern"]
                }),
                requires_approval: false,
            },
            ToolSchema {
                name: "git_status",
                description: "Show concise repository status (`git status --short --branch`) without requiring approval.",
                parameters: json!({
                    "type": "OBJECT",
                    "properties": {}
                }),
                requires_approval: false,
            },
            ToolSchema {
                name: "git_diff",
                description: "Inspect unified git diff (`git diff --no-color`) for the working tree or a relative path without requiring approval.",
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
                name: "load_skill",
                description: "Load an on-demand protocol or stack guide (e.g. `scent`, `rust`, or workspace SKILL.md) without requiring approval.",
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
                name: "plan_update",
                description: "Publish or update a structured multi-step progress checklist (`[x]`, `[>]`, `[ ]`) rendered in the TUI without requiring approval.",
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
                name: "shell_exec",
                description: "Execute a shell command in the workspace root directory.",
                parameters: json!({
                    "type": "OBJECT",
                    "properties": {
                        "command": { "type": "STRING", "description": "Shell command to execute." }
                    },
                    "required": ["command"]
                }),
                requires_approval: true,
            },
        ];
        #[cfg(feature = "scent")]
        {
            out.push(ToolSchema {
                name: "scent_outline",
                description: "Return compact @TOON v1 AST outlines (public signatures and 1-based line ranges) for Rust, Python, or Java files.",
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
                name: "scent_impact",
                description: "Trace semantic AST call impact around a target symbol as ranked @TOON v1 tuples (epicenter, callers, callees, line ranges).",
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
        let content = fs::read_to_string(&path)
            .map_err(|e| format!("failed to read `{}`: {e}", args.path))?;
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
        if let Ok(mut guard) = self.seen_reads.lock() {
            if let Some((prev_digest, prev_tick)) = guard.get(&key) {
                if prev_digest == &digest {
                    let tick_label = tick_hint.unwrap_or(*prev_tick);
                    return Ok(format!(
                        "@TOON v1\n# cols: subject | predicate | object | meta\n{rel} | unchanged_since_tick:{tick_label} | lines:{start}-{end} | digest:{digest}\n# total: 1\n"
                    ));
                }
            }
            let tick_val =
                tick_hint.unwrap_or_else(|| self.read_seq.fetch_add(1, Ordering::Relaxed));
            guard.insert(key, (digest, tick_val));
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
        let path = self.policy.resolve_for_write(&args.path)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("failed to create parent dir: {e}"))?;
        }
        fs::write(&path, args.content.as_bytes())
            .map_err(|e| format!("failed to write `{}`: {e}", args.path))?;
        self.invalidate_seen_file(&path);
        Ok(format!(
            "wrote {} bytes to {}",
            args.content.len(),
            args.path
        ))
    }

    fn fs_edit(&self, args: FsEditArgs) -> Result<String, String> {
        let hunks = extract_edit_hunks(&args)?;
        let path = self.policy.resolve_existing(&args.path)?;
        let content = fs::read_to_string(&path)
            .map_err(|e| format!("failed to read `{}`: {e}", args.path))?;

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
        if trimmed_count > 0 {
            Ok(format!(
                "edited `{}` ({} replacement(s), {} line-trimmed)",
                args.path,
                hunks.len(),
                trimmed_count
            ))
        } else {
            Ok(format!(
                "edited `{}` ({} replacement(s))",
                args.path,
                hunks.len()
            ))
        }
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
        let max_results = args.max_results.unwrap_or(50).min(200);
        let mut hits = Vec::new();
        self.walk_search(&start, &args.pattern, max_results, &mut hits);
        if hits.is_empty() {
            Ok("no matches".to_string())
        } else {
            Ok(truncate_utf8(
                &hits.join("\n"),
                self.policy.max_output_bytes,
            ))
        }
    }

    fn walk_search(&self, path: &Path, pattern: &str, max_results: usize, hits: &mut Vec<String>) {
        if hits.len() >= max_results || self.policy.is_ignored(path) {
            return;
        }
        let Ok(meta) = fs::symlink_metadata(path) else {
            return;
        };
        if meta.file_type().is_symlink() {
            return;
        }
        if meta.is_file() {
            if meta.len() > 1024 * 1024 {
                return;
            }
            let Ok(text) = fs::read_to_string(path) else {
                return;
            };
            let rel = path
                .strip_prefix(&self.policy.root)
                .unwrap_or(path)
                .to_string_lossy()
                .replace('\\', "/");
            for (idx, line) in text.lines().enumerate() {
                if line.contains(pattern) {
                    hits.push(format!("{rel}:{}: {line}", idx + 1));
                    if hits.len() >= max_results {
                        break;
                    }
                }
            }
            return;
        }
        if let Ok(read_dir) = fs::read_dir(path) {
            let mut items: Vec<PathBuf> = read_dir.flatten().map(|e| e.path()).collect();
            items.sort();
            for item in items {
                self.walk_search(&item, pattern, max_results, hits);
                if hits.len() >= max_results {
                    break;
                }
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
        let cmd = if cfg!(windows) {
            let mut c = Command::new("pwsh");
            c.args(["-NoProfile", "-Command", &args.command]);
            c
        } else {
            let mut c = Command::new("sh");
            c.args(["-c", &args.command]);
            c
        };
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
        cmd.current_dir(&self.policy.root)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }

        let mut child = cmd
            .spawn()
            .map_err(|e| format!("failed to spawn {label}: {e}"))?;
        let out_handle = read_pipe_capped(child.stdout.take(), self.policy.max_output_bytes);
        let err_handle = read_pipe_capped(child.stderr.take(), self.policy.max_output_bytes);
        let deadline = Instant::now() + self.policy.command_timeout;

        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {
                    if Instant::now() >= deadline {
                        #[cfg(windows)]
                        {
                            let _ = Command::new("taskkill")
                                .args(["/F", "/T", "/PID", &child.id().to_string()])
                                .stdout(Stdio::null())
                                .stderr(Stdio::null())
                                .status();
                        }
                        #[cfg(unix)]
                        {
                            let _ = Command::new("kill")
                                .args(["-9", &format!("-{}", child.id())])
                                .stdout(Stdio::null())
                                .stderr(Stdio::null())
                                .status();
                        }
                        let _ = child.kill();
                        let _ = child.wait();
                        let _ = out_handle.join();
                        let _ = err_handle.join();
                        return Err(format!(
                            "{label} timed out after {:?}",
                            self.policy.command_timeout
                        ));
                    }
                    thread::sleep(Duration::from_millis(20));
                }
                Err(e) => return Err(format!("wait failed: {e}")),
            }
        };

        let stdout = scrub_secrets(&out_handle.join().unwrap_or_default());
        let stderr = scrub_secrets(&err_handle.join().unwrap_or_default());
        Ok((status.code().unwrap_or(-1), stdout, stderr))
    }

    #[cfg(feature = "scent")]
    fn scent_outline(&self, args: ScentOutlineArgs) -> Result<String, String> {
        let base = args.path.as_deref().unwrap_or(".");
        let resolved = self.policy.resolve_existing(base)?;
        let mut idx = self
            .scent_index
            .lock()
            .map_err(|_| "scent index lock poisoned".to_string())?;
        idx.index_target(&self.policy.root, Some(&resolved))
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
        Ok(truncate_utf8(
            &idx.outline_toon(opts),
            self.policy.max_output_bytes,
        ))
    }

    #[cfg(feature = "scent")]
    fn scent_impact(&self, args: ScentImpactArgs) -> Result<String, String> {
        let mut idx = self
            .scent_index
            .lock()
            .map_err(|_| "scent index lock poisoned".to_string())?;
        idx.index_workspace(&self.policy.root)
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
}

fn read_pipe_capped<R: Read + Send + 'static>(
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

fn truncate_utf8(s: &str, max_bytes: usize) -> String {
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

#[derive(Deserialize)]
struct CodeSearchArgs {
    pattern: String,
    path: Option<String>,
    max_results: Option<usize>,
}

#[derive(Deserialize)]
struct ShellExecArgs {
    command: String,
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
