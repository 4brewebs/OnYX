use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
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

/// Ejecutor de herramientas de código sujeto a `WorkspacePolicy`.
#[derive(Clone, Debug)]
pub struct WorkspaceTools {
    policy: WorkspacePolicy,
}

impl WorkspaceTools {
    pub fn new(policy: WorkspacePolicy) -> Self {
        Self { policy }
    }

    pub fn policy(&self) -> &WorkspacePolicy {
        &self.policy
    }

    pub fn all_tool_names() -> &'static [&'static str] {
        &[
            "fs_read",
            "fs_list",
            "fs_write",
            "fs_edit",
            "code_search",
            "shell_exec",
        ]
    }

    pub fn approval_required_names() -> &'static [&'static str] {
        &["fs_write", "fs_edit", "shell_exec"]
    }

    pub fn schemas() -> Vec<ToolSchema> {
        vec![
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
        ]
    }

    pub fn execute(&self, tool: &str, args_json: &str) -> Result<String, String> {
        let raw = if args_json.trim().is_empty() {
            "{}"
        } else {
            args_json
        };
        match tool {
            "fs_read" => {
                let args: FsReadArgs =
                    serde_json::from_str(raw).map_err(|e| format!("invalid fs_read args: {e}"))?;
                self.fs_read(args)
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
            _ => Err(format!("unknown workspace tool `{tool}`")),
        }
    }

    fn fs_read(&self, args: FsReadArgs) -> Result<String, String> {
        let path = self.policy.resolve_existing(&args.path)?;
        let content = fs::read_to_string(&path)
            .map_err(|e| format!("failed to read `{}`: {e}", args.path))?;
        let lines: Vec<&str> = content.lines().collect();
        let total = lines.len();
        let start = args.start_line.unwrap_or(1).max(1);
        let end = args.end_line.unwrap_or(total).min(total);
        if start > total && total > 0 {
            return Ok(format!(
                "(file has {total} lines; start_line {start} is past EOF)"
            ));
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
            let rel = path
                .strip_prefix(&self.policy.root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            if path.is_dir() {
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
        let count = content.matches(&args.old_text).count();
        if count == 0 {
            return Err(format!("old_text not found in `{}`", args.path));
        }
        if count > 1 {
            return Err(format!(
                "old_text matched {count} times in `{}`; provide more context",
                args.path
            ));
        }
        let updated = content.replacen(&args.old_text, &args.new_text, 1);
        fs::write(&path, updated.as_bytes())
            .map_err(|e| format!("failed to write `{}`: {e}", args.path))?;
        Ok(format!("edited `{}` (1 replacement)", args.path))
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
        if path.is_file() {
            let Ok(meta) = fs::metadata(path) else {
                return;
            };
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

        let mut child = cmd
            .spawn()
            .map_err(|e| format!("failed to spawn shell command: {e}"))?;
        let deadline = Instant::now() + self.policy.command_timeout;

        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {
                    if Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
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

        let mut stdout = String::new();
        if let Some(mut out) = child.stdout.take() {
            let mut buf = Vec::new();
            let _ = out.read_to_end(&mut buf);
            stdout = String::from_utf8_lossy(&buf).into_owned();
        }
        let mut stderr = String::new();
        if let Some(mut err) = child.stderr.take() {
            let mut buf = Vec::new();
            let _ = err.read_to_end(&mut buf);
            stderr = String::from_utf8_lossy(&buf).into_owned();
        }

        let combined = format!(
            "exit_code: {}\nstdout:\n{}\nstderr:\n{}",
            status.code().unwrap_or(-1),
            stdout,
            stderr
        );
        Ok(truncate_utf8(&combined, self.policy.max_output_bytes))
    }
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
