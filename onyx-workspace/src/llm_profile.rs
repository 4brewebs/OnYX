//! Perfiles LLM configurables (`[[llm_profiles]]`) para cambio en caliente de
//! proveedor, modelo y esfuerzo de razonamiento.
//!
//! Parser *hand-rolled* de la sección `[[llm_profiles]]` de `.onyx/config.toml` (misma
//! técnica que `[[external_drivers]]` en [`crate::external_driver`]): soporta múltiples
//! bloques repetidos con escalares y `null`. **0 dependencias nuevas.**
//!
//! Fusión `replace-by-id` con precedencia `global (~/.onyx/config.toml) <
//! workspace (.onyx/config.toml)`; los bloques del workspace se ignoran por completo
//! cuando el proyecto no está confiado (gate de [`crate::trust`]).
//!
//! **Seguridad:** está **prohibido** el `api_key` literal — el parser lo rechaza con un
//! error accionable que indica `api_key_env`. Los valores de credenciales solo se
//! resuelven en el anfitrión (vía la variable de entorno nombrada), nunca se persisten.

use std::fs;
use std::path::Path;

use crate::policy::strip_toml_comment_pub;

/// Perfil LLM declarado en `.onyx/config.toml` (`[[llm_profiles]]`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LlmProfileConfig {
    /// Identificador estable (`[a-z0-9_-]`); `"custom"` está reservado.
    pub id: String,
    /// Proveedor: `"gemini"` | `"openai"`.
    pub provider: String,
    /// Nombre de modelo (no vacío).
    pub model: String,
    /// `"low"` | `"medium"` | `"high"` | `None` (auto).
    pub effort: Option<String>,
    /// Endpoint base opcional (validado en el anfitrión con `validate_api_base`).
    pub api_base: Option<String>,
    /// Nombre de la variable de entorno que contiene la API key (nunca el valor literal).
    pub api_key_env: Option<String>,
}

/// Bloque `[[llm_profiles]]` acumulado durante el parseo (campos opcionales aún por llegar).
#[derive(Default)]
struct PartialProfile {
    id: Option<String>,
    provider: Option<String>,
    model: Option<String>,
    effort: Option<Option<String>>,
    api_base: Option<Option<String>>,
    api_key_env: Option<Option<String>>,
}

impl PartialProfile {
    /// Valida el bloque completo y produce la configuración efectiva con defaults.
    fn finalize(self, line_no: usize) -> Result<LlmProfileConfig, String> {
        let id = self.id.ok_or_else(|| {
            format!("config.toml line {line_no}: `[[llm_profiles]]` block missing required `id`")
        })?;
        if id.is_empty()
            || !id
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
        {
            return Err(format!(
                "config.toml line {line_no}: profile `id` must be non-empty with charset `[a-z0-9_-]`, got `{id}`"
            ));
        }
        // `custom` es el identificador reservado de los cambios ad-hoc de modelo/esfuerzo
        // (marcados en caliente por `LlmProfileHandle`): rechazarlo evita sombras en silencio.
        if id == "custom" {
            return Err(format!(
                "config.toml line {line_no}: profile `id` `custom` is reserved for ad-hoc model/effort switches and cannot be redefined"
            ));
        }
        let provider = self.provider.ok_or_else(|| {
            format!("config.toml line {line_no}: profile `{id}` missing required `provider`")
        })?;
        if provider != "gemini" && provider != "openai" && provider != "anthropic" {
            return Err(format!(
                "config.toml line {line_no}: profile `{id}` `provider` must be `\"gemini\"`, `\"openai\"` or `\"anthropic\"`, got `{provider}`"
            ));
        }
        let model = self.model.ok_or_else(|| {
            format!("config.toml line {line_no}: profile `{id}` missing required `model`")
        })?;
        if model.trim().is_empty() {
            return Err(format!(
                "config.toml line {line_no}: profile `{id}` `model` cannot be empty"
            ));
        }
        let effort = self.effort.flatten();
        if let Some(e) = &effort {
            if !matches!(e.as_str(), "low" | "medium" | "high") {
                return Err(format!(
                    "config.toml line {line_no}: profile `{id}` `effort` must be `\"low\"`, `\"medium\"`, `\"high\"` or `null`, got `{e}`"
                ));
            }
        }
        Ok(LlmProfileConfig {
            id,
            provider,
            model: model.trim().to_string(),
            effort,
            api_base: self.api_base.flatten(),
            api_key_env: self.api_key_env.flatten(),
        })
    }
}

/// Parsea la sección `[[llm_profiles]]` de un texto TOML.
/// Ignora las demás secciones para convivir limpiamente con `[permissions]`/`[sandbox]`/
/// `[impact]`/`[[external_drivers]]`.
pub fn parse_llm_profiles_toml(input: &str) -> Result<Vec<LlmProfileConfig>, String> {
    let mut out: Vec<LlmProfileConfig> = Vec::new();
    let text = input.strip_prefix('\u{feff}').unwrap_or(input);
    let lines: Vec<&str> = text.lines().collect();
    let mut current: Option<PartialProfile> = None;
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
                if out.iter().any(|p| p.id == cfg.id) {
                    return Err(format!(
                        "config.toml line {current_line}: duplicate profile id `{}` in `[[llm_profiles]]`",
                        cfg.id
                    ));
                }
                out.push(cfg);
            }
            if stripped.starts_with("[[") && stripped.ends_with("]]") {
                let header = stripped[2..stripped.len() - 2].trim();
                if header == "llm_profiles" {
                    current = Some(PartialProfile::default());
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
            format!("config.toml line {line_no}: expected `key = value` inside [[llm_profiles]]")
        })?;
        let key = raw_key.trim();
        let value_buf = raw_val.trim().to_string();

        match key {
            "api_key" => {
                return Err(format!(
                    "config.toml line {line_no}: plaintext `api_key` is forbidden in `[[llm_profiles]]`; use api_key_env instead of plaintext api_key (e.g. `api_key_env = \"OPENAI_API_KEY\"`)"
                ));
            }
            "id" => {
                if block.id.is_some() {
                    return Err(dup_key_err(line_no, key));
                }
                block.id = Some(parse_toml_string(&value_buf)?);
            }
            "provider" => {
                if block.provider.is_some() {
                    return Err(dup_key_err(line_no, key));
                }
                block.provider = Some(parse_toml_string(&value_buf)?);
            }
            "model" => {
                if block.model.is_some() {
                    return Err(dup_key_err(line_no, key));
                }
                block.model = Some(parse_toml_string(&value_buf)?);
            }
            "effort" => {
                if block.effort.is_some() {
                    return Err(dup_key_err(line_no, key));
                }
                block.effort = Some(parse_toml_optional_string(&value_buf)?);
            }
            "api_base" => {
                if block.api_base.is_some() {
                    return Err(dup_key_err(line_no, key));
                }
                block.api_base = Some(parse_toml_optional_string(&value_buf)?);
            }
            "api_key_env" => {
                if block.api_key_env.is_some() {
                    return Err(dup_key_err(line_no, key));
                }
                block.api_key_env = Some(parse_toml_optional_string(&value_buf)?);
            }
            other => {
                return Err(format!(
                    "config.toml line {line_no}: unknown key `{other}` in [[llm_profiles]] (expected `id`, `provider`, `model`, `effort`, `api_base`, or `api_key_env`)"
                ));
            }
        }
        i += 1;
    }

    if let Some(block) = current {
        let cfg = block.finalize(current_line)?;
        if out.iter().any(|p| p.id == cfg.id) {
            return Err(format!(
                "config.toml line {current_line}: duplicate profile id `{}` in `[[llm_profiles]]`",
                cfg.id
            ));
        }
        out.push(cfg);
    }

    Ok(out)
}

fn dup_key_err(line_no: usize, key: &str) -> String {
    format!("config.toml line {line_no}: duplicate key `{key}` in [[llm_profiles]] block")
}

/// Escalar opcional: `null` → `None`; `"texto"` → `Some(texto)`.
fn parse_toml_optional_string(raw: &str) -> Result<Option<String>, String> {
    let trimmed = raw.trim();
    if trimmed == "null" {
        return Ok(None);
    }
    Ok(Some(parse_toml_string(trimmed)?))
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

/// Fusión `replace-by-id` con precedencia `global < workspace (trusted)`.
/// Si `trust_project == false`, los bloques del workspace se ignoran por completo (sin merge
/// parcial) y se devuelve cuántos se descartaron (para la advertencia en `trust_warnings`).
pub fn merge_llm_profiles(
    global: Vec<LlmProfileConfig>,
    project: Option<Vec<LlmProfileConfig>>,
    trust_project: bool,
) -> (Vec<LlmProfileConfig>, usize) {
    let mut merged = global;
    let apply = |list: Vec<LlmProfileConfig>, merged: &mut Vec<LlmProfileConfig>| {
        for incoming in list {
            match merged.iter_mut().find(|p| p.id == incoming.id) {
                Some(slot) => *slot = incoming, // reemplazo completo por `id`
                None => merged.push(incoming),
            }
        }
    };
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

/// Carga y fusiona `[[llm_profiles]]` desde `~/.onyx/config.toml` (global) y
/// `<workspace>/.onyx/config.toml` aplicando el gate de Workspace Trust
/// (mismo patrón que `load_external_drivers`).
pub fn load_llm_profiles(
    global_path: Option<&Path>,
    project_path: Option<&Path>,
    trust_project: bool,
) -> Result<(Vec<LlmProfileConfig>, Vec<String>), String> {
    let mut warnings = Vec::new();
    let mut global = Vec::new();
    if let Some(gp) = global_path {
        if gp.is_file() {
            let raw = fs::read_to_string(gp)
                .map_err(|e| format!("failed to read `{}`: {e}", gp.display()))?;
            global = parse_llm_profiles_toml(&raw)?;
        }
    }
    let project = if let Some(pp) = project_path {
        if pp.is_file() {
            let raw = fs::read_to_string(pp)
                .map_err(|e| format!("failed to read `{}`: {e}", pp.display()))?;
            Some(parse_llm_profiles_toml(&raw)?)
        } else {
            None
        }
    } else {
        None
    };
    let (merged, ignored) = merge_llm_profiles(global, project, trust_project);
    if ignored > 0 {
        warnings.push(format!(
            "untrusted project `.onyx/config.toml`: ignored {ignored} `[[llm_profiles]]` block(s); run `openonyx trust` or `/trust` to approve"
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

[[llm_profiles]]
id = "qwen-local"
provider = "openai"
model = "qwen38-27b"
effort = "high"
api_base = "http://127.0.0.1:11434/v1"

[[llm_profiles]]
id = "g4-prod"
provider = "gemini"
model = "gemini-4-hydrogen"
effort = null
api_key_env = "GEMINI_API_KEY"

[impact]
enabled = true
"#;

    #[test]
    fn parser_covers_multiple_blocks_and_defaults() {
        let profiles = parse_llm_profiles_toml(SAMPLE).unwrap();
        assert_eq!(profiles.len(), 2);

        let local = &profiles[0];
        assert_eq!(local.id, "qwen-local");
        assert_eq!(local.provider, "openai");
        assert_eq!(local.model, "qwen38-27b");
        assert_eq!(local.effort.as_deref(), Some("high"));
        assert_eq!(local.api_base.as_deref(), Some("http://127.0.0.1:11434/v1"));
        assert_eq!(local.api_key_env, None);

        let prod = &profiles[1];
        assert_eq!(prod.id, "g4-prod");
        assert_eq!(prod.provider, "gemini");
        assert_eq!(prod.model, "gemini-4-hydrogen");
        assert_eq!(prod.effort, None);
        assert_eq!(prod.api_base, None);
        assert_eq!(prod.api_key_env.as_deref(), Some("GEMINI_API_KEY"));
    }

    #[test]
    fn parser_rejects_plaintext_api_key_with_actionable_message() {
        let err = parse_llm_profiles_toml(
            "[[llm_profiles]]\nid = \"leaky\"\nprovider = \"openai\"\nmodel = \"gpt-4o\"\napi_key = \"sk-supersecret\"\n",
        )
        .unwrap_err();
        assert!(
            err.contains("use api_key_env instead of plaintext api_key"),
            "{err}"
        );
        assert!(
            !err.contains("sk-supersecret"),
            "el valor no debe eco-señalarse"
        );
    }

    #[test]
    fn parser_rejects_invalid_blocks() {
        let bad_id = parse_llm_profiles_toml(
            "[[llm_profiles]]\nid = \"Bad ID\"\nprovider = \"openai\"\nmodel = \"m\"\n",
        )
        .unwrap_err();
        assert!(bad_id.contains("charset"), "{bad_id}");

        let reserved_custom = parse_llm_profiles_toml(
            "[[llm_profiles]]\nid = \"custom\"\nprovider = \"openai\"\nmodel = \"m\"\n",
        )
        .unwrap_err();
        assert!(reserved_custom.contains("reserved"), "{reserved_custom}");

        let bad_provider = parse_llm_profiles_toml(
            "[[llm_profiles]]\nid = \"ok\"\nprovider = \"mistral\"\nmodel = \"m\"\n",
        )
        .unwrap_err();
        assert!(bad_provider.contains("provider"), "{bad_provider}");

        let empty_model = parse_llm_profiles_toml(
            "[[llm_profiles]]\nid = \"ok\"\nprovider = \"openai\"\nmodel = \"   \"\n",
        )
        .unwrap_err();
        assert!(empty_model.contains("model"), "{empty_model}");

        let bad_effort = parse_llm_profiles_toml(
            "[[llm_profiles]]\nid = \"ok\"\nprovider = \"openai\"\nmodel = \"m\"\neffort = \"ultra\"\n",
        )
        .unwrap_err();
        assert!(bad_effort.contains("effort"), "{bad_effort}");

        let unknown_key = parse_llm_profiles_toml(
            "[[llm_profiles]]\nid = \"ok\"\nprovider = \"openai\"\nmodel = \"m\"\nfoo = 1\n",
        )
        .unwrap_err();
        assert!(unknown_key.contains("unknown key"), "{unknown_key}");

        let missing_provider =
            parse_llm_profiles_toml("[[llm_profiles]]\nid = \"ok\"\nmodel = \"m\"\n").unwrap_err();
        assert!(
            missing_provider.contains("missing required `provider`"),
            "{missing_provider}"
        );

        let dup_id = parse_llm_profiles_toml(
            "[[llm_profiles]]\nid = \"a\"\nprovider = \"openai\"\nmodel = \"m\"\n[[llm_profiles]]\nid = \"a\"\nprovider = \"gemini\"\nmodel = \"m2\"\n",
        )
        .unwrap_err();
        assert!(dup_id.contains("duplicate profile id"), "{dup_id}");

        let dup_key = parse_llm_profiles_toml(
            "[[llm_profiles]]\nid = \"a\"\nid = \"b\"\nprovider = \"openai\"\nmodel = \"m\"\n",
        )
        .unwrap_err();
        assert!(dup_key.contains("duplicate key"), "{dup_key}");
    }

    #[test]
    fn parser_accepts_anthropic_provider() {
        let profiles = parse_llm_profiles_toml(
            "[[llm_profiles]]\nid = \"claude-prod\"\nprovider = \"anthropic\"\nmodel = \"claude-sonnet-4-5\"\neffort = \"medium\"\napi_key_env = \"ANTHROPIC_API_KEY\"\n",
        )
        .unwrap();
        assert_eq!(profiles.len(), 1);
        let p = &profiles[0];
        assert_eq!(p.id, "claude-prod");
        assert_eq!(p.provider, "anthropic");
        assert_eq!(p.model, "claude-sonnet-4-5");
        assert_eq!(p.effort.as_deref(), Some("medium"));
        assert_eq!(p.api_key_env.as_deref(), Some("ANTHROPIC_API_KEY"));
    }

    fn profile(id: &str, provider: &str, model: &str) -> LlmProfileConfig {
        LlmProfileConfig {
            id: id.to_string(),
            provider: provider.to_string(),
            model: model.to_string(),
            effort: None,
            api_base: None,
            api_key_env: None,
        }
    }

    #[test]
    fn merge_is_replace_by_id_with_global_project_precedence() {
        let global_qwen = profile("qwen-local", "openai", "qwen38-27b");
        let project_qwen = profile("qwen-local", "openai", "qwen38-30b");
        let project_extra = profile("claude-relay", "openai", "claude-sonnet-4");

        // Trusted: workspace reemplaza por `id` completo y añade perfiles.
        let (merged, ignored) = merge_llm_profiles(
            vec![global_qwen.clone()],
            Some(vec![project_qwen.clone(), project_extra.clone()]),
            true,
        );
        assert_eq!(ignored, 0);
        assert_eq!(merged.len(), 2);
        let qwen = merged.iter().find(|p| p.id == "qwen-local").unwrap();
        assert_eq!(qwen.model, "qwen38-30b", "workspace > global");
        assert!(merged.iter().any(|p| p.id == "claude-relay"));

        // Untrusted: los bloques del workspace se ignoran por completo.
        let (merged, ignored) = merge_llm_profiles(
            vec![global_qwen.clone()],
            Some(vec![project_qwen, project_extra]),
            false,
        );
        assert_eq!(ignored, 2);
        assert_eq!(merged.len(), 1);
        assert_eq!(
            merged[0].model, "qwen38-27b",
            "global sobrevive sin workspace"
        );

        // Sin global ni proyecto: vacío (el perfil implícito de arranque vive en el anfitrión).
        let (merged, ignored) = merge_llm_profiles(Vec::new(), None, true);
        assert!(merged.is_empty());
        assert_eq!(ignored, 0);
    }

    #[test]
    fn load_applies_trust_gate_and_warns_when_ignoring_project() {
        let dir = std::env::temp_dir().join(format!(
            "openonyx_llm_profiles_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let global = dir.join("global.toml");
        let project = dir.join("project.toml");
        std::fs::write(
            &global,
            "[[llm_profiles]]\nid = \"g\"\nprovider = \"openai\"\nmodel = \"mg\"\n",
        )
        .unwrap();
        std::fs::write(
            &project,
            "[[llm_profiles]]\nid = \"p\"\nprovider = \"gemini\"\nmodel = \"mp\"\n",
        )
        .unwrap();

        let (profiles, warnings) = load_llm_profiles(Some(&global), Some(&project), true).unwrap();
        assert!(warnings.is_empty());
        assert_eq!(profiles.len(), 2);
        assert!(profiles.iter().any(|p| p.id == "p"));

        let (profiles, warnings) = load_llm_profiles(Some(&global), Some(&project), false).unwrap();
        assert_eq!(profiles.len(), 1);
        assert!(profiles.iter().all(|p| p.id != "p"));
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("ignored 1 `[[llm_profiles]]`")),
            "{warnings:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
