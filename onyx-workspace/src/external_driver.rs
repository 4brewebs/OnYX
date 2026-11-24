//! Registro configurable de harnesses CLI externos (`SDD-10` Fase 1).
//!
//! Parser *hand-rolled* de la sección `[[external_drivers]]` de `.onyx/config.toml` (misma
//! técnica que `[permissions]` en [`crate::policy`]): soporta múltiples bloques repetidos
//! con escalares y arrays de strings (incluso multilínea). **0 dependencias nuevas.**
//!
//! Fusión `replace-by-id` con precedencia `preset < global (~/.onyx/config.toml) <
//! workspace (.onyx/config.toml)`, y los bloques del workspace se ignoran por completo
//! cuando el proyecto no está confiado (gate de [`crate::trust`]).

use std::fs;
use std::path::Path;
use std::time::Duration;

use crate::policy::{
    array_brackets_closed_pub, parse_toml_string_array_pub, strip_toml_comment_pub,
};
use crate::worktree::{ExternalDriverConfig, PromptTransport, SubagentNetworkPolicy};

const DEFAULT_EXTERNAL_DRIVER_TIMEOUT_SECS: u64 = 600;

/// Presets integrados de serie: `opencode` (bin `opencode`, args `["run"]`, `stdin`, 600 s).
/// El TOML los sobreescribe por `id` completo o añade drivers adicionales.
pub fn builtin_external_driver_presets() -> Vec<ExternalDriverConfig> {
    vec![ExternalDriverConfig {
        id: "opencode".to_string(),
        bin: "opencode".to_string(),
        args: vec!["run".to_string()],
        prompt_transport: PromptTransport::Stdin,
        prompt_flag: None,
        timeout: Duration::from_secs(600),
        allowed_env: Vec::new(),
        inherit_home: false,
        network_policy: SubagentNetworkPolicy::ExternalHarnessAllowlist(Vec::new()),
    }]
}

/// Bloque `[[external_drivers]]` acumulado durante el parseo (campos opcionales aún por llegar).
#[derive(Default)]
struct PartialDriver {
    id: Option<String>,
    bin: Option<String>,
    args: Option<Vec<String>>,
    prompt_transport: Option<PromptTransport>,
    /// `None` = clave aún no vista; `Some(None)` = `prompt_flag = null` explícito.
    prompt_flag: Option<Option<String>>,
    timeout_secs: Option<u64>,
    allowed_env: Option<Vec<String>>,
    inherit_home: Option<bool>,
    network_allowlist: Option<Vec<String>>,
}

impl PartialDriver {
    /// Valida el bloque completo y produce la configuración efectiva con defaults.
    fn finalize(self, line_no: usize) -> Result<ExternalDriverConfig, String> {
        let id = self.id.ok_or_else(|| {
            format!(
                "config.toml line {line_no}: `[[external_drivers]]` block missing required `id`"
            )
        })?;
        if id.is_empty()
            || !id
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
        {
            return Err(format!(
                "config.toml line {line_no}: driver `id` must be non-empty with charset `[a-z0-9_-]`, got `{id}`"
            ));
        }
        // `native` es el identificador reservado del motor interno (siempre disponible y
        // nunca configurable): rechazarlo en el parseo evita sombras en silencio.
        if id == "native" {
            return Err(format!(
                "config.toml line {line_no}: driver `id` `native` is reserved for the built-in OnYX engine and cannot be redefined"
            ));
        }
        let bin = self.bin.ok_or_else(|| {
            format!("config.toml line {line_no}: driver `{id}` missing required `bin`")
        })?;
        if bin.trim().is_empty() {
            return Err(format!(
                "config.toml line {line_no}: driver `{id}` `bin` cannot be empty"
            ));
        }
        let timeout_secs = self
            .timeout_secs
            .unwrap_or(DEFAULT_EXTERNAL_DRIVER_TIMEOUT_SECS);
        if timeout_secs == 0 {
            return Err(format!(
                "config.toml line {line_no}: driver `{id}` `timeout_secs` must be > 0"
            ));
        }
        Ok(ExternalDriverConfig {
            id,
            bin: bin.trim().to_string(),
            args: self.args.unwrap_or_default(),
            prompt_transport: self.prompt_transport.unwrap_or(PromptTransport::Stdin),
            prompt_flag: self.prompt_flag.flatten(),
            timeout: Duration::from_secs(timeout_secs),
            allowed_env: self.allowed_env.unwrap_or_default(),
            inherit_home: self.inherit_home.unwrap_or(false),
            network_policy: SubagentNetworkPolicy::ExternalHarnessAllowlist(
                self.network_allowlist.unwrap_or_default(),
            ),
        })
    }
}

/// Parsea la sección `[[external_drivers]]` de un texto TOML.
/// Ignora las demás secciones para convivir limpiamente con `[permissions]`/`[sandbox]`/`[impact]`.
pub fn parse_external_drivers_toml(input: &str) -> Result<Vec<ExternalDriverConfig>, String> {
    let mut out: Vec<ExternalDriverConfig> = Vec::new();
    let text = input.strip_prefix('\u{feff}').unwrap_or(input);
    let lines: Vec<&str> = text.lines().collect();
    let mut current: Option<PartialDriver> = None;
    let mut current_line = 0usize;
    let mut i = 0usize;

    while i < lines.len() {
        let line_no = i + 1;
        let stripped = strip_toml_comment_pub(lines[i]).trim();
        if stripped.is_empty() {
            i += 1;
            continue;
        }
        if stripped.starts_with('[') {
            // Finaliza el bloque anterior (si existía) antes de entrar a otra sección.
            if let Some(block) = current.take() {
                let cfg = block.finalize(current_line)?;
                if out.iter().any(|d| d.id == cfg.id) {
                    return Err(format!(
                        "config.toml line {current_line}: duplicate driver id `{}` in `[[external_drivers]]`",
                        cfg.id
                    ));
                }
                out.push(cfg);
            }
            if stripped.starts_with("[[") && stripped.ends_with("]]") {
                let header = stripped[2..stripped.len() - 2].trim();
                if header == "external_drivers" {
                    current = Some(PartialDriver::default());
                    current_line = line_no;
                }
            }
            i += 1;
            continue;
        }
        let Some(block) = current.as_mut() else {
            i += 1;
            continue;
        };
        let (raw_key, raw_val) = stripped.split_once('=').ok_or_else(|| {
            format!(
                "config.toml line {line_no}: expected `key = value` inside [[external_drivers]]"
            )
        })?;
        let key = raw_key.trim();
        let mut value_buf = raw_val.trim().to_string();

        // Soporta arrays multi-línea `[ ... ]` acumulando hasta cerrar el corchete.
        if value_buf.starts_with('[') {
            while !array_brackets_closed_pub(&value_buf) {
                i += 1;
                if i >= lines.len() {
                    return Err(format!(
                        "config.toml line {line_no}: unterminated array for `{key}`"
                    ));
                }
                let next_piece = strip_toml_comment_pub(lines[i]).trim();
                if !next_piece.is_empty() {
                    value_buf.push(' ');
                    value_buf.push_str(next_piece);
                }
            }
        }

        match key {
            "id" => {
                if block.id.is_some() {
                    return Err(dup_key_err(line_no, key));
                }
                block.id = Some(parse_toml_string(&value_buf)?);
            }
            "bin" => {
                if block.bin.is_some() {
                    return Err(dup_key_err(line_no, key));
                }
                block.bin = Some(parse_toml_string(&value_buf)?);
            }
            "args" => {
                if block.args.is_some() {
                    return Err(dup_key_err(line_no, key));
                }
                block.args = Some(parse_string_array(&value_buf, line_no, key)?);
            }
            "prompt_transport" => {
                if block.prompt_transport.is_some() {
                    return Err(dup_key_err(line_no, key));
                }
                let raw = parse_toml_string(&value_buf)?;
                block.prompt_transport = Some(
                    PromptTransport::parse(&raw).ok_or_else(|| {
                        format!(
                            "config.toml line {line_no}: `prompt_transport` must be `\"stdin\"`, `\"arg\"` or `\"temp_file\"`, got `{raw}`"
                        )
                    })?,
                );
            }
            "prompt_flag" => {
                if block.prompt_flag.is_some() {
                    return Err(dup_key_err(line_no, key));
                }
                if value_buf.trim() == "null" {
                    block.prompt_flag = Some(None);
                } else {
                    block.prompt_flag = Some(Some(parse_toml_string(&value_buf)?));
                }
            }
            "timeout_secs" => {
                if block.timeout_secs.is_some() {
                    return Err(dup_key_err(line_no, key));
                }
                let raw = value_buf.trim();
                block.timeout_secs = Some(
                    raw.parse::<u64>().map_err(|_| {
                        format!(
                            "config.toml line {line_no}: `timeout_secs` must be a positive integer, got `{raw}`"
                        )
                    })?,
                );
            }
            "allowed_env" => {
                if block.allowed_env.is_some() {
                    return Err(dup_key_err(line_no, key));
                }
                block.allowed_env = Some(parse_string_array(&value_buf, line_no, key)?);
            }
            "inherit_home" => {
                if block.inherit_home.is_some() {
                    return Err(dup_key_err(line_no, key));
                }
                block.inherit_home = Some(parse_toml_bool(&value_buf)?);
            }
            "network_allowlist" => {
                if block.network_allowlist.is_some() {
                    return Err(dup_key_err(line_no, key));
                }
                block.network_allowlist = Some(parse_string_array(&value_buf, line_no, key)?);
            }
            other => {
                return Err(format!(
                    "config.toml line {line_no}: unknown key `{other}` in [[external_drivers]] (expected `id`, `bin`, `args`, `prompt_transport`, `prompt_flag`, `timeout_secs`, `allowed_env`, `inherit_home`, or `network_allowlist`)"
                ));
            }
        }
        i += 1;
    }

    if let Some(block) = current {
        let cfg = block.finalize(current_line)?;
        if out.iter().any(|d| d.id == cfg.id) {
            return Err(format!(
                "config.toml line {current_line}: duplicate driver id `{}` in `[[external_drivers]]`",
                cfg.id
            ));
        }
        out.push(cfg);
    }

    Ok(out)
}

fn dup_key_err(line_no: usize, key: &str) -> String {
    format!("config.toml line {line_no}: duplicate key `{key}` in [[external_drivers]] block")
}

fn parse_string_array(value_buf: &str, line_no: usize, key: &str) -> Result<Vec<String>, String> {
    parse_toml_string_array_pub(value_buf)
        .map_err(|e| format!("config.toml line {line_no} (`{key}`): {e}"))
}

fn parse_toml_bool(raw: &str) -> Result<bool, String> {
    match raw.trim() {
        "true" => Ok(true),
        "false" => Ok(false),
        other => Err(format!("expected boolean `true`/`false`, found `{other}`")),
    }
}

fn parse_toml_string(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    if let Some(inner) = trimmed
        .strip_prefix('\'')
        .and_then(|s| s.strip_suffix('\''))
    {
        return Ok(inner.to_string());
    }
    if let Some(inner) = trimmed.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
        let mut out = String::with_capacity(inner.len());
        let mut chars = inner.chars();
        while let Some(c) = chars.next() {
            if c == '\\' {
                match chars.next() {
                    Some('"') => out.push('"'),
                    Some('\\') => out.push('\\'),
                    Some('n') => out.push('\n'),
                    Some('r') => out.push('\r'),
                    Some('t') => out.push('\t'),
                    Some(other) => {
                        out.push('\\');
                        out.push(other);
                    }
                    None => return Err("unterminated escape sequence".to_string()),
                }
            } else {
                out.push(c);
            }
        }
        return Ok(out);
    }
    Err(format!("expected quoted string, found `{trimmed}`"))
}

/// Fusión `replace-by-id` con precedencia `preset < global < workspace (trusted)`.
/// Si `trust_project == false`, los bloques del workspace se ignoran por completo (sin merge
/// parcial) y se devuelve cuántos se descartaron (para la advertencia en `doctor`/banners).
pub fn merge_external_drivers(
    global: Vec<ExternalDriverConfig>,
    project: Option<Vec<ExternalDriverConfig>>,
    trust_project: bool,
) -> (Vec<ExternalDriverConfig>, usize) {
    let mut merged = builtin_external_driver_presets();
    let apply = |list: Vec<ExternalDriverConfig>, merged: &mut Vec<ExternalDriverConfig>| {
        for incoming in list {
            match merged.iter_mut().find(|d| d.id == incoming.id) {
                Some(slot) => *slot = incoming, // reemplazo completo por `id`
                None => merged.push(incoming),
            }
        }
    };
    apply(global, &mut merged);
    let ignored = if trust_project {
        if let Some(list) = project {
            apply(list, &mut merged);
        }
        0
    } else {
        project.map(|l| l.len()).unwrap_or(0)
    };
    (merged, ignored)
}

/// Carga y fusiona `[[external_drivers]]` desde `~/.onyx/config.toml` (global) y
/// `<workspace>/.onyx/config.toml` aplicando el gate de Workspace Trust
/// (mismo patrón que `PermissionRules::load_from_paths_with_trust`).
pub fn load_external_drivers(
    global_path: Option<&Path>,
    project_path: Option<&Path>,
    trust_project: bool,
) -> Result<(Vec<ExternalDriverConfig>, Vec<String>), String> {
    let mut warnings = Vec::new();
    let mut global = Vec::new();
    if let Some(gp) = global_path {
        if gp.is_file() {
            let raw = fs::read_to_string(gp)
                .map_err(|e| format!("failed to read `{}`: {e}", gp.display()))?;
            global = parse_external_drivers_toml(&raw)?;
        }
    }
    let project = if let Some(pp) = project_path {
        if pp.is_file() {
            let raw = fs::read_to_string(pp)
                .map_err(|e| format!("failed to read `{}`: {e}", pp.display()))?;
            Some(parse_external_drivers_toml(&raw)?)
        } else {
            None
        }
    } else {
        None
    };
    let (merged, ignored) = merge_external_drivers(global, project, trust_project);
    if ignored > 0 {
        warnings.push(format!(
            "untrusted project `.onyx/config.toml`: ignored {ignored} `[[external_drivers]]` block(s); run `openonyx trust` or `/trust` to approve"
        ));
    }
    Ok((merged, warnings))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
# otras secciones conviven sin problema
[permissions]
allow = ["fs_read(*)"]

[[external_drivers]]
id = "opencode"
bin = "opencode"
args = ["run", "--model", "qwen38-27b"]
prompt_transport = "stdin"
timeout_secs = 900
allowed_env = ["OPENCODE_API_KEY",
    "OPENAI_BASE_URL"
]
inherit_home = true
network_allowlist = ["127.0.0.1",
    "api.openai.com"]

[[external_drivers]]
id = "claude-x"
bin = "claude"
args = ["-p"]
prompt_transport = "arg"
prompt_flag = null

[impact]
enabled = true
"#;

    #[test]
    fn parser_covers_repeated_blocks_scalars_and_multiline_arrays() {
        let drivers = parse_external_drivers_toml(SAMPLE).unwrap();
        assert_eq!(drivers.len(), 2);

        let oc = &drivers[0];
        assert_eq!(oc.id, "opencode");
        assert_eq!(oc.bin, "opencode");
        assert_eq!(oc.args, vec!["run", "--model", "qwen38-27b"]);
        assert_eq!(oc.prompt_transport, PromptTransport::Stdin);
        assert_eq!(oc.timeout, Duration::from_secs(900));
        assert_eq!(oc.allowed_env, vec!["OPENCODE_API_KEY", "OPENAI_BASE_URL"]);
        assert!(oc.inherit_home);
        assert_eq!(
            oc.network_policy,
            SubagentNetworkPolicy::ExternalHarnessAllowlist(vec![
                "127.0.0.1".to_string(),
                "api.openai.com".to_string()
            ])
        );

        let cx = &drivers[1];
        assert_eq!(cx.id, "claude-x");
        assert_eq!(cx.prompt_transport, PromptTransport::Arg);
        assert_eq!(cx.prompt_flag, None);
        assert_eq!(cx.timeout, Duration::from_secs(600));
        assert_eq!(cx.args, vec!["-p"]);
        assert_eq!(cx.allowed_env, Vec::<String>::new());
        assert!(!cx.inherit_home);
    }

    #[test]
    fn parser_rejects_invalid_blocks() {
        let bad_id =
            parse_external_drivers_toml("[[external_drivers]]\nid = \"Bad ID\"\nbin = \"x\"\n")
                .unwrap_err();
        assert!(bad_id.contains("charset"), "{bad_id}");

        let empty_bin =
            parse_external_drivers_toml("[[external_drivers]]\nid = \"ok\"\nbin = \"   \"\n")
                .unwrap_err();
        assert!(empty_bin.contains("bin"), "{empty_bin}");

        let zero_timeout = parse_external_drivers_toml(
            "[[external_drivers]]\nid = \"ok\"\nbin = \"x\"\ntimeout_secs = 0\n",
        )
        .unwrap_err();
        assert!(zero_timeout.contains("timeout_secs"), "{zero_timeout}");

        let unknown_key = parse_external_drivers_toml(
            "[[external_drivers]]\nid = \"ok\"\nbin = \"x\"\nfoo = 1\n",
        )
        .unwrap_err();
        assert!(unknown_key.contains("unknown key"), "{unknown_key}");

        let reserved_native =
            parse_external_drivers_toml("[[external_drivers]]\nid = \"native\"\nbin = \"x\"\n")
                .unwrap_err();
        assert!(reserved_native.contains("reserved"), "{reserved_native}");

        let missing_bin =
            parse_external_drivers_toml("[[external_drivers]]\nid = \"ok\"\n").unwrap_err();
        assert!(
            missing_bin.contains("missing required `bin`"),
            "{missing_bin}"
        );

        let bad_transport = parse_external_drivers_toml(
            "[[external_drivers]]\nid = \"ok\"\nbin = \"x\"\nprompt_transport = \"carrier-pigeon\"\n",
        )
        .unwrap_err();
        assert!(
            bad_transport.contains("prompt_transport"),
            "{bad_transport}"
        );

        let dup_id = parse_external_drivers_toml(
            "[[external_drivers]]\nid = \"a\"\nbin = \"x\"\n[[external_drivers]]\nid = \"a\"\nbin = \"y\"\n",
        )
        .unwrap_err();
        assert!(dup_id.contains("duplicate driver id"), "{dup_id}");
    }

    #[test]
    fn merge_is_replace_by_id_with_preset_global_workspace_precedence() {
        let preset = ExternalDriverConfig {
            id: "opencode".to_string(),
            bin: "opencode".to_string(),
            args: vec!["run".to_string()],
            ..first_default()
        };
        assert_eq!(builtin_external_driver_presets(), vec![preset.clone()]);

        let mut global_oc = preset.clone();
        global_oc.timeout = Duration::from_secs(111);
        let mut project_oc = preset.clone();
        project_oc.timeout = Duration::from_secs(222);
        let project_extra = ExternalDriverConfig {
            id: "claude".to_string(),
            bin: "claude".to_string(),
            ..first_default()
        };

        // Trusted: workspace reemplaza el preset por `id` completo y añade drivers.
        let (merged, ignored) = merge_external_drivers(
            vec![global_oc.clone()],
            Some(vec![project_oc.clone(), project_extra.clone()]),
            true,
        );
        assert_eq!(ignored, 0);
        let oc = merged.iter().find(|d| d.id == "opencode").unwrap();
        assert_eq!(
            oc.timeout,
            Duration::from_secs(222),
            "workspace > global > preset"
        );
        assert!(merged.iter().any(|d| d.id == "claude"));

        // Untrusted: los bloques del workspace se ignoran por completo.
        let (merged, ignored) = merge_external_drivers(
            vec![global_oc.clone()],
            Some(vec![project_oc.clone(), project_extra.clone()]),
            false,
        );
        assert_eq!(ignored, 2);
        let oc = merged.iter().find(|d| d.id == "opencode").unwrap();
        assert_eq!(
            oc.timeout,
            Duration::from_secs(111),
            "global > preset when untrusted"
        );
        assert!(!merged.iter().any(|d| d.id == "claude"));

        // Sin global: el preset sobrevive.
        let (merged, _) = merge_external_drivers(Vec::new(), None, true);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].id, "opencode");
        assert_eq!(merged[0].args, vec!["run".to_string()]);
    }

    fn first_default() -> ExternalDriverConfig {
        ExternalDriverConfig {
            id: String::new(),
            bin: String::new(),
            args: Vec::new(),
            prompt_transport: PromptTransport::Stdin,
            prompt_flag: None,
            timeout: Duration::from_secs(600),
            allowed_env: Vec::new(),
            inherit_home: false,
            network_policy: SubagentNetworkPolicy::ExternalHarnessAllowlist(Vec::new()),
        }
    }
}
