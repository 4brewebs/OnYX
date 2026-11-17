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
                description: "Replace an exact unique substring in an existing file.",
                parameters: json!({
                    "type": "OBJECT",
                    "properties": {
                        "path": { "type": "STRING", "description": "Relative path inside the workspace." },
                        "old_text": { "type": "STRING", "description": "Exact text to replace (must occur once)." },
                        "new_text": { "type": "STRING", "description": "Replacement text." }
                    },
                    "required": ["path", "old_text", "new_text"]
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
                        "cursor": { "type": "INTEGER", "description": "Optional 0-based row cursor for pagination." },
                        "max_rows": { "type": "INTEGER", "description": "Optional maximum TOON rows per page (default 120)." }
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
                        "cursor": { "type": "INTEGER", "description": "Optional 0-based row cursor for pagination." },
                        "max_rows": { "type": "INTEGER", "description": "Optional maximum TOON rows per page (default 120)." }
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
        let path = self.policy.resolve_existing(&args.path)?;
        let content = fs::read_to_string(&path)
            .map_err(|e| format!("failed to read `{}`: {e}", args.path))?;

        let exact_count = content.matches(&args.old_text).count();
        if exact_count == 1 {
            let updated = content.replacen(&args.old_text, &args.new_text, 1);
            fs::write(&path, updated.as_bytes())
                .map_err(|e| format!("failed to write `{}`: {e}", args.path))?;
            self.invalidate_seen_file(&path);
            return Ok(format!("edited `{}` (1 replacement)", args.path));
        }
        if exact_count > 1 {
            return Err(format!(
                "old_text matched {exact_count} times in `{}`; provide more context",
                args.path
            ));
        }

        let updated = replace_line_trimmed_unique(&content, &args.old_text, &args.new_text)
            .map_err(|reason| format!("in `{}`: {reason}", args.path))?;
        fs::write(&path, updated.as_bytes())
            .map_err(|e| format!("failed to write `{}`: {e}", args.path))?;
        self.invalidate_seen_file(&path);
        Ok(format!(
            "edited `{}` (1 line-trimmed replacement)",
            args.path
        ))
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

    fn shell_exec(&self, args: ShellExecArgs) -> Result<String, String> {
        if args.command.trim().is_empty() {
            return Err("command cannot be empty".to_string());
        }
        let mut cmd = if cfg!(windows) {
            let mut c = Command::new("pwsh");
            c.args(["-NoProfile", "-Command", &args.command]);
            c
        } else {
            let mut c = Command::new("sh");
            c.args(["-c", &args.command]);
            c
        };
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
            .map_err(|e| format!("failed to spawn shell command: {e}"))?;
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
                            "command timed out after {:?}",
                            self.policy.command_timeout
                        ));
                    }
                    thread::sleep(Duration::from_millis(20));
                }
                Err(e) => return Err(format!("wait failed: {e}")),
            }
        };

        let stdout = out_handle.join().unwrap_or_default();
        let stderr = err_handle.join().unwrap_or_default();

        let combined = scrub_secrets(&format!(
            "exit_code: {}\nstdout:\n{}\nstderr:\n{}",
            status.code().unwrap_or(-1),
            stdout,
            stderr
        ));
        Ok(truncate_utf8(&combined, self.policy.max_output_bytes))
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
            max_rows: args.max_rows.unwrap_or(onyx_scent::DEFAULT_MAX_ROWS),
            public_only: args.public_only.unwrap_or(false),
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
            max_rows: args.max_rows.unwrap_or(onyx_scent::DEFAULT_MAX_ROWS),
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

#[derive(Deserialize)]
struct FsEditArgs {
    path: String,
    old_text: String,
    new_text: String,
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

#[cfg(feature = "scent")]
#[derive(Deserialize)]
struct ScentOutlineArgs {
    path: Option<String>,
    public_only: Option<bool>,
    cursor: Option<usize>,
    max_rows: Option<usize>,
}

#[cfg(feature = "scent")]
#[derive(Deserialize)]
struct ScentImpactArgs {
    target: String,
    max_hops: Option<u8>,
    cursor: Option<usize>,
    max_rows: Option<usize>,
}

fn replace_line_trimmed_unique(
    content: &str,
    old_text: &str,
    new_text: &str,
) -> Result<String, String> {
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
        1 => {
            let (byte_start, byte_end) = matches[0];
            let mut out = String::with_capacity(content.len() + new_text.len());
            out.push_str(&content[..byte_start]);
            out.push_str(new_text);
            if content[..byte_end].ends_with('\n') && !new_text.ends_with('\n') {
                if content[..byte_end].ends_with("\r\n") {
                    out.push_str("\r\n");
                } else {
                    out.push('\n');
                }
            }
            out.push_str(&content[byte_end..]);
            Ok(out)
        }
        n => Err(format!(
            "old_text matched {n} times (line-trimmed); provide more context"
        )),
    }
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
