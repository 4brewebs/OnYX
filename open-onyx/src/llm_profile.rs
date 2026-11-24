//! Cambio dinámico en caliente de proveedor/modelo/perfil/esfuerzo LLM
//! (`SDD-03` `REQ-03.6` + hot-swap LLM).
//!
//! Aquí viven `LlmProvider`, `LlmConfig` y `validate_api_base` (movidos de
//! `main.rs` para compartirlos entre arranque CLI, TUI y Web sin duplicar), y
//! `LlmProfileHandle`: el registro thread-safe de perfiles
//! `[[llm_profiles]]` (cargados por `onyx-workspace`) + el objetivo LLM activo.
//!
//! **Seguridad:**
//! - `public_snapshot()` **nunca** expone `api_key` ni `api_key_env` (solo
//!   metadatos: id, provider, model, effort, disponibilidad).
//! - Cada `api_base` declarada se valida con [`validate_api_base`] en el
//!   registro; un perfil remoto que filtraría credenciales queda `unavailable`.
//! - El valor de la credencial se resuelve **siempre** desde la variable de
//!   entorno nombrada (o desde el arranque CLI para `"default"`); jamás se
//!   persiste en `config.toml` ni en snapshots.

use std::env;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::context::ReasoningEffort;
use crate::gemini::DEFAULT_GEMINI_MODEL;
use crate::openai::DEFAULT_OPENAI_MODEL;
use onyx_workspace::{LlmProfileConfig, RecordedLlmProfile};

/// Proveedor LLM soportado por el harness (arranque y hot-swap).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LlmProvider {
    Gemini,
    OpenAi,
}

impl LlmProvider {
    /// Todos los proveedores, en orden estable (para snapshots deterministas).
    pub(crate) const ALL: [LlmProvider; 2] = [Self::Gemini, Self::OpenAi];

    pub(crate) fn parse(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "gemini" => Ok(Self::Gemini),
            "openai" => Ok(Self::OpenAi),
            other => Err(format!(
                "unsupported provider `{other}` (expected `gemini` or `openai`)"
            )),
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Gemini => "gemini",
            Self::OpenAi => "openai",
        }
    }

    /// Modelo por defecto del proveedor (mismo contrato que el arranque CLI).
    pub(crate) fn default_model(self) -> &'static str {
        match self {
            Self::Gemini => DEFAULT_GEMINI_MODEL,
            Self::OpenAi => DEFAULT_OPENAI_MODEL,
        }
    }

    /// Variable de entorno estándar de la API key del proveedor.
    pub(crate) fn api_key_env(self) -> &'static str {
        match self {
            Self::Gemini => "GEMINI_API_KEY",
            Self::OpenAi => "OPENAI_API_KEY",
        }
    }

    /// Variables de entorno candidatas al base URL, en orden de preferencia
    /// (mismo contrato que el arranque CLI).
    pub(crate) fn api_base_envs(self) -> &'static [&'static str] {
        match self {
            Self::Gemini => &["GEMINI_API_BASE"],
            Self::OpenAi => &["OPENAI_API_BASE", "OPENAI_BASE_URL"],
        }
    }
}

/// Configuración LLM efectiva de una sesión (arranque CLI o objetivo activo).
#[derive(Clone)]
pub(crate) struct LlmConfig {
    pub(crate) provider: LlmProvider,
    pub(crate) api_key: String,
    pub(crate) model: String,
    pub(crate) effort: Option<ReasoningEffort>,
    pub(crate) api_base: Option<String>,
    pub(crate) project_instructions: String,
}

/// `true` si un host de base URL es loopback (`localhost`, `127.0.0.1`, `::1`, `*.localhost`):
/// los endpoints locales (Ollama/vLLM/LM Studio) no acumulan coste estimado.
pub(crate) fn is_loopback_host(host: &str) -> bool {
    let lower = host.to_ascii_lowercase();
    lower == "localhost" || lower == "127.0.0.1" || lower == "::1" || lower.ends_with(".localhost")
}

fn is_official_provider_host(provider: LlmProvider, host: &str) -> bool {
    let lower = host.to_ascii_lowercase();
    match provider {
        LlmProvider::Gemini => lower == "generativelanguage.googleapis.com",
        LlmProvider::OpenAi => lower == "api.openai.com",
    }
}

/// Valida un base URL de endpoint (`--api-base`, `*_API_BASE` o perfil
/// `[[llm_profiles]]`):
/// - Permite `http://` **solo** en loopback (`localhost`, `127.0.0.1`, `[::1]`, `*.localhost`).
/// - Exige `https://` para cualquier host remoto.
/// - Bloquea envío de credenciales (`api_key` no vacía) a hosts remotos no oficiales salvo
///   opt-in explícito del usuario (`--allow-custom-auth-host` / `ONYX_ALLOW_CUSTOM_AUTH_HOST=1`);
///   **nunca** se concede desde `.onyx/config.toml`.
pub(crate) fn validate_api_base(
    provider: LlmProvider,
    raw_url: &str,
    has_api_key: bool,
    allow_custom_auth_host: bool,
) -> Result<String, String> {
    let trimmed = raw_url.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return Err("--api-base must not be empty".to_string());
    }
    if trimmed.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(format!(
            "invalid --api-base `{trimmed}`: whitespace/control characters not allowed"
        ));
    }

    let (is_https, rest) = if let Some(r) = trimmed.strip_prefix("https://") {
        (true, r)
    } else if let Some(r) = trimmed.strip_prefix("http://") {
        (false, r)
    } else {
        return Err(format!(
            "invalid --api-base `{trimmed}`: scheme must be `https://` (or `http://` for loopback hosts)"
        ));
    };

    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.is_empty() {
        return Err(format!("invalid --api-base `{trimmed}`: missing host"));
    }
    if authority.contains('@') {
        return Err(format!(
            "invalid --api-base `{trimmed}`: embedded credentials (`user:pass@host`) are not allowed"
        ));
    }

    let host = if let Some(bracketed) = authority.strip_prefix('[') {
        let Some((ipv6, tail)) = bracketed.split_once(']') else {
            return Err(format!(
                "invalid --api-base `{trimmed}`: unclosed IPv6 bracket"
            ));
        };
        if !tail.is_empty() && !tail.starts_with(':') {
            return Err(format!(
                "invalid --api-base `{trimmed}`: invalid authority after `]`"
            ));
        }
        ipv6
    } else {
        authority.split(':').next().unwrap_or("")
    };

    if host.is_empty() {
        return Err(format!("invalid --api-base `{trimmed}`: empty host"));
    }

    let loopback = is_loopback_host(host);
    if !is_https && !loopback {
        return Err(format!(
            "refusing insecure `http://` --api-base for remote host `{host}`: use `https://` (`http://` is only permitted for loopback hosts like `localhost`, `127.0.0.1`, or `[::1]`)"
        ));
    }

    if !loopback
        && !is_official_provider_host(provider, host)
        && has_api_key
        && !allow_custom_auth_host
    {
        return Err(format!(
            "refusing to send API key to non-official remote host `{host}`: pass `--allow-custom-auth-host` (or set `ONYX_ALLOW_CUSTOM_AUTH_HOST=1`) to explicitly authorize sending credentials to this endpoint"
        ));
    }

    Ok(trimmed.to_string())
}

/// Snapshot **inmutable** del objetivo LLM activo, tomado en el momento del
/// despacho de cada llamada: un cambio en caliente nunca muta una petición
/// HTTP en vuelo.
#[derive(Clone, Debug)]
pub(crate) struct ResolvedLlmTarget {
    pub(crate) profile_id: String,
    pub(crate) provider: LlmProvider,
    pub(crate) model: String,
    pub(crate) effort: Option<ReasoningEffort>,
    pub(crate) api_key: String,
    pub(crate) api_base: Option<String>,
    pub(crate) is_local_endpoint: bool,
    pub(crate) project_instructions: Arc<String>,
}

/// Etiqueta pública del esfuerzo (`None` → `"auto"`).
pub(crate) fn effort_label(effort: Option<ReasoningEffort>) -> &'static str {
    match effort {
        Some(e) => e.as_str(),
        None => "auto",
    }
}

/// Metadatos públicos de un perfil registrado (**sin** credenciales).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LlmProfilePublicEntry {
    pub(crate) id: String,
    pub(crate) provider: String,
    pub(crate) model: String,
    pub(crate) effort: String,
    pub(crate) available: bool,
    pub(crate) unavailable_reason: Option<String>,
}

/// Snapshot público de [`LlmProfileHandle`] para TUI/Web: **nunca** contiene
/// `api_key` ni `api_key_env`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LlmProfilesPublicState {
    pub(crate) active_profile_id: String,
    pub(crate) provider: String,
    pub(crate) model: String,
    pub(crate) effort: String,
    pub(crate) profiles: Vec<LlmProfilePublicEntry>,
    pub(crate) available_providers: Vec<String>,
}

/// Origen de las credenciales de un perfil (el valor nunca se guarda).
enum LlmKeySource {
    /// API key del arranque CLI (solo el perfil implícito `"default"`).
    Startup,
    /// `api_key_env = "NOMBRE"` explícito en TOML (obligatoria).
    Explicit(String),
    /// Sin `api_key_env` explícito: se resuelve por env como en el arranque CLI
    /// (variable estándar del provider → `ONYX_API_KEY`); `optional` → un perfil
    /// OpenAI con `api_base` local (Ollama/vLLM) puede operar con clave vacía.
    Fallback { optional: bool },
}

/// Perfil registrado en el handle (el valor de la credencial nunca se guarda;
/// solo su origen: arranque CLI, variable de entorno nombrada o env por provider).
struct LlmProfileEntry {
    id: String,
    provider: LlmProvider,
    model: String,
    effort: Option<ReasoningEffort>,
    /// Base URL ya validada con [`validate_api_base`] en el registro.
    api_base: Option<String>,
    key_source: LlmKeySource,
    available: bool,
    unavailable_reason: Option<String>,
}

/// Estado del objetivo activo (equivalente inmutable de un `LlmConfig`).
#[derive(Clone)]
struct LlmActiveState {
    profile_id: String,
    provider: LlmProvider,
    model: String,
    effort: Option<ReasoningEffort>,
    api_key: String,
    api_base: Option<String>,
}

struct LlmRegistryState {
    entries: Vec<LlmProfileEntry>,
    active: LlmActiveState,
    /// API key del arranque CLI (para volver a `"default"` sin leer env).
    startup_key: String,
    allow_custom_auth_host: bool,
    project_instructions: Arc<String>,
}

/// Registro thread-safe de perfiles LLM y objetivo activo.
///
/// - Se construye desde el `LlmConfig` de arranque (perfil implícito
///   `"default"`, o el `[[llm_profiles]]` que coincida en provider/modelo/base)
///   más todos los perfiles declarados y validados.
/// - Todo cambio (`switch_profile`/`set_provider`/`set_model`/`set_effort`)
///   marca `dirty`, que la TUI/Web consumen en `pump_idle` para refrescar la
///   cabecera sin polling.
#[derive(Clone)]
pub(crate) struct LlmProfileHandle {
    state: Arc<Mutex<LlmRegistryState>>,
    dirty: Arc<AtomicBool>,
}

impl LlmProfileHandle {
    /// Construye el registro desde la configuración de arranque CLI y los
    /// perfiles `[[llm_profiles]]` ya cargados por `onyx-workspace` (con su
    /// gate de Workspace Trust).
    ///
    /// Disponibilidad de cada perfil: se marca `unavailable` (con motivo
    /// accionable) si falta la variable de entorno de la API key o si
    /// `validate_api_base` rechaza su `api_base` (p. ej. exfiltración a host
    /// remoto no oficial sin opt-in).
    pub(crate) fn new(
        cfg: &LlmConfig,
        profiles: &[LlmProfileConfig],
        allow_custom_auth_host: bool,
    ) -> Self {
        let mut entries: Vec<LlmProfileEntry> = vec![LlmProfileEntry {
            id: "default".to_string(),
            provider: cfg.provider,
            model: cfg.model.clone(),
            effort: cfg.effort,
            api_base: cfg.api_base.clone(),
            key_source: LlmKeySource::Startup,
            available: true,
            unavailable_reason: None,
        }];

        for p in profiles {
            let Ok(provider) = LlmProvider::parse(&p.provider) else {
                continue; // el parser de onyx-workspace ya garantiza `gemini`/`openai`
            };
            let effort = p
                .effort
                .as_deref()
                .and_then(|e| ReasoningEffort::parse(e).ok());
            // Credenciales: `api_key_env` explícito (obligatorio) o fallback por env
            // como en el arranque CLI (variable del provider → `ONYX_API_KEY`); un
            // perfil OpenAI con `api_base` local puede operar sin clave (Ollama/vLLM).
            let key_source = match &p.api_key_env {
                Some(name) => LlmKeySource::Explicit(name.clone()),
                None => LlmKeySource::Fallback {
                    optional: provider == LlmProvider::OpenAi && p.api_base.is_some(),
                },
            };
            let key_present = match &key_source {
                LlmKeySource::Startup => true,
                LlmKeySource::Explicit(name) => {
                    env::var(name).ok().is_some_and(|k| !k.trim().is_empty())
                }
                LlmKeySource::Fallback { optional } => {
                    Self::env_key_for(provider).is_some() || *optional
                }
            };

            let (mut available, mut unavailable_reason, api_base) = match &p.api_base {
                Some(raw) => {
                    match validate_api_base(provider, raw, key_present, allow_custom_auth_host) {
                        Ok(validated) => (true, None, Some(validated)),
                        Err(err) => (false, Some(err), None),
                    }
                }
                None => (true, None, None),
            };
            if available && !key_present {
                available = false;
                unavailable_reason = match &key_source {
                    LlmKeySource::Explicit(name) => Some(format!(
                        "API key env var `{name}` is not set (required by LLM profile `{}`)",
                        p.id
                    )),
                    _ => Some(format!(
                        "no API key for provider `{}`: set `{}` or `ONYX_API_KEY` (or an explicit `api_key_env` on the profile)",
                        provider.as_str(),
                        provider.api_key_env()
                    )),
                };
            }

            let entry = LlmProfileEntry {
                id: p.id.clone(),
                provider,
                model: p.model.clone(),
                effort,
                api_base,
                key_source,
                available,
                unavailable_reason,
            };
            // `replace-by-id`: un perfil declarado con un id existente sustituye
            // al registrado — salvo un bloque TOML `id = "default"` que falle la
            // disponibilidad, que no invalida el perfil de arranque operativo.
            if let Some(slot) = entries.iter_mut().find(|e| e.id == entry.id) {
                if entry.available || slot.id != "default" {
                    *slot = entry;
                }
            } else {
                entries.push(entry);
            }
        }

        // Perfil activo: el de arranque; si un perfil **disponible** declarado
        // coincide en provider+modelo+base, su `id` lo representa (la clave sigue
        // siendo la CLI).
        let active_profile_id = entries
            .iter()
            .find(|e| {
                e.id != "default"
                    && e.available
                    && e.provider == cfg.provider
                    && e.model == cfg.model
                    && e.api_base == cfg.api_base
            })
            .map(|e| e.id.clone())
            .unwrap_or_else(|| "default".to_string());

        let state = LlmRegistryState {
            entries,
            active: LlmActiveState {
                profile_id: active_profile_id,
                provider: cfg.provider,
                model: cfg.model.clone(),
                effort: cfg.effort,
                api_key: cfg.api_key.clone(),
                api_base: cfg.api_base.clone(),
            },
            startup_key: cfg.api_key.clone(),
            allow_custom_auth_host,
            project_instructions: Arc::new(cfg.project_instructions.clone()),
        };
        Self {
            state: Arc::new(Mutex::new(state)),
            dirty: Arc::new(AtomicBool::new(false)),
        }
    }

    fn mark_dirty(&self) {
        self.dirty.store(true, Ordering::SeqCst);
    }

    /// `true` si algún cambio en caliente ha ocurrido desde el último
    /// `take_dirty()` (para refrescar UI en `pump_idle`).
    #[allow(dead_code)] // Fase 3 (TUI `pump_idle`) / Fase 4 (SSE Web).
    pub(crate) fn take_dirty(&self) -> bool {
        self.dirty.swap(false, Ordering::SeqCst)
    }

    /// Resuelve la API key de un perfil (nunca un valor persistido): clave del
    /// arranque para `"default"`, variable de entorno nombrada (`Explicit`,
    /// obligatoria) o env por provider (`Fallback`; `optional` → clave vacía).
    fn resolve_entry_key(entry: &LlmProfileEntry, startup_key: &str) -> Result<String, String> {
        match &entry.key_source {
            LlmKeySource::Startup => Ok(startup_key.to_string()),
            LlmKeySource::Explicit(name) => env::var(name)
                .ok()
                .filter(|k| !k.trim().is_empty())
                .ok_or_else(|| {
                    format!(
                        "API key env var `{name}` is not set (required by LLM profile `{}`)",
                        entry.id
                    )
                }),
            LlmKeySource::Fallback { optional } => match Self::env_key_for(entry.provider) {
                Some(k) => Ok(k),
                None if *optional => Ok(String::new()),
                None => Err(format!(
                    "no API key for provider `{}`: set `{}` or `ONYX_API_KEY` (or an explicit `api_key_env` on the profile)",
                    entry.provider.as_str(),
                    entry.provider.api_key_env()
                )),
            },
        }
    }

    /// API key del proveedor desde el entorno con la misma precedencia que el
    /// arranque CLI (variable estándar del provider → `ONYX_API_KEY`).
    fn env_key_for(provider: LlmProvider) -> Option<String> {
        [provider.api_key_env(), "ONYX_API_KEY"]
            .iter()
            .find_map(|v| env::var(*v).ok())
            .filter(|k| !k.trim().is_empty())
    }

    /// Credenciales del proveedor desde el entorno, con el mismo contrato que
    /// el arranque CLI (autodescubrimiento del provider alternativo).
    fn env_credentials_for(provider: LlmProvider) -> Option<(String, Option<String>)> {
        let key = Self::env_key_for(provider);
        let base = provider
            .api_base_envs()
            .iter()
            .find_map(|v| env::var(*v).ok())
            .filter(|b| !b.trim().is_empty());
        match provider {
            LlmProvider::Gemini => key.map(|k| (k, base)),
            LlmProvider::OpenAi => match (key, base) {
                (Some(k), base) => Some((k, base)),
                (None, Some(b)) => Some((String::new(), Some(b))),
                (None, None) => None,
            },
        }
    }

    fn provider_available(&self, st: &LlmRegistryState, provider: LlmProvider) -> bool {
        st.entries
            .iter()
            .any(|e| e.provider == provider && e.available)
            || Self::env_credentials_for(provider).is_some()
    }

    fn target_from(&self, st: &LlmRegistryState) -> ResolvedLlmTarget {
        let is_local_endpoint = st
            .active
            .api_base
            .as_deref()
            .is_some_and(|raw| is_loopback_host(api_base_host(raw)));
        ResolvedLlmTarget {
            profile_id: st.active.profile_id.clone(),
            provider: st.active.provider,
            model: st.active.model.clone(),
            effort: st.active.effort,
            api_key: st.active.api_key.clone(),
            api_base: st.active.api_base.clone(),
            is_local_endpoint,
            project_instructions: st.project_instructions.clone(),
        }
    }

    /// Snapshot inmutable del objetivo activo (para despachar cada llamada).
    pub(crate) fn active_target(&self) -> ResolvedLlmTarget {
        let st = self.state.lock().unwrap();
        self.target_from(&st)
    }

    /// Instrucciones de proyecto (`AGENTS.md`/`ONYX.md`) compartidas por todos
    /// los perfiles del registro (cargadas una sola vez en el arranque).
    #[allow(dead_code)] // Fase 3 (TUI: cabecera de perfiles) / Fase 4 (SSE Web).
    pub(crate) fn project_instructions(&self) -> Arc<String> {
        self.state.lock().unwrap().project_instructions.clone()
    }

    /// Cambia a un perfil registrado. Error accionable si no existe o no está
    /// disponible (clave ausente, `api_base` rechazada, …).
    pub(crate) fn switch_profile(&self, id: &str) -> Result<ResolvedLlmTarget, String> {
        let id = id.trim();
        let mut st = self.state.lock().unwrap();
        let entry = st.entries.iter().find(|e| e.id == id).ok_or_else(|| {
            let known = st
                .entries
                .iter()
                .map(|e| e.id.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            format!("unknown LLM profile `{id}` (available: {known})")
        })?;
        if !entry.available {
            let reason = entry
                .unavailable_reason
                .clone()
                .unwrap_or_else(|| "no availability details".to_string());
            return Err(format!("LLM profile `{id}` is unavailable: {reason}"));
        }
        let api_key = Self::resolve_entry_key(entry, &st.startup_key)?;
        st.active = LlmActiveState {
            profile_id: entry.id.clone(),
            provider: entry.provider,
            model: entry.model.clone(),
            effort: entry.effort,
            api_key,
            api_base: entry.api_base.clone(),
        };
        self.mark_dirty();
        Ok(self.target_from(&st))
    }

    /// Cambia de proveedor usando las credenciales/base del primer perfil
    /// disponible de ese proveedor (o el autodescubrimiento por env). El
    /// modelo se conserva si coincide con el del perfil; si no, se ajusta al
    /// del perfil (o al default del proveedor). Error accionable sin credenciales.
    pub(crate) fn set_provider(&self, provider: LlmProvider) -> Result<ResolvedLlmTarget, String> {
        let mut st = self.state.lock().unwrap();
        if st.active.provider == provider {
            return Ok(self.target_from(&st));
        }
        // Primero un perfil declarado del proveedor; si no, el perfil de
        // arranque `"default"` cuando sea de ese proveedor (p. ej.
        // `--provider openai --api-base http://127.0.0.1:8000/v1` sin env vars).
        if let Some(entry) = st
            .entries
            .iter()
            .find(|e| e.id != "default" && e.provider == provider && e.available)
            .or_else(|| {
                st.entries
                    .iter()
                    .find(|e| e.id == "default" && e.provider == provider && e.available)
            })
        {
            let api_key = Self::resolve_entry_key(entry, &st.startup_key)?;
            st.active = LlmActiveState {
                profile_id: entry.id.clone(),
                provider,
                model: entry.model.clone(),
                effort: st.active.effort,
                api_key,
                api_base: entry.api_base.clone(),
            };
        } else {
            let Some((api_key, base)) = Self::env_credentials_for(provider) else {
                return Err(format!(
                    "no credentials for provider `{}`: set `{}` (or define an `[[llm_profiles]]` entry with `api_key_env`)",
                    provider.as_str(),
                    provider.api_key_env()
                ));
            };
            let api_base = match base {
                Some(raw) => Some(
                    validate_api_base(
                        provider,
                        &raw,
                        !api_key.trim().is_empty(),
                        st.allow_custom_auth_host,
                    )
                    .map_err(|e| {
                        format!("cannot switch to provider `{}`: {e}", provider.as_str())
                    })?,
                ),
                None => None,
            };
            st.active = LlmActiveState {
                profile_id: "custom".to_string(),
                provider,
                model: provider.default_model().to_string(),
                effort: st.active.effort,
                api_key,
                api_base,
            };
        }
        self.mark_dirty();
        Ok(self.target_from(&st))
    }

    /// Cambia el modelo del objetivo activo; si difiere del preset del perfil
    /// activo, el objetivo queda marcado como `"custom"`.
    pub(crate) fn set_model(&self, model: &str) -> Result<ResolvedLlmTarget, String> {
        let model = model.trim();
        if model.is_empty() {
            return Err("a non-empty model name is required".to_string());
        }
        let mut st = self.state.lock().unwrap();
        let matches_preset = st
            .entries
            .iter()
            .find(|e| e.id == st.active.profile_id)
            .is_some_and(|e| e.model == model);
        st.active.model = model.to_string();
        if !matches_preset {
            st.active.profile_id = "custom".to_string();
        }
        self.mark_dirty();
        Ok(self.target_from(&st))
    }

    /// Cambia el esfuerzo de razonamiento del objetivo activo (`None` = auto).
    pub(crate) fn set_effort(&self, effort: Option<ReasoningEffort>) -> ResolvedLlmTarget {
        let mut st = self.state.lock().unwrap();
        let matches_preset = st
            .entries
            .iter()
            .find(|e| e.id == st.active.profile_id)
            .is_some_and(|e| e.effort == effort);
        st.active.effort = effort;
        if !matches_preset {
            st.active.profile_id = "custom".to_string();
        }
        self.mark_dirty();
        self.target_from(&st)
    }

    /// Sincroniza el handle con un estado modal restaurado (`/rewind`,
    /// `chat --resume --tick`, `on_restore`): **best-effort**, nunca falla
    /// aunque un perfil ya no exista (cauce a provider/modelo/esfuerzo).
    pub(crate) fn restore_from_graph(&self, rec: &RecordedLlmProfile) {
        let want_model = rec.model.trim();
        let want_effort = rec
            .effort
            .as_deref()
            .and_then(|e| ReasoningEffort::parse(e).ok());
        let recorded_provider = LlmProvider::parse(rec.provider.trim()).ok();
        let target = match self.switch_profile(rec.profile_id.trim()) {
            Ok(t) => t,
            Err(_) => {
                if let Some(provider) = recorded_provider {
                    let _ = self.set_provider(provider);
                }
                self.active_target()
            }
        };
        // Modelo/esfuerzo grabados solo si el proveedor activo coincide con el
        // grabado (no proyectar p. ej. un modelo OpenAI sobre Gemini → 404).
        if Some(target.provider) == recorded_provider {
            if target.model != want_model {
                let _ = self.set_model(want_model);
            }
            if target.effort != want_effort {
                let _ = self.set_effort(want_effort);
            }
        }
    }

    /// Snapshot público para TUI/Web (solo metadatos; **nunca** credenciales).
    pub(crate) fn public_snapshot(&self) -> LlmProfilesPublicState {
        let st = self.state.lock().unwrap();
        let profiles = st
            .entries
            .iter()
            .map(|e| LlmProfilePublicEntry {
                id: e.id.clone(),
                provider: e.provider.as_str().to_string(),
                model: e.model.clone(),
                effort: effort_label(e.effort).to_string(),
                available: e.available,
                unavailable_reason: e.unavailable_reason.clone(),
            })
            .collect();
        let available_providers = LlmProvider::ALL
            .iter()
            .filter(|p| self.provider_available(&st, **p))
            .map(|p| p.as_str().to_string())
            .collect();
        LlmProfilesPublicState {
            active_profile_id: st.active.profile_id.clone(),
            provider: st.active.provider.as_str().to_string(),
            model: st.active.model.clone(),
            effort: effort_label(st.active.effort).to_string(),
            profiles,
            available_providers,
        }
    }
}

/// Extrae el host de un base URL (para `is_local_endpoint`).
pub(crate) fn api_base_host(raw: &str) -> &str {
    let rest = raw.trim().split_once("//").map(|(_, r)| r).unwrap_or("");
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if let Some(bracketed) = authority.strip_prefix('[') {
        bracketed.split_once(']').map(|(v, _)| v).unwrap_or("")
    } else {
        authority.split(':').next().unwrap_or("")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use onyx_workspace::LlmProfileConfig;

    fn startup_cfg(provider: LlmProvider, model: &str, api_base: Option<&str>) -> LlmConfig {
        LlmConfig {
            provider,
            api_key: "startup-key".to_string(),
            model: model.to_string(),
            effort: None,
            api_base: api_base.map(|s| s.to_string()),
            project_instructions: String::new(),
        }
    }

    fn profile_cfg(
        id: &str,
        provider: &str,
        model: &str,
        api_base: Option<&str>,
        api_key_env: Option<&str>,
    ) -> LlmProfileConfig {
        LlmProfileConfig {
            id: id.to_string(),
            provider: provider.to_string(),
            model: model.to_string(),
            effort: None,
            api_base: api_base.map(|s| s.to_string()),
            api_key_env: api_key_env.map(|s| s.to_string()),
        }
    }

    /// Edition 2024: `set_var`/`remove_var` son `unsafe` (mutan el proceso entero).
    fn set_test_env(name: &str, value: &str) {
        unsafe {
            env::set_var(name, value);
        }
    }

    fn remove_test_env(name: &str) {
        unsafe {
            env::remove_var(name);
        }
    }

    fn restore_test_env(name: &str, saved: Option<String>) {
        unsafe {
            match saved {
                Some(v) => env::set_var(name, v),
                None => env::remove_var(name),
            }
        }
    }

    /// Guarda y borra las variables indicadas; devuelve qué restaurar al final.
    fn without_env(names: &[&str]) -> Vec<(String, Option<String>)> {
        names
            .iter()
            .map(|name| {
                let saved = env::var(name).ok();
                remove_test_env(name);
                (name.to_string(), saved)
            })
            .collect()
    }

    #[test]
    fn startup_config_becomes_default_profile_and_matching_toml_profile_represents_it() {
        let key_env = "OPENONIX_TEST_LLM_KEY_MATCHING";
        set_test_env(key_env, "env-key-value");
        let handle = LlmProfileHandle::new(
            &startup_cfg(
                LlmProvider::OpenAi,
                "qwen38-27b",
                Some("http://127.0.0.1:11434/v1"),
            ),
            &[profile_cfg(
                "qwen-local",
                "openai",
                "qwen38-27b",
                Some("http://127.0.0.1:11434/v1"),
                Some(key_env),
            )],
            false,
        );
        remove_test_env(key_env);

        let target = handle.active_target();
        assert_eq!(
            target.profile_id, "qwen-local",
            "el perfil coincidente representa el arranque"
        );
        assert_eq!(target.provider, LlmProvider::OpenAi);
        assert_eq!(target.model, "qwen38-27b");
        assert_eq!(
            target.api_key, "startup-key",
            "la clave activa sigue siendo la del arranque CLI"
        );
        assert!(target.is_local_endpoint);

        let snap = handle.public_snapshot();
        assert_eq!(snap.active_profile_id, "qwen-local");
        assert_eq!(snap.effort, "auto");
        assert_eq!(snap.profiles.len(), 2, "default + qwen-local");
        assert!(snap.profiles.iter().any(|p| p.id == "default"));
        assert!(
            snap.available_providers.contains(&"openai".to_string()),
            "{:?}",
            snap.available_providers
        );
    }

    #[test]
    fn switch_profile_unknown_and_unavailable_give_actionable_errors() {
        let missing_env = "OPENONIX_TEST_LLM_KEY_MISSING_9876";
        remove_test_env(missing_env);
        let handle = LlmProfileHandle::new(
            &startup_cfg(LlmProvider::Gemini, DEFAULT_GEMINI_MODEL, None),
            &[profile_cfg(
                "leaky",
                "gemini",
                "g1",
                None,
                Some(missing_env),
            )],
            false,
        );

        let err = handle.switch_profile("nope").unwrap_err();
        assert!(err.contains("unknown LLM profile"), "{err}");
        assert!(err.contains("default"), "{err}");

        let err = handle.switch_profile("leaky").unwrap_err();
        assert!(err.contains("unavailable"), "{err}");
        assert!(err.contains(missing_env), "{err}");
    }

    #[test]
    fn set_provider_without_credentials_fails_actionably() {
        // Hermeticidad: sin credenciales OpenAI ni base URLs de descubrimiento
        // (`OPENAI_API_BASE`/`OPENAI_BASE_URL` solos ya habilitan el fallback).
        let saved = without_env(&[
            "OPENAI_API_KEY",
            "ONYX_API_KEY",
            "OPENAI_API_BASE",
            "OPENAI_BASE_URL",
        ]);

        let handle = LlmProfileHandle::new(
            &startup_cfg(LlmProvider::Gemini, DEFAULT_GEMINI_MODEL, None),
            &[],
            false,
        );
        let err = handle.set_provider(LlmProvider::OpenAi).unwrap_err();
        assert!(err.contains("no credentials"), "{err}");
        assert!(err.contains("OPENAI_API_KEY"), "{err}");

        // Misma operación idempotente: no marca dirty ni cambia el objetivo.
        assert!(!handle.take_dirty());
        let target = handle.set_provider(LlmProvider::Gemini).unwrap();
        assert_eq!(target.provider, LlmProvider::Gemini);
        assert_eq!(target.profile_id, "default");

        for (name, saved) in saved {
            restore_test_env(&name, saved);
        }
    }

    #[test]
    fn local_openai_profile_without_api_key_is_available() {
        // Hermeticidad: sin credenciales OpenAI en el entorno del desarrollador.
        let saved = without_env(&["OPENAI_API_KEY", "ONYX_API_KEY"]);

        // Perfil local tipo Ollama/vLLM: OpenAI con `api_base` loopback y sin
        // `api_key_env` → la clave es opcional y el perfil queda disponible.
        let handle = LlmProfileHandle::new(
            &startup_cfg(LlmProvider::Gemini, DEFAULT_GEMINI_MODEL, None),
            &[profile_cfg(
                "qwen-local",
                "openai",
                "qwen38-27b",
                Some("http://127.0.0.1:11434/v1"),
                None,
            )],
            false,
        );

        let snap = handle.public_snapshot();
        let local = snap.profiles.iter().find(|p| p.id == "qwen-local").unwrap();
        assert!(local.available, "{local:?}");

        let t = handle.switch_profile("qwen-local").unwrap();
        assert_eq!(t.provider, LlmProvider::OpenAi);
        assert_eq!(t.model, "qwen38-27b");
        assert_eq!(t.api_key, "", "un endpoint local opera con clave vacía");
        assert_eq!(t.api_base.as_deref(), Some("http://127.0.0.1:11434/v1"));
        assert!(t.is_local_endpoint);

        for (name, saved) in saved {
            restore_test_env(&name, saved);
        }
    }

    #[test]
    fn set_model_off_preset_marks_custom_and_stays_custom() {
        let handle = LlmProfileHandle::new(
            &startup_cfg(LlmProvider::OpenAi, DEFAULT_OPENAI_MODEL, None),
            &[],
            false,
        );
        let t = handle.set_model("qwen38-30b").unwrap();
        assert_eq!(t.profile_id, "custom");
        assert_eq!(t.model, "qwen38-30b");
        assert_eq!(t.provider, LlmProvider::OpenAi);

        // Volver al modelo original no recupera el id del preset (estado ad-hoc
        // deterministicista: solo `switch_profile`/`set_provider` lo rehacen).
        let t = handle.set_model(DEFAULT_OPENAI_MODEL).unwrap();
        assert_eq!(t.profile_id, "custom");
        assert!(
            handle.set_model("   ").is_err(),
            "el modelo no puede quedar vacío"
        );
    }

    #[test]
    fn set_effort_updates_target_and_dirty_flag() {
        let handle = LlmProfileHandle::new(
            &startup_cfg(LlmProvider::Gemini, DEFAULT_GEMINI_MODEL, None),
            &[],
            false,
        );
        assert!(!handle.take_dirty());
        let t = handle.set_effort(Some(ReasoningEffort::High));
        assert_eq!(t.effort, Some(ReasoningEffort::High));
        assert_eq!(t.profile_id, "custom", "difiere del preset del perfil");
        assert!(handle.take_dirty());
        assert!(!handle.take_dirty(), "take_dirty solo una vez");

        let t = handle.set_effort(None);
        assert_eq!(t.effort, None);
        assert_eq!(handle.active_target().effort, None);
    }

    #[test]
    fn profile_remote_api_base_exfiltration_is_blocked_at_registration() {
        let key_env = "OPENONIX_TEST_LLM_KEY_EXFIL";
        set_test_env(key_env, "some-key");

        let strict = LlmProfileHandle::new(
            &startup_cfg(LlmProvider::Gemini, DEFAULT_GEMINI_MODEL, None),
            &[profile_cfg(
                "evil",
                "openai",
                "gpt-4o",
                Some("https://evil.example.com/v1"),
                Some(key_env),
            )],
            false,
        );
        let snap = strict.public_snapshot();
        let evil = snap.profiles.iter().find(|p| p.id == "evil").unwrap();
        assert!(!evil.available);
        assert!(
            evil.unavailable_reason
                .as_deref()
                .is_some_and(|r| r.contains("refusing to send API key")),
            "{evil:?}"
        );
        let err = strict.switch_profile("evil").unwrap_err();
        assert!(err.contains("unavailable"), "{err}");
        remove_test_env(key_env);

        // Con opt-in explícito del usuario sí queda disponible.
        set_test_env(key_env, "some-key");
        let optin = LlmProfileHandle::new(
            &startup_cfg(LlmProvider::Gemini, DEFAULT_GEMINI_MODEL, None),
            &[profile_cfg(
                "evil",
                "openai",
                "gpt-4o",
                Some("https://evil.example.com/v1"),
                Some(key_env),
            )],
            true,
        );
        remove_test_env(key_env);
        let snap = optin.public_snapshot();
        assert!(
            snap.profiles
                .iter()
                .find(|p| p.id == "evil")
                .unwrap()
                .available,
            "{snap:?}"
        );
    }

    #[test]
    fn public_snapshot_never_exposes_credentials() {
        let key_env = "OPENONIX_TEST_LLM_KEY_SNAPSHOT";
        set_test_env(key_env, "profile-secret-456");
        let handle = LlmProfileHandle::new(
            &startup_cfg(LlmProvider::OpenAi, DEFAULT_OPENAI_MODEL, None),
            &[profile_cfg(
                "with-key",
                "openai",
                "gpt-4o",
                None,
                Some(key_env),
            )],
            false,
        );
        remove_test_env(key_env);

        let snap = handle.public_snapshot();
        let rendered = format!("{snap:?}");
        assert!(!rendered.contains("startup-key"), "{rendered}");
        assert!(!rendered.contains("profile-secret-456"), "{rendered}");
        assert!(!rendered.contains(key_env), "ni el nombre de la variable");
    }

    #[test]
    fn restore_from_graph_is_best_effort_and_applies_recorded_state() {
        let key_env = "OPENONIX_TEST_LLM_KEY_RESTORE";
        set_test_env(key_env, "restore-key");
        let g4_prod = LlmProfileConfig {
            id: "g4-prod".to_string(),
            provider: "gemini".to_string(),
            model: "gemini-4-titanium".to_string(),
            effort: Some("high".to_string()),
            api_base: None,
            api_key_env: Some(key_env.to_string()),
        };
        let handle = LlmProfileHandle::new(
            &startup_cfg(LlmProvider::Gemini, DEFAULT_GEMINI_MODEL, None),
            &[g4_prod],
            false,
        );

        // Caso nominal: perfil registrado existente.
        handle.restore_from_graph(&RecordedLlmProfile {
            profile_id: "g4-prod".to_string(),
            provider: "gemini".to_string(),
            model: "gemini-4-titanium".to_string(),
            effort: Some("high".to_string()),
        });
        let t = handle.active_target();
        assert_eq!(t.profile_id, "g4-prod");
        assert_eq!(t.model, "gemini-4-titanium");
        assert_eq!(t.effort, Some(ReasoningEffort::High));
        remove_test_env(key_env);

        // Perfil ya no existe: cauce a provider/modelo/esfuerzo sin fallar.
        handle.restore_from_graph(&RecordedLlmProfile {
            profile_id: "gone".to_string(),
            provider: "gemini".to_string(),
            model: "gemini-4-titanium".to_string(),
            effort: None,
        });
        let t = handle.active_target();
        assert_eq!(t.profile_id, "custom");
        assert_eq!(t.model, "gemini-4-titanium");
        assert_eq!(t.effort, None);
    }
}
