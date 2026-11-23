//! Servidor web HTTP/1.1 + SSE determinista y driver `WebDriver` para `openonyx serve`.
//!
//! Implementación pura sobre `std::net::TcpListener`, `std::thread`, `Condvar`, `serde_json`
//! y `blake3` (**0 `tokio`**, **0 crates externos nuevos**):
//! - Sirve la PWA mobile-first Vanilla HTML/CSS/JS embebida con `include_str!` (`open-onyx/web/`).
//! - Expone la API REST + SSE versionada (`/api/v1/...`) lista para clientes externos (p. ej. Svelte).
//! - Autenticación obligatoria por API key (`Authorization: Bearer` o cookie `HttpOnly` tras
//!   `POST /api/v1/auth/login`), bind seguro `127.0.0.1:8787` por defecto y rechazo automático de
//!   binds no loopback sin clave explícita (`>= 16` chars).
//! - Cabeceras de seguridad modernas (CSP estricto con `require-trusted-types-for 'script'`,
//!   `X-Frame-Options: DENY`, `X-Content-Type-Options: nosniff`, `CORP`, `COOP`, `Sec-Fetch-Site`
//!   CSRF guard y `Clear-Site-Data` en logout).

use std::collections::{BTreeMap, VecDeque};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use onyx_soul_agents::{
    ConceptId, EventBus, GraphView, IngressEvent, OnyxEvent, PlatformDriver, Reply, SurfaceBinding,
    Value, Vocab, pending_approvals,
};
use onyx_workspace::{
    ImpactAssessment, PermissionDecision, SandboxDriverKind, SproutSupervisor, WorkspaceModeHandle,
    WorkspacePolicy, WorkspaceTools, WorkspaceVocab, apply_impact_escalation,
    detect_sandbox_capabilities, graph_subagent_records, graph_workspace_plan,
    graph_workspace_services, preview_tool_diff,
};
use serde::{Deserialize, Serialize};

use crate::context::{ReasoningEffort, SessionTelemetryHandle};
use crate::gemini::StreamPreviewHandle;
use crate::pack::SharedPackStore;
use crate::session::UiIdlePump;
use crate::subagent::{MergePreviewState, SUBAGENT_PROPOSAL_CMD_PREFIX, SubagentHandle};
use crate::tui_input::WorkspaceFileIndex;
use crate::vault::session_display_label;

pub const DEFAULT_WEB_BIND: &str = "127.0.0.1:8787";
pub const MIN_EXPLICIT_API_KEY_LEN: usize = 16;
const MAX_HTTP_HEADER_BYTES: usize = 16 * 1024;
const MAX_HTTP_BODY_BYTES: usize = 64 * 1024;
const MAX_SSE_CLIENTS: usize = 32;
const STREAM_SSE_THROTTLE_MS: u64 = 120;
const MAX_STREAM_THOUGHT_BYTES: usize = 4 * 1024;

const ASSET_INDEX_HTML: &str = include_str!("../web/index.html");
const ASSET_STYLES_CSS: &str = include_str!("../web/styles.css");
const ASSET_APP_JS: &str = include_str!("../web/app.js");
const ASSET_MANIFEST: &str = include_str!("../web/manifest.webmanifest");
const ASSET_ICON_SVG: &str = include_str!("../web/icon.svg");
const ASSET_LOGO_JPG: &[u8] = include_bytes!("../web/logo.jpg");

const CSP_HEADER: &str = "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; connect-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'; require-trusted-types-for 'script'";

/// Configuración validada para arrancar `openonyx serve`.
#[derive(Clone, Debug)]
pub struct WebServerConfig {
    pub bind_addr: SocketAddr,
    pub api_key: String,
    pub generated_key: bool,
    pub cors_origin: Option<String>,
}

impl WebServerConfig {
    /// Resuelve dirección de escucha y política de API key:
    /// - En loopback (`127.0.0.1`, `[::1]`), genera clave aleatoria de 256 bits si no se provee.
    /// - En interfaces no loopback (`0.0.0.0`, IP pública VPS), exige clave explícita `>= 16` chars.
    pub fn resolve(
        raw_bind: &str,
        explicit_api_key: Option<String>,
        cors_origin: Option<String>,
    ) -> Result<Self, String> {
        let bind_addr: SocketAddr = raw_bind.trim().parse().map_err(|e| {
            format!("invalid --bind address `{raw_bind}` (expected `ip:port`, e.g. `127.0.0.1:8787`): {e}")
        })?;

        let is_loopback = bind_addr.ip().is_loopback();
        let trimmed_key = explicit_api_key
            .map(|k| k.trim().to_string())
            .filter(|k| !k.is_empty());

        let (api_key, generated_key) = match trimmed_key {
            Some(k) => {
                if !is_loopback && k.len() < MIN_EXPLICIT_API_KEY_LEN {
                    return Err(format!(
                        "refusing to bind non-loopback address `{bind_addr}` with an API key shorter than {MIN_EXPLICIT_API_KEY_LEN} characters"
                    ));
                }
                (k, false)
            }
            None => {
                if !is_loopback {
                    return Err(format!(
                        "refusing to bind non-loopback address `{bind_addr}` without an explicit API key: pass `--api-key <key>` or set `ONYX_WEB_API_KEY` (at least {MIN_EXPLICIT_API_KEY_LEN} characters)"
                    ));
                }
                (generate_ephemeral_api_key()?, true)
            }
        };

        let clean_cors = cors_origin
            .map(|o| o.trim().trim_end_matches('/').to_string())
            .filter(|o| !o.is_empty());
        if let Some(ref origin) = clean_cors {
            if origin == "*" || !(origin.starts_with("http://") || origin.starts_with("https://")) {
                return Err(format!(
                    "invalid --cors-origin `{origin}`: must be an explicit `http://` or `https://` origin (wildcard `*` is not allowed)"
                ));
            }
        }

        Ok(Self {
            bind_addr,
            api_key,
            generated_key,
            cors_origin: clean_cors,
        })
    }
}

/// Genera una API key aleatoria de 256 bits usando el CSPRNG del sistema operativo.
pub fn generate_ephemeral_api_key() -> Result<String, String> {
    let mut bytes = [0u8; 32];
    fill_os_random(&mut bytes)?;
    let mut hex = String::with_capacity(69);
    hex.push_str("onyx_");
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(hex, "{b:02x}");
    }
    Ok(hex)
}

#[cfg(windows)]
fn fill_os_random(buf: &mut [u8]) -> Result<(), String> {
    const BCRYPT_USE_SYSTEM_PREFERRED_RNG: u32 = 0x0000_0002;
    #[link(name = "bcrypt")]
    unsafe extern "system" {
        fn BCryptGenRandom(
            h_algorithm: *mut std::ffi::c_void,
            pb_buffer: *mut u8,
            cb_buffer: u32,
            dw_flags: u32,
        ) -> i32;
    }
    let status = unsafe {
        BCryptGenRandom(
            std::ptr::null_mut(),
            buf.as_mut_ptr(),
            buf.len() as u32,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    if status == 0 {
        Ok(())
    } else {
        Err(format!("BCryptGenRandom failed (status 0x{status:08x})"))
    }
}

#[cfg(not(windows))]
fn fill_os_random(buf: &mut [u8]) -> Result<(), String> {
    let mut f = std::fs::File::open("/dev/urandom")
        .map_err(|e| format!("failed to open /dev/urandom: {e}"))?;
    f.read_exact(buf)
        .map_err(|e| format!("failed to read OS random bytes: {e}"))
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let ha = blake3::hash(a);
    let hb = blake3::hash(b);
    let mut diff = 0u8;
    for (x, y) in ha.as_bytes().iter().zip(hb.as_bytes().iter()) {
        diff |= x ^ y;
    }
    diff == 0 && a.len() == b.len()
}

fn derive_session_cookie_token(api_key: &str, salt: &[u8; 32]) -> String {
    let mut hasher = blake3::Hasher::new_keyed(salt);
    hasher.update(b"openonyx-web-session-v1:");
    hasher.update(api_key.as_bytes());
    hasher.finalize().to_hex().to_string()
}

// ---------------------------------------------------------------------------
// Proyección JSON del estado de sesión (`onyx.web.state.v1`)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize)]
pub struct WebMemoryEntry {
    pub seq: i64,
    pub tick: i64,
    pub kind: String,
    pub text: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct WebAgentSnapshot {
    pub id: u64,
    pub name: String,
    pub is_root: bool,
    pub state: String,
    pub goal: String,
    pub steps: i64,
    pub max_steps: i64,
    pub memory: Vec<WebMemoryEntry>,
}

#[derive(Clone, Debug, Serialize)]
pub struct WebStreamPreview {
    pub agent_id: u64,
    pub text: String,
    pub thought: String,
    pub thought_chars: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct WebImpactSnapshot {
    pub summary_line: String,
    pub hard_threshold_hit: bool,
    pub max_heat_bp: u32,
    pub callers: usize,
    pub hops: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct WebApprovalSnapshot {
    pub agent_id: u64,
    pub agent_name: String,
    pub request: u64,
    pub tool: String,
    pub args: String,
    pub diff: Option<String>,
    pub impact: Option<WebImpactSnapshot>,
}

#[derive(Clone, Debug, Serialize)]
pub struct WebTelemetrySnapshot {
    pub context_fill_pct: u8,
    pub input_tokens: u64,
    pub cached_tokens: u64,
    pub output_tokens: u64,
    pub thinking_tokens: u64,
    pub estimated_cost_usd: f64,
}

#[derive(Clone, Debug, Serialize)]
pub struct WebSubagentSnapshot {
    pub job_id: String,
    pub name: String,
    pub status: String,
    pub ok: Option<bool>,
    pub spawn_tick: u64,
    pub branch: String,
    pub goal: String,
    pub touched_files: Vec<String>,
    pub summary: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct WebStateSnapshot {
    pub schema: &'static str,
    pub version: u64,
    pub tick: u64,
    pub mode: String,
    pub sandbox_badge: String,
    pub untrusted: bool,
    pub model: String,
    pub effort: String,
    pub vault_label: Option<String>,
    pub focused_agent_id: Option<u64>,
    pub agents: Vec<WebAgentSnapshot>,
    pub subagents: Vec<WebSubagentSnapshot>,
    pub stream_preview: Option<WebStreamPreview>,
    pub pending_approval: Option<WebApprovalSnapshot>,
    pub plan: Option<String>,
    pub services: Option<String>,
    pub telemetry: WebTelemetrySnapshot,
    pub queued_prompts: Vec<String>,
}

// ---------------------------------------------------------------------------
// Hub compartido entre hilos HTTP/SSE y el motor determinista (`WebDriver`)
// ---------------------------------------------------------------------------

struct HubState {
    version: u64,
    snapshot_json: String,
    stream_version: u64,
    stream_json: String,
    state_dirty: bool,
    incoming_events: VecDeque<OnyxEvent>,
    queued_prompts: VecDeque<String>,
    focused_agent_id: Option<u64>,
    last_known_root: Option<u64>,
    active_approval: Option<(ConceptId, u64, String, String)>,
    root_busy: bool,
    tick: u64,
}

struct WebHub {
    api_key: String,
    session_token: String,
    cors_origin: Option<String>,
    workspace_root: Option<PathBuf>,
    policy: Option<WorkspacePolicy>,
    file_index: Mutex<WorkspaceFileIndex>,
    state: Mutex<HubState>,
    state_cv: Condvar,
    input_cv: Condvar,
    quit: Arc<AtomicBool>,
    sse_clients: AtomicUsize,
}

impl WebHub {
    fn new(
        config: &WebServerConfig,
        workspace_root: Option<PathBuf>,
        policy: Option<WorkspacePolicy>,
    ) -> Result<Arc<Self>, String> {
        let mut salt = [0u8; 32];
        fill_os_random(&mut salt)?;
        let session_token = derive_session_cookie_token(&config.api_key, &salt);
        Ok(Arc::new(Self {
            api_key: config.api_key.clone(),
            session_token,
            cors_origin: config.cors_origin.clone(),
            workspace_root,
            policy,
            file_index: Mutex::new(WorkspaceFileIndex::default()),
            state: Mutex::new(HubState {
                version: 0,
                snapshot_json: "{}".to_string(),
                stream_version: 0,
                stream_json: "null".to_string(),
                state_dirty: false,
                incoming_events: VecDeque::new(),
                queued_prompts: VecDeque::new(),
                focused_agent_id: None,
                last_known_root: None,
                active_approval: None,
                root_busy: false,
                tick: 0,
            }),
            state_cv: Condvar::new(),
            input_cv: Condvar::new(),
            quit: Arc::new(AtomicBool::new(false)),
            sse_clients: AtomicUsize::new(0),
        }))
    }

    fn take_state_dirty(&self) -> bool {
        self.state
            .lock()
            .map(|mut g| std::mem::replace(&mut g.state_dirty, false))
            .unwrap_or(false)
    }

    fn verify_api_key(&self, candidate: &str) -> bool {
        constant_time_eq(candidate.as_bytes(), self.api_key.as_bytes())
    }

    fn verify_session_cookie(&self, candidate: &str) -> bool {
        constant_time_eq(candidate.as_bytes(), self.session_token.as_bytes())
    }

    fn push_web_command(&self, raw_cmd: &str) -> Result<(), String> {
        let cmd = raw_cmd.trim();
        if cmd.is_empty() {
            return Err("command cannot be empty".to_string());
        }
        if cmd.starts_with(SUBAGENT_PROPOSAL_CMD_PREFIX.trim_end()) || cmd.starts_with("/__onyx_") {
            return Err("reserved internal command prefix".to_string());
        }
        let mut guard = self
            .state
            .lock()
            .map_err(|_| "web hub lock poisoned".to_string())?;
        if guard.root_busy && !is_immediate_web_slash(cmd) {
            guard.queued_prompts.push_back(cmd.to_string());
            guard.state_dirty = true;
        } else {
            let lower = cmd.to_ascii_lowercase();
            if lower == "/cancel"
                || lower.starts_with("/cancel ")
                || lower == "/stop"
                || lower.starts_with("/stop ")
            {
                guard.queued_prompts.clear();
                guard.active_approval = None;
                guard.state_dirty = true;
            }
            guard
                .incoming_events
                .push_back(OnyxEvent::Ingress(IngressEvent::Command(cmd.to_string())));
        }
        self.input_cv.notify_all();
        Ok(())
    }

    fn push_web_approval(
        &self,
        agent_id: u64,
        request: u64,
        approve: bool,
        reason: &str,
    ) -> Result<(), String> {
        let mut guard = self
            .state
            .lock()
            .map_err(|_| "web hub lock poisoned".to_string())?;
        let Some((active_agent, active_req, _, _)) = guard.active_approval.as_ref() else {
            return Err("no pending approval".to_string());
        };
        if active_agent.0 != agent_id || *active_req != request {
            return Err(format!(
                "approval mismatch (expected agent={} req={}, got agent={} req={})",
                active_agent.0, active_req, agent_id, request
            ));
        }
        let ev = if approve {
            Reply::approve(*active_agent, *active_req).to_event()
        } else {
            let msg = if reason.trim().is_empty() {
                "denied by user in Web UI"
            } else {
                reason.trim()
            };
            Reply::deny(*active_agent, *active_req, msg).to_event()
        };
        guard.active_approval = None;
        guard.incoming_events.push_back(ev);
        self.input_cv.notify_all();
        Ok(())
    }

    fn set_focused_agent(&self, agent_id: u64) -> Result<(), String> {
        let mut guard = self
            .state
            .lock()
            .map_err(|_| "web hub lock poisoned".to_string())?;
        guard.focused_agent_id = Some(agent_id);
        guard.state_dirty = true;
        self.input_cv.notify_all();
        Ok(())
    }

    fn clear_queue(&self) -> Result<(), String> {
        let mut guard = self
            .state
            .lock()
            .map_err(|_| "web hub lock poisoned".to_string())?;
        guard.queued_prompts.clear();
        guard.state_dirty = true;
        self.input_cv.notify_all();
        Ok(())
    }

    fn search_files(&self, query: &str) -> Vec<String> {
        let q = query.trim().to_ascii_lowercase();
        let Ok(mut idx) = self.file_index.lock() else {
            return Vec::new();
        };
        let all = idx.files(self.workspace_root.as_deref(), self.policy.as_ref());
        let mut matches: Vec<String> = all
            .iter()
            .filter(|f| q.is_empty() || f.to_ascii_lowercase().contains(&q))
            .take(12)
            .cloned()
            .collect();
        matches.sort();
        matches
    }
}

fn is_immediate_web_slash(cmd: &str) -> bool {
    let lower = cmd.trim().to_ascii_lowercase();
    lower == "/mode"
        || lower.starts_with("/mode ")
        || lower == "/services"
        || lower.starts_with("/services ")
        || lower == "/service"
        || lower.starts_with("/service ")
        || lower == "/subagents"
        || lower == "/subagent"
        || lower.starts_with("/merge ")
        || lower.starts_with("/discard ")
        || lower == "/trust"
        || lower.starts_with("/trust ")
        || lower == "/undo"
        || lower == "/cancel"
        || lower.starts_with("/cancel ")
        || lower == "/stop"
        || lower.starts_with("/stop ")
}

// ---------------------------------------------------------------------------
// WebDriver (`PlatformDriver`) y WebHandle (`UiIdlePump`)
// ---------------------------------------------------------------------------

struct WebRuntimeInner {
    vocab: Vocab,
    ws_vocab: Option<WorkspaceVocab>,
    workspace_root: Option<PathBuf>,
    policy: Option<WorkspacePolicy>,
    tools: Option<WorkspaceTools>,
    sprout: Option<SproutSupervisor>,
    subagents: Option<SubagentHandle>,
    mode_handle: WorkspaceModeHandle,
    stream_preview: StreamPreviewHandle,
    telemetry: SessionTelemetryHandle,
    model_name: String,
    reasoning_effort: Option<ReasoningEffort>,
    session_label: Option<String>,
    snapshot_store: Option<SharedPackStore>,
    native_driver: SandboxDriverKind,
    hub: Arc<WebHub>,
    cached_approval_diff: Option<String>,
    cached_approval_impact: Option<ImpactAssessment>,
    cached_approval_key: Option<(ConceptId, u64)>,
    stream_pending: bool,
    last_stream_emit: Instant,
}

/// Handle ligero implementado para `UiIdlePump` (usado por `run_session` en `StopPolicy::Quit`).
#[derive(Clone)]
pub struct WebHandle {
    inner: Arc<Mutex<WebRuntimeInner>>,
    hub: Arc<WebHub>,
}

impl UiIdlePump for WebHandle {
    fn render(&self, graph: &dyn GraphView) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.publish_snapshot(graph, false);
        }
    }

    fn has_queued_events(&self) -> bool {
        self.hub
            .state
            .lock()
            .map(|g| !g.incoming_events.is_empty())
            .unwrap_or(false)
    }

    fn pump_idle(&self, graph: &dyn GraphView) {
        // Espera breve sobre Condvar (35ms): despierta al instante si llega un comando web,
        // sin consumir CPU y sin avanzar `host.ticks()`.
        if let Ok(guard) = self.hub.state.lock() {
            if guard.incoming_events.is_empty() && !self.hub.quit.load(Ordering::Relaxed) {
                let _ = self
                    .hub
                    .input_cv
                    .wait_timeout(guard, Duration::from_millis(35));
            }
        }
        if let Ok(mut inner) = self.inner.lock() {
            let stream_dirty = inner.stream_preview.take_dirty();
            let telemetry_dirty = inner.telemetry.take_dirty();
            let dispatched = inner.dispatch_queued_if_idle(graph);
            let hub_dirty = self.hub.take_state_dirty();

            // Solo serializamos y difundimos `event: state` completo cuando realmente cambió
            // el estado de sesión/telemetría/cola (0 trabajo de grafo en reposo o durante streaming).
            if telemetry_dirty || dispatched || hub_dirty {
                inner.publish_snapshot(graph, true);
            }

            // Los deltas de streaming/pensamiento viajan por `event: stream` ligero con throttle (~120 ms).
            if stream_dirty {
                inner.stream_pending = true;
            }
            if inner.stream_pending
                && inner.last_stream_emit.elapsed() >= Duration::from_millis(STREAM_SSE_THROTTLE_MS)
            {
                inner.stream_pending = false;
                inner.last_stream_emit = Instant::now();
                inner.publish_stream_preview(graph);
            }
        }
    }
}

/// Driver determinista para `openonyx serve` que proyecta el grafo a clientes Web/SSE.
pub struct WebDriver {
    inner: Arc<Mutex<WebRuntimeInner>>,
    hub: Arc<WebHub>,
    local_addr: SocketAddr,
}

impl WebDriver {
    pub fn start(
        vocab: Vocab,
        config: &WebServerConfig,
        workspace_root: Option<PathBuf>,
        policy: Option<WorkspacePolicy>,
    ) -> Result<Self, String> {
        let listener = TcpListener::bind(config.bind_addr)
            .map_err(|e| format!("failed to bind `{}`: {e}", config.bind_addr))?;
        let local_addr = listener
            .local_addr()
            .map_err(|e| format!("failed to read bound address: {e}"))?;
        listener
            .set_nonblocking(true)
            .map_err(|e| format!("failed to configure listener: {e}"))?;

        let hub = WebHub::new(config, workspace_root.clone(), policy.clone())?;
        let native_driver = detect_sandbox_capabilities().native_driver;
        let inner = Arc::new(Mutex::new(WebRuntimeInner {
            vocab,
            ws_vocab: None,
            workspace_root,
            policy,
            tools: None,
            sprout: None,
            subagents: None,
            mode_handle: WorkspaceModeHandle::default(),
            stream_preview: StreamPreviewHandle::default(),
            telemetry: SessionTelemetryHandle::default(),
            model_name: String::new(),
            reasoning_effort: None,
            session_label: None,
            snapshot_store: None,
            native_driver,
            hub: Arc::clone(&hub),
            cached_approval_diff: None,
            cached_approval_impact: None,
            cached_approval_key: None,
            stream_pending: false,
            last_stream_emit: Instant::now()
                .checked_sub(Duration::from_secs(1))
                .unwrap_or_else(Instant::now),
        }));

        let accept_hub = Arc::clone(&hub);
        thread::spawn(move || accept_loop(listener, accept_hub));

        Ok(Self {
            inner,
            hub,
            local_addr,
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub fn with_mode_handle(self, mode_handle: WorkspaceModeHandle) -> Self {
        if let Ok(mut inner) = self.inner.lock() {
            inner.mode_handle = mode_handle;
        }
        self
    }

    pub fn with_tools(self, tools: WorkspaceTools) -> Self {
        if let Ok(mut inner) = self.inner.lock() {
            inner.tools = Some(tools);
        }
        self
    }

    pub fn with_sprout(self, sprout: SproutSupervisor) -> Self {
        if let Ok(mut inner) = self.inner.lock() {
            inner.sprout = Some(sprout);
        }
        self
    }

    pub fn with_subagents(self, subagents: SubagentHandle) -> Self {
        if let Ok(mut inner) = self.inner.lock() {
            inner.subagents = Some(subagents);
        }
        self
    }

    pub fn with_workspace_vocab(self, ws_vocab: WorkspaceVocab) -> Self {
        if let Ok(mut inner) = self.inner.lock() {
            inner.ws_vocab = Some(ws_vocab);
        }
        self
    }

    pub fn with_stream_preview(self, stream_preview: StreamPreviewHandle) -> Self {
        if let Ok(mut inner) = self.inner.lock() {
            inner.stream_preview = stream_preview;
        }
        self
    }

    pub fn with_telemetry(self, telemetry: SessionTelemetryHandle) -> Self {
        if let Ok(mut inner) = self.inner.lock() {
            inner.telemetry = telemetry;
        }
        self
    }

    pub fn with_model_info(
        self,
        model: impl Into<String>,
        effort: Option<ReasoningEffort>,
    ) -> Self {
        if let Ok(mut inner) = self.inner.lock() {
            inner.model_name = model.into();
            inner.reasoning_effort = effort;
        }
        self
    }

    pub fn with_session_storage(
        self,
        session_label: Option<String>,
        snapshot_store: Option<SharedPackStore>,
    ) -> Self {
        if let Ok(mut inner) = self.inner.lock() {
            inner.session_label = session_label;
            inner.snapshot_store = snapshot_store;
        }
        self
    }

    pub fn handle(&self) -> WebHandle {
        WebHandle {
            inner: Arc::clone(&self.inner),
            hub: Arc::clone(&self.hub),
        }
    }

    pub fn quit_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.hub.quit)
    }
}

impl Drop for WebDriver {
    fn drop(&mut self) {
        self.hub.quit.store(true, Ordering::Relaxed);
        self.hub.state_cv.notify_all();
        self.hub.input_cv.notify_all();
    }
}

impl PlatformDriver for WebDriver {
    fn poll(&mut self, bus: &mut dyn EventBus) {
        if let Ok(mut guard) = self.hub.state.lock() {
            while let Some(ev) = guard.incoming_events.pop_front() {
                bus.push(ev);
            }
        }
    }

    fn present(&mut self, graph: &dyn GraphView, _surfaces: &[SurfaceBinding]) {
        if let Ok(mut inner) = self.inner.lock() {
            if let Ok(mut g) = self.hub.state.lock() {
                g.tick = g.tick.saturating_add(1);
            }
            inner.publish_snapshot(graph, true);
        }
    }
}

impl WebRuntimeInner {
    fn active_session_label(&self) -> Option<String> {
        self.snapshot_store
            .as_ref()
            .and_then(|st| {
                let dir = st.current_dir();
                self.workspace_root
                    .as_deref()
                    .and_then(|ws| session_display_label(ws, &dir))
                    .or_else(|| dir.file_name().and_then(|n| n.to_str()).map(str::to_string))
            })
            .or_else(|| self.session_label.clone())
    }

    fn dispatch_queued_if_idle(&self, graph: &dyn GraphView) -> bool {
        let root_busy = is_root_agent_busy(graph, &self.vocab);
        if let Ok(mut guard) = self.hub.state.lock() {
            guard.root_busy = root_busy || guard.active_approval.is_some();
            if !guard.root_busy && guard.incoming_events.is_empty() {
                if let Some(next_prompt) = guard.queued_prompts.pop_front() {
                    guard
                        .incoming_events
                        .push_back(OnyxEvent::Ingress(IngressEvent::Command(next_prompt)));
                    return true;
                }
            }
        }
        false
    }

    /// Difunde solo el delta ligero `event: stream` (~0.5-4 KB) sin recorrer ni serializar
    /// todo el grafo EAV en cada token de streaming o pensamiento.
    fn publish_stream_preview(&self, graph: &dyn GraphView) {
        let preview = self.stream_preview.get().and_then(|sp| {
            let state = prop_text(graph, ConceptId(sp.agent), self.vocab.state);
            if state.as_deref() == Some("thinking")
                && (!sp.text.is_empty() || !sp.thought.is_empty())
            {
                Some(WebStreamPreview {
                    agent_id: sp.agent,
                    text: sp.text,
                    thought_chars: sp.thought.chars().count(),
                    thought: tail_utf8(&sp.thought, MAX_STREAM_THOUGHT_BYTES),
                })
            } else {
                None
            }
        });
        let Ok(json) = serde_json::to_string(&preview) else {
            return;
        };
        if let Ok(mut guard) = self.hub.state.lock() {
            if guard.stream_json != json {
                guard.stream_json = json;
                guard.stream_version = guard.stream_version.saturating_add(1);
                self.hub.state_cv.notify_all();
            }
        }
    }

    fn publish_snapshot(&mut self, graph: &dyn GraphView, force_notify: bool) {
        let agents = collect_agents(graph, &self.vocab);
        let root_busy = is_root_agent_busy(graph, &self.vocab);

        // Evaluar aprobación pendiente (igual que TuiDriver: respeta policy + Scent-Heated Approval).
        let approvals = pending_approvals(graph, &self.vocab);
        let current_mode = self.mode_handle.get();
        let mut active_approval_snapshot: Option<WebApprovalSnapshot> = None;
        let mut active_approval_tuple: Option<(ConceptId, u64, String, String)> = None;

        for a in &approvals {
            let Some(p) = &a.pending_approval else {
                continue;
            };
            let mut merge_preview_diff: Option<String> = None;
            let impact_assessment = if p.tool == "subagent_merge" {
                let static_dec = self.policy.as_ref().map_or(PermissionDecision::Ask, |pol| {
                    pol.evaluate_permission_in_mode(current_mode, &p.tool, &p.args)
                });
                if static_dec == PermissionDecision::Deny {
                    continue;
                }
                let job_id = serde_json::from_str::<serde_json::Value>(&p.args)
                    .ok()
                    .and_then(|v| v.get("job_id").and_then(|j| j.as_str()).map(str::to_string));
                let preview = job_id.and_then(|jid| {
                    self.subagents
                        .as_ref()
                        .and_then(|h| h.preview_merge_for_request(&jid, a.agent.0, a.request).ok())
                });
                match preview {
                    Some(MergePreviewState::Clean(ticket)) => {
                        if apply_impact_escalation(static_dec, ticket.impact.as_ref())
                            != PermissionDecision::Ask
                        {
                            continue;
                        }
                        merge_preview_diff =
                            Some(truncate_web_diff_lines(&ticket.unified_diff, 120));
                        ticket.impact
                    }
                    Some(MergePreviewState::NoOp) => {
                        if static_dec != PermissionDecision::Ask {
                            continue;
                        }
                        None
                    }
                    Some(MergePreviewState::Conflict { .. }) => continue,
                    None => {
                        if static_dec != PermissionDecision::Ask {
                            continue;
                        }
                        None
                    }
                }
            } else if let Some(tools) = &self.tools {
                let (decision, impact) =
                    tools.evaluate_permission_with_impact(current_mode, &p.tool, &p.args);
                if decision != PermissionDecision::Ask {
                    continue;
                }
                impact
            } else if let Some(policy) = &self.policy {
                if policy.evaluate_permission_in_mode(current_mode, &p.tool, &p.args)
                    != PermissionDecision::Ask
                {
                    continue;
                }
                None
            } else {
                None
            };

            let key = (a.agent, a.request);
            if self.cached_approval_key != Some(key) {
                self.cached_approval_diff = merge_preview_diff.or_else(|| {
                    self.workspace_root
                        .as_deref()
                        .and_then(|root| preview_tool_diff(root, &p.tool, &p.args, 120))
                });
                self.cached_approval_impact = impact_assessment;
                self.cached_approval_key = Some(key);
            }

            let agent_name = prop_text(graph, a.agent, self.vocab.name).unwrap_or_default();
            active_approval_tuple = Some((a.agent, a.request, p.tool.clone(), p.args.clone()));
            active_approval_snapshot = Some(WebApprovalSnapshot {
                agent_id: a.agent.0,
                agent_name,
                request: a.request,
                tool: p.tool.clone(),
                args: p.args.clone(),
                diff: self.cached_approval_diff.clone(),
                impact: self
                    .cached_approval_impact
                    .as_ref()
                    .map(|imp| WebImpactSnapshot {
                        summary_line: imp.summary_line.clone(),
                        hard_threshold_hit: imp.hard_threshold_hit,
                        max_heat_bp: imp.max_heat_bp,
                        callers: imp.callers,
                        hops: imp.hops,
                    }),
            });
            break;
        }
        if active_approval_snapshot.is_none() {
            self.cached_approval_key = None;
            self.cached_approval_diff = None;
            self.cached_approval_impact = None;
        }

        let plan = self
            .ws_vocab
            .as_ref()
            .and_then(|wv| graph_workspace_plan(graph, wv));
        let services = self
            .ws_vocab
            .as_ref()
            .and_then(|wv| graph_workspace_services(graph, wv))
            .or_else(|| self.sprout.as_ref().and_then(|s| s.render_panel_text()));

        let tel = self.telemetry.snapshot();
        let stream_preview = self.stream_preview.get().and_then(|sp| {
            let is_thinking = agents
                .iter()
                .any(|a| a.id == sp.agent && a.state == "thinking");
            if is_thinking && (!sp.text.is_empty() || !sp.thought.is_empty()) {
                Some(WebStreamPreview {
                    agent_id: sp.agent,
                    text: sp.text,
                    thought_chars: sp.thought.chars().count(),
                    thought: tail_utf8(&sp.thought, MAX_STREAM_THOUGHT_BYTES),
                })
            } else {
                None
            }
        });
        let stream_json_now =
            serde_json::to_string(&stream_preview).unwrap_or_else(|_| "null".to_string());
        let sandbox_badge = self
            .policy
            .as_ref()
            .map(|p| p.sandbox.mode.effective_badge(self.native_driver))
            .unwrap_or("workspace")
            .to_string();
        let untrusted = self
            .policy
            .as_ref()
            .is_some_and(|p| !p.trust.is_fully_trusted());

        let Ok(mut guard) = self.hub.state.lock() else {
            return;
        };
        guard.active_approval = active_approval_tuple;
        guard.root_busy = root_busy || guard.active_approval.is_some();
        if guard.stream_json != stream_json_now {
            guard.stream_json = stream_json_now;
            guard.stream_version = guard.stream_version.saturating_add(1);
        }

        // Resolución de agente enfocado (sigue nuevos root agents sin dejar que subagentes roben foco).
        let latest_root = agents.iter().rfind(|a| a.is_root).map(|a| a.id);
        if latest_root.is_some() && latest_root != guard.last_known_root {
            guard.last_known_root = latest_root;
            guard.focused_agent_id = latest_root;
        } else if guard
            .focused_agent_id
            .is_none_or(|fid| !agents.iter().any(|a| a.id == fid))
        {
            guard.focused_agent_id = latest_root.or_else(|| agents.first().map(|a| a.id));
        }

        let subagents: Vec<WebSubagentSnapshot> = if let Some(ref handle) = self.subagents {
            handle
                .list_summaries()
                .into_iter()
                .map(|s| WebSubagentSnapshot {
                    job_id: s.job_id,
                    name: s.name,
                    status: s.status.to_string(),
                    ok: s.ok,
                    spawn_tick: s.spawn_tick,
                    branch: s.branch_name,
                    goal: s.goal,
                    touched_files: s.touched_files,
                    summary: s.summary,
                })
                .collect()
        } else if let Some(wv) = &self.ws_vocab {
            graph_subagent_records(graph, wv)
                .into_iter()
                .map(|r| WebSubagentSnapshot {
                    job_id: r.job_id.clone(),
                    name: r.job_id,
                    status: r.status,
                    ok: Some(r.ok),
                    spawn_tick: r.spawn_tick,
                    branch: r.branch,
                    goal: String::new(),
                    touched_files: r.touched_files,
                    summary: r.summary,
                })
                .collect()
        } else {
            Vec::new()
        };

        let next_version = guard.version.saturating_add(1);
        let snap = WebStateSnapshot {
            schema: "onyx.web.state.v1",
            version: next_version,
            tick: guard.tick,
            mode: current_mode.as_label().to_string(),
            sandbox_badge,
            untrusted,
            model: self.model_name.clone(),
            effort: self
                .reasoning_effort
                .map(ReasoningEffort::as_str)
                .unwrap_or("auto")
                .to_string(),
            vault_label: self.active_session_label(),
            focused_agent_id: guard.focused_agent_id,
            agents,
            subagents,
            stream_preview,
            pending_approval: active_approval_snapshot,
            plan,
            services,
            telemetry: WebTelemetrySnapshot {
                context_fill_pct: tel.context_fill_pct,
                input_tokens: tel.cumulative.input_tokens,
                cached_tokens: tel.cumulative.cached_input_tokens,
                output_tokens: tel.cumulative.output_tokens,
                thinking_tokens: tel.cumulative.reasoning_tokens,
                estimated_cost_usd: tel.estimated_cost_usd,
            },
            queued_prompts: guard.queued_prompts.iter().cloned().collect(),
        };

        if let Ok(json) = serde_json::to_string(&snap) {
            // Evitar emitir eventos SSE redundantes comparando contenido sin el campo `version`.
            let changed = force_notify
                || guard.version == 0
                || strip_version(&json) != strip_version(&guard.snapshot_json);
            if changed {
                guard.version = next_version;
                guard.snapshot_json = json;
                self.hub.state_cv.notify_all();
            }
        }
    }
}

fn strip_version(json: &str) -> &str {
    match json.find(",\"tick\":") {
        Some(idx) => &json[idx..],
        None => json,
    }
}

fn truncate_web_diff_lines(diff: &str, max_lines: usize) -> String {
    let lines: Vec<&str> = diff.lines().collect();
    if lines.len() <= max_lines {
        diff.to_string()
    } else {
        let mut head = lines[..max_lines].join("\n");
        head.push_str(&format!("\n... (+{} more lines)", lines.len() - max_lines));
        head
    }
}

fn tail_utf8(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut start = s.len().saturating_sub(max_bytes);
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    format!("…{}", &s[start..])
}

fn prop_text(graph: &dyn GraphView, id: ConceptId, attr: onyx_soul_agents::Attr) -> Option<String> {
    match graph.get(id, attr) {
        Some(Value::Text(s)) => Some(s),
        _ => None,
    }
}

fn is_root_agent_busy(graph: &dyn GraphView, vocab: &Vocab) -> bool {
    let latest_root = graph
        .query(&[vocab.state])
        .into_iter()
        .filter(|id| graph.related(*id, vocab.rel_parent).is_empty())
        .last();
    latest_root.is_some_and(|id| {
        matches!(
            prop_text(graph, id, vocab.state).as_deref(),
            Some("thinking" | "acting" | "waiting_children" | "waiting_approval")
        )
    })
}

fn collect_agents(graph: &dyn GraphView, vocab: &Vocab) -> Vec<WebAgentSnapshot> {
    let mut ids = graph.query(&[vocab.state]);
    ids.sort_by_key(|c| c.0);
    ids.into_iter()
        .map(|id| {
            let name =
                prop_text(graph, id, vocab.name).unwrap_or_else(|| format!("agent-{}", id.0));
            let state = prop_text(graph, id, vocab.state).unwrap_or_default();
            let goal = prop_text(graph, id, vocab.goal).unwrap_or_default();
            let steps = match graph.get(id, vocab.steps) {
                Some(Value::Int(n)) => n,
                _ => 0,
            };
            let max_steps = match graph.get(id, vocab.max_steps) {
                Some(Value::Int(n)) => n,
                _ => 0,
            };
            let is_root = graph.related(id, vocab.rel_parent).is_empty();

            let mut mem_rows: Vec<(i64, i64, String, String)> = graph
                .related(id, vocab.rel_memory)
                .into_iter()
                .filter_map(|m| {
                    let seq = match graph.get(m, vocab.mem_seq) {
                        Some(Value::Int(n)) => n,
                        _ => return None,
                    };
                    let tick = match graph.get(m, vocab.mem_tick) {
                        Some(Value::Int(n)) => n,
                        _ => 0,
                    };
                    let kind = prop_text(graph, m, vocab.mem_kind).unwrap_or_default();
                    let text = prop_text(graph, m, vocab.mem_text).unwrap_or_default();
                    Some((seq, tick, kind, text))
                })
                .collect();
            mem_rows.sort_by_key(|(seq, _, _, _)| *seq);

            let mut memory: Vec<WebMemoryEntry> = mem_rows
                .into_iter()
                .map(|(seq, tick, kind, text)| WebMemoryEntry {
                    seq,
                    tick,
                    kind,
                    text,
                })
                .collect();
            if let Some(res) = prop_text(graph, id, vocab.result) {
                if !res.is_empty() && memory.last().map(|m| m.text.as_str()) != Some(res.as_str()) {
                    let next_seq = memory.last().map(|m| m.seq.saturating_add(1)).unwrap_or(1);
                    let last_tick = memory.last().map(|m| m.tick).unwrap_or(0);
                    memory.push(WebMemoryEntry {
                        seq: next_seq,
                        tick: last_tick,
                        kind: "say".to_string(),
                        text: res,
                    });
                }
            }

            WebAgentSnapshot {
                id: id.0,
                name,
                is_root,
                state,
                goal,
                steps,
                max_steps,
                memory,
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Servidor HTTP/1.1 + SSE sobre `std::net::TcpListener` (0 `tokio`)
// ---------------------------------------------------------------------------

struct HttpRequest {
    method: String,
    path: String,
    query: String,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

fn accept_loop(listener: TcpListener, hub: Arc<WebHub>) {
    while !hub.quit.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _addr)) => {
                let conn_hub = Arc::clone(&hub);
                thread::spawn(move || {
                    let _ = handle_connection(stream, conn_hub);
                });
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(20));
            }
            Err(_) => {
                thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

fn handle_connection(mut stream: TcpStream, hub: Arc<WebHub>) -> io::Result<()> {
    // En Windows (Winsock2) y BSD/macOS, los sockets aceptados heredan el modo no bloqueante
    // de `TcpListener::set_nonblocking(true)`. Sin desactivarlo aquí, `read_line()` falla al
    // instante con `WSAEWOULDBLOCK` antes de que lleguen los bytes HTTP del navegador.
    stream.set_nonblocking(false)?;
    let _ = stream.set_nodelay(true);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));

    let Some(req) = read_http_request(stream.try_clone()?)? else {
        return Ok(());
    };
    if let Some(sse_extra) = dispatch_http_request(&req, &hub, &mut stream)? {
        return handle_sse_stream(stream, hub, &sse_extra);
    }
    let _ = stream.shutdown(std::net::Shutdown::Write);
    Ok(())
}

/// Despacha una petición HTTP/1.1 sobre cualquier `Write` (socket real o buffer en memoria en tests).
/// Devuelve `Ok(Some(extra_headers))` cuando la ruta es `GET /api/v1/events` autenticado (SSE).
fn dispatch_http_request<W: Write>(
    req: &HttpRequest,
    hub: &Arc<WebHub>,
    out: &mut W,
) -> io::Result<Option<Vec<(&'static str, String)>>> {
    let cors_header = resolve_cors_origin(req, hub.cors_origin.as_deref());

    // Preflight CORS (`OPTIONS`)
    if req.method == "OPTIONS" {
        let mut extra = vec![
            ("Allow", "GET, POST, OPTIONS".to_string()),
            (
                "Access-Control-Allow-Methods",
                "GET, POST, OPTIONS".to_string(),
            ),
            (
                "Access-Control-Allow-Headers",
                "Content-Type, Authorization".to_string(),
            ),
        ];
        if let Some(orig) = cors_header {
            extra.push(("Access-Control-Allow-Origin", orig));
            extra.push(("Access-Control-Allow-Credentials", "true".to_string()));
        }
        write_http_response(
            out,
            204,
            "No Content",
            "text/plain; charset=utf-8",
            b"",
            &extra,
        )?;
        return Ok(None);
    }

    let mut base_extra: Vec<(&'static str, String)> = Vec::new();
    if let Some(orig) = cors_header.clone() {
        base_extra.push(("Access-Control-Allow-Origin", orig));
        base_extra.push(("Access-Control-Allow-Credentials", "true".to_string()));
    }

    // Assets estáticos PWA (sin datos sensibles de sesión)
    if req.method == "GET" {
        match req.path.as_str() {
            "/" | "/index.html" => {
                write_http_response(
                    out,
                    200,
                    "OK",
                    "text/html; charset=utf-8",
                    ASSET_INDEX_HTML.as_bytes(),
                    &base_extra,
                )?;
                return Ok(None);
            }
            "/styles.css" => {
                write_http_response(
                    out,
                    200,
                    "OK",
                    "text/css; charset=utf-8",
                    ASSET_STYLES_CSS.as_bytes(),
                    &base_extra,
                )?;
                return Ok(None);
            }
            "/app.js" => {
                write_http_response(
                    out,
                    200,
                    "OK",
                    "application/javascript; charset=utf-8",
                    ASSET_APP_JS.as_bytes(),
                    &base_extra,
                )?;
                return Ok(None);
            }
            "/manifest.webmanifest" => {
                write_http_response(
                    out,
                    200,
                    "OK",
                    "application/manifest+json; charset=utf-8",
                    ASSET_MANIFEST.as_bytes(),
                    &base_extra,
                )?;
                return Ok(None);
            }
            "/icon.svg" => {
                write_http_response(
                    out,
                    200,
                    "OK",
                    "image/svg+xml; charset=utf-8",
                    ASSET_ICON_SVG.as_bytes(),
                    &base_extra,
                )?;
                return Ok(None);
            }
            "/logo.jpg" => {
                write_http_response(out, 200, "OK", "image/jpeg", ASSET_LOGO_JPG, &base_extra)?;
                return Ok(None);
            }
            "/favicon.ico" => {
                write_http_response(out, 204, "No Content", "image/x-icon", b"", &base_extra)?;
                return Ok(None);
            }
            "/api/v1/health" => {
                let body = serde_json::json!({
                    "status": "ok",
                    "version": env!("CARGO_PKG_VERSION"),
                    "auth_required": true,
                })
                .to_string();
                write_http_response(
                    out,
                    200,
                    "OK",
                    "application/json; charset=utf-8",
                    body.as_bytes(),
                    &base_extra,
                )?;
                return Ok(None);
            }
            _ => {}
        }
    }

    // Fetch Metadata CSRF guard en rutas mutantes `POST` (antes de auth per modern-web-guidance `security`).
    if req.method == "POST" {
        if let Some(site) = req.headers.get("sec-fetch-site") {
            let s = site.trim().to_ascii_lowercase();
            let has_bearer = extract_bearer_token(req).is_some();
            let allowed_site = matches!(s.as_str(), "same-origin" | "none")
                || (has_bearer && cors_header.is_some());
            if !allowed_site {
                write_json_error(
                    out,
                    403,
                    "Forbidden",
                    "cross-site POST blocked by Sec-Fetch-Site policy",
                    &base_extra,
                )?;
                return Ok(None);
            }
        }
    }

    // Autenticación login / logout
    if req.method == "POST" && req.path == "/api/v1/auth/login" {
        #[derive(Deserialize)]
        struct LoginBody {
            api_key: String,
        }
        let parsed: LoginBody = match serde_json::from_slice(&req.body) {
            Ok(b) => b,
            Err(_) => {
                write_json_error(
                    out,
                    400,
                    "Bad Request",
                    "expected JSON `{ \"api_key\": \"...\" }`",
                    &base_extra,
                )?;
                return Ok(None);
            }
        };
        if !hub.verify_api_key(parsed.api_key.trim()) {
            write_json_error(out, 401, "Unauthorized", "invalid API key", &base_extra)?;
            return Ok(None);
        }
        let is_https_proxy = req
            .headers
            .get("x-forwarded-proto")
            .is_some_and(|v| v.trim().eq_ignore_ascii_case("https"));
        let cookie = if is_https_proxy {
            format!(
                "__Host-onyx_session={}; HttpOnly; SameSite=Lax; Secure; Path=/",
                hub.session_token
            )
        } else {
            format!(
                "onyx_session={}; HttpOnly; SameSite=Lax; Path=/",
                hub.session_token
            )
        };
        base_extra.push(("Set-Cookie", cookie));
        write_http_response(
            out,
            200,
            "OK",
            "application/json; charset=utf-8",
            br#"{"ok":true}"#,
            &base_extra,
        )?;
        return Ok(None);
    }

    if req.method == "POST" && req.path == "/api/v1/auth/logout" {
        base_extra.push((
            "Set-Cookie",
            "onyx_session=; Max-Age=0; HttpOnly; SameSite=Lax; Path=/".to_string(),
        ));
        base_extra.push((
            "Set-Cookie",
            "__Host-onyx_session=; Max-Age=0; HttpOnly; SameSite=Lax; Secure; Path=/".to_string(),
        ));
        base_extra.push((
            "Clear-Site-Data",
            "\"cookies\", \"storage\", \"cache\"".to_string(),
        ));
        write_http_response(
            out,
            200,
            "OK",
            "application/json; charset=utf-8",
            br#"{"ok":true}"#,
            &base_extra,
        )?;
        return Ok(None);
    }

    // A partir de aquí, todas las rutas `/api/v1/...` exigen autenticación válida (Bearer o cookie).
    if !is_authenticated(req, hub) {
        base_extra.push(("WWW-Authenticate", "Bearer realm=\"openOnyx\"".to_string()));
        write_json_error(
            out,
            401,
            "Unauthorized",
            "authentication required",
            &base_extra,
        )?;
        return Ok(None);
    }

    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/api/v1/state") => {
            let body = hub
                .state
                .lock()
                .map(|g| g.snapshot_json.clone())
                .unwrap_or_else(|_| "{}".to_string());
            write_http_response(
                out,
                200,
                "OK",
                "application/json; charset=utf-8",
                body.as_bytes(),
                &base_extra,
            )?;
            Ok(None)
        }
        ("GET", "/api/v1/events") => Ok(Some(base_extra)),
        ("GET", "/api/v1/files") => {
            let query = parse_query_param(&req.query, "q").unwrap_or_default();
            let files = hub.search_files(&query);
            let body = serde_json::json!({ "files": files }).to_string();
            write_http_response(
                out,
                200,
                "OK",
                "application/json; charset=utf-8",
                body.as_bytes(),
                &base_extra,
            )?;
            Ok(None)
        }
        ("POST", "/api/v1/command") => {
            #[derive(Deserialize)]
            struct CommandBody {
                command: String,
            }
            let parsed: CommandBody = match serde_json::from_slice(&req.body) {
                Ok(b) => b,
                Err(_) => {
                    write_json_error(
                        out,
                        400,
                        "Bad Request",
                        "expected JSON `{ \"command\": \"...\" }`",
                        &base_extra,
                    )?;
                    return Ok(None);
                }
            };
            match hub.push_web_command(&parsed.command) {
                Ok(()) => write_http_response(
                    out,
                    200,
                    "OK",
                    "application/json; charset=utf-8",
                    br#"{"ok":true}"#,
                    &base_extra,
                )?,
                Err(msg) => write_json_error(out, 400, "Bad Request", &msg, &base_extra)?,
            }
            Ok(None)
        }
        ("POST", "/api/v1/approval") => {
            #[derive(Deserialize)]
            struct ApprovalBody {
                agent_id: u64,
                request: u64,
                approve: bool,
                #[serde(default)]
                reason: String,
            }
            let parsed: ApprovalBody = match serde_json::from_slice(&req.body) {
                Ok(b) => b,
                Err(_) => {
                    write_json_error(
                        out,
                        400,
                        "Bad Request",
                        "expected JSON `{ \"agent_id\", \"request\", \"approve\" }`",
                        &base_extra,
                    )?;
                    return Ok(None);
                }
            };
            match hub.push_web_approval(
                parsed.agent_id,
                parsed.request,
                parsed.approve,
                &parsed.reason,
            ) {
                Ok(()) => write_http_response(
                    out,
                    200,
                    "OK",
                    "application/json; charset=utf-8",
                    br#"{"ok":true}"#,
                    &base_extra,
                )?,
                Err(msg) => write_json_error(out, 409, "Conflict", &msg, &base_extra)?,
            }
            Ok(None)
        }
        ("POST", "/api/v1/focus") => {
            #[derive(Deserialize)]
            struct FocusBody {
                agent_id: u64,
            }
            let parsed: FocusBody = match serde_json::from_slice(&req.body) {
                Ok(b) => b,
                Err(_) => {
                    write_json_error(
                        out,
                        400,
                        "Bad Request",
                        "expected JSON `{ \"agent_id\": <n> }`",
                        &base_extra,
                    )?;
                    return Ok(None);
                }
            };
            let _ = hub.set_focused_agent(parsed.agent_id);
            write_http_response(
                out,
                200,
                "OK",
                "application/json; charset=utf-8",
                br#"{"ok":true}"#,
                &base_extra,
            )?;
            Ok(None)
        }
        ("POST", "/api/v1/queue/clear") => {
            let _ = hub.clear_queue();
            write_http_response(
                out,
                200,
                "OK",
                "application/json; charset=utf-8",
                br#"{"ok":true}"#,
                &base_extra,
            )?;
            Ok(None)
        }
        _ => {
            write_json_error(out, 404, "Not Found", "route not found", &base_extra)?;
            Ok(None)
        }
    }
}

fn handle_sse_stream(
    mut stream: TcpStream,
    hub: Arc<WebHub>,
    extra: &[(&str, String)],
) -> io::Result<()> {
    let active = hub.sse_clients.fetch_add(1, Ordering::AcqRel);
    if active >= MAX_SSE_CLIENTS {
        hub.sse_clients.fetch_sub(1, Ordering::AcqRel);
        return write_json_error(
            &mut stream,
            503,
            "Service Unavailable",
            "too many concurrent SSE subscribers",
            extra,
        );
    }
    struct SseGuard(Arc<WebHub>);
    impl Drop for SseGuard {
        fn drop(&mut self) {
            self.0.sse_clients.fetch_sub(1, Ordering::AcqRel);
        }
    }
    let _guard = SseGuard(Arc::clone(&hub));

    let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));
    let mut head = String::from(
        "HTTP/1.1 200 OK\r\n\
         Content-Type: text/event-stream; charset=utf-8\r\n\
         Cache-Control: no-cache, no-store\r\n\
         Connection: keep-alive\r\n\
         X-Content-Type-Options: nosniff\r\n\
         X-Frame-Options: DENY\r\n\
         Referrer-Policy: strict-origin-when-cross-origin\r\n\
         Cross-Origin-Resource-Policy: same-origin\r\n\
         Vary: Sec-Fetch-Dest, Sec-Fetch-Mode, Sec-Fetch-Site, Origin\r\n",
    );
    for (k, v) in extra {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())?;
    stream.flush()?;

    let mut last_ver = 0u64;
    let mut last_stream_ver = 0u64;
    let mut last_ping = Instant::now();

    while !hub.quit.load(Ordering::Relaxed) {
        let (state_payload, stream_payload) = {
            let Ok(mut guard) = hub.state.lock() else {
                break;
            };
            if guard.version == last_ver && guard.stream_version == last_stream_ver {
                let (next_guard, _) = hub
                    .state_cv
                    .wait_timeout(guard, Duration::from_millis(500))
                    .unwrap_or_else(|p| p.into_inner());
                guard = next_guard;
            }
            let st = if guard.version != last_ver {
                last_ver = guard.version;
                // `event: state` already includes `stream_preview`, so advance `last_stream_ver`
                // in lockstep to avoid sending a redundant immediate `event: stream` frame.
                last_stream_ver = guard.stream_version;
                Some(guard.snapshot_json.clone())
            } else {
                None
            };
            let sp = if st.is_none() && guard.stream_version != last_stream_ver {
                last_stream_ver = guard.stream_version;
                Some(guard.stream_json.clone())
            } else {
                None
            };
            (st, sp)
        };

        if let Some(json) = state_payload {
            let frame = format!("event: state\ndata: {json}\n\n");
            if stream.write_all(frame.as_bytes()).is_err() || stream.flush().is_err() {
                break;
            }
            last_ping = Instant::now();
        } else if let Some(json) = stream_payload {
            let frame = format!("event: stream\ndata: {json}\n\n");
            if stream.write_all(frame.as_bytes()).is_err() || stream.flush().is_err() {
                break;
            }
            last_ping = Instant::now();
        } else if last_ping.elapsed() >= Duration::from_secs(15) {
            if stream.write_all(b": ping\n\n").is_err() || stream.flush().is_err() {
                break;
            }
            last_ping = Instant::now();
        }
    }

    Ok(())
}

fn read_http_request<R: Read>(input: R) -> io::Result<Option<HttpRequest>> {
    let mut reader = BufReader::new(input);
    let mut request_line = String::new();
    let n = reader.read_line(&mut request_line)?;
    if n == 0 {
        return Ok(None);
    }
    let trimmed_line = request_line.trim_end_matches(['\r', '\n']);
    let mut parts = trimmed_line.split_whitespace();
    let Some(method) = parts.next() else {
        return Ok(None);
    };
    let Some(target) = parts.next() else {
        return Ok(None);
    };

    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target.to_string(), String::new()),
    };

    let mut headers = BTreeMap::new();
    let mut total_header_bytes = request_line.len();
    loop {
        let mut line = String::new();
        let read_n = reader.read_line(&mut line)?;
        if read_n == 0 {
            break;
        }
        total_header_bytes += read_n;
        if total_header_bytes > MAX_HTTP_HEADER_BYTES {
            return Ok(None);
        }
        let clean = line.trim_end_matches(['\r', '\n']);
        if clean.is_empty() {
            break;
        }
        if let Some((k, v)) = clean.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }

    let content_len = headers
        .get("content-length")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0);
    if content_len > MAX_HTTP_BODY_BYTES {
        return Ok(None);
    }
    let mut body = vec![0u8; content_len];
    if content_len > 0 {
        reader.read_exact(&mut body)?;
    }

    Ok(Some(HttpRequest {
        method: method.to_ascii_uppercase(),
        path,
        query,
        headers,
        body,
    }))
}

fn resolve_cors_origin(req: &HttpRequest, allowed: Option<&str>) -> Option<String> {
    let allow = allowed?;
    let origin = req.headers.get("origin")?.trim().trim_end_matches('/');
    if origin.eq_ignore_ascii_case(allow) {
        Some(allow.to_string())
    } else {
        None
    }
}

fn extract_bearer_token(req: &HttpRequest) -> Option<&str> {
    let auth = req.headers.get("authorization")?;
    let rest = auth
        .strip_prefix("Bearer ")
        .or_else(|| auth.strip_prefix("bearer "))?;
    let trimmed = rest.trim();
    (!trimmed.is_empty()).then_some(trimmed)
}

fn extract_session_cookie(req: &HttpRequest) -> Option<&str> {
    let raw = req.headers.get("cookie")?;
    for pair in raw.split(';') {
        let Some((k, v)) = pair.trim().split_once('=') else {
            continue;
        };
        let name = k.trim();
        if name == "__Host-onyx_session" || name == "onyx_session" {
            let val = v.trim();
            if !val.is_empty() {
                return Some(val);
            }
        }
    }
    None
}

fn is_authenticated(req: &HttpRequest, hub: &WebHub) -> bool {
    if let Some(bearer) = extract_bearer_token(req) {
        if hub.verify_api_key(bearer) {
            return true;
        }
    }
    if let Some(cookie) = extract_session_cookie(req) {
        if hub.verify_session_cookie(cookie) {
            return true;
        }
    }
    false
}

fn parse_query_param(query: &str, key: &str) -> Option<String> {
    for pair in query.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            if k == key {
                return Some(percent_decode(v));
            }
        }
    }
    None
}

fn percent_decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(hex) = std::str::from_utf8(&bytes[i + 1..i + 3]) {
                if let Ok(val) = u8::from_str_radix(hex, 16) {
                    out.push(val);
                    i += 3;
                    continue;
                }
            }
        } else if bytes[i] == b'+' {
            out.push(b' ');
            i += 1;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn write_json_error<W: Write>(
    stream: &mut W,
    status: u16,
    reason: &str,
    msg: &str,
    extra: &[(&str, String)],
) -> io::Result<()> {
    let body = serde_json::json!({ "error": msg }).to_string();
    write_http_response(
        stream,
        status,
        reason,
        "application/json; charset=utf-8",
        body.as_bytes(),
        extra,
    )
}

fn write_http_response<W: Write>(
    stream: &mut W,
    status: u16,
    reason: &str,
    content_type: &str,
    body: &[u8],
    extra: &[(&str, String)],
) -> io::Result<()> {
    let mut head = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         Cache-Control: no-store\r\n\
         Content-Security-Policy: {CSP_HEADER}\r\n\
         X-Frame-Options: DENY\r\n\
         X-Content-Type-Options: nosniff\r\n\
         Referrer-Policy: strict-origin-when-cross-origin\r\n\
         Permissions-Policy: camera=(), geolocation=(), microphone=(), focus-without-user-activation=()\r\n\
         Cross-Origin-Opener-Policy: same-origin-allow-popups\r\n\
         Cross-Origin-Resource-Policy: same-origin\r\n\
         Vary: Sec-Fetch-Dest, Sec-Fetch-Mode, Sec-Fetch-Site, Origin\r\n",
        body.len()
    );
    for (k, v) in extra {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())?;
    if !body.is_empty() {
        stream.write_all(body)?;
    }
    stream.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use onyx_soul_agents::{AgentConfig, AgentHost, AgentView, RecordingBridge, Reply};
    use onyx_workspace::{WorkspaceLedgerSystem, WorkspaceMode, WorkspaceToolBridge};

    fn in_memory_exchange(hub: &Arc<WebHub>, raw_req: &str) -> (String, bool) {
        let req = read_http_request(io::Cursor::new(raw_req.as_bytes()))
            .unwrap()
            .expect("valid HTTP request");
        let mut buf = Vec::new();
        let sse = dispatch_http_request(&req, hub, &mut buf)
            .unwrap()
            .is_some();
        (String::from_utf8(buf).unwrap(), sse)
    }

    #[test]
    fn serve_rejects_non_loopback_bind_without_explicit_api_key() {
        assert!(WebServerConfig::resolve("0.0.0.0:8787", None, None).is_err());
        assert!(WebServerConfig::resolve("0.0.0.0:8787", Some("short".to_string()), None).is_err());
        let ok = WebServerConfig::resolve(
            "0.0.0.0:8787",
            Some("super-secret-vps-key-2026".to_string()),
            Some("https://ui.example.com".to_string()),
        )
        .unwrap();
        assert!(!ok.generated_key);
        assert_eq!(ok.cors_origin.as_deref(), Some("https://ui.example.com"));

        // Wildcard CORS MUST be rejected.
        assert!(WebServerConfig::resolve("127.0.0.1:0", None, Some("*".to_string())).is_err());

        // Loopback without explicit key generates an ephemeral 256-bit key.
        let local = WebServerConfig::resolve("127.0.0.1:0", None, None).unwrap();
        assert!(local.generated_key);
        assert!(local.api_key.starts_with("onyx_") && local.api_key.len() == 69);
    }

    #[test]
    fn serve_auth_login_cookie_bearer_cors_csrf_logout_and_security_headers() {
        let cfg = WebServerConfig::resolve(
            "127.0.0.1:0",
            Some("test-api-key-1234567890".to_string()),
            Some("http://localhost:5173".to_string()),
        )
        .unwrap();
        let host = AgentHost::builder(1).build().unwrap();
        let driver = WebDriver::start(host.vocab(), &cfg, None, None).unwrap();
        let hub = Arc::clone(&driver.hub);
        driver.handle().render(host.graph());

        // 1. Unauthenticated /api/v1/state returns 401.
        let (unauth, sse) = in_memory_exchange(
            &hub,
            "GET /api/v1/state HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n",
        );
        assert!(!sse);
        assert!(unauth.starts_with("HTTP/1.1 401 Unauthorized"), "{unauth}");

        // 2. PWA root serves index.html with strict CSP + security headers + native <dialog>.
        let (root, _) = in_memory_exchange(&hub, "GET / HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n");
        assert!(root.starts_with("HTTP/1.1 200 OK"), "{root}");
        assert!(
            root.contains("require-trusted-types-for 'script'"),
            "{root}"
        );
        assert!(root.contains("X-Frame-Options: DENY"), "{root}");
        assert!(root.contains("X-Content-Type-Options: nosniff"), "{root}");
        assert!(root.contains("<dialog id=\"login-dialog\""), "{root}");
        assert!(root.contains("src=\"/logo.jpg\""), "{root}");
        assert!(root.contains("fetchpriority=\"high\""), "{root}");
        assert!(
            root.contains("<progress id=\"agent-busy-progress\""),
            "{root}"
        );
        assert!(root.contains("id=\"trace-mode-btn\""), "{root}");
        assert!(
            ASSET_STYLES_CSS.contains("flex-shrink: 0"),
            "styles.css must prevent flexbox from collapsing trace cards to 0px"
        );
        assert!(
            !ASSET_APP_JS.contains("innerHTML"),
            "app.js must never use innerHTML"
        );

        // 2b. Embedded real openOnyx logo is served with image/jpeg and exact byte payload.
        let logo_req = read_http_request(io::Cursor::new(
            b"GET /logo.jpg HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n".as_slice(),
        ))
        .unwrap()
        .expect("valid HTTP request");
        let mut logo_buf = Vec::new();
        assert!(
            dispatch_http_request(&logo_req, &hub, &mut logo_buf)
                .unwrap()
                .is_none()
        );
        let split_pos = logo_buf
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .expect("HTTP header separator");
        let logo_head = std::str::from_utf8(&logo_buf[..split_pos]).unwrap();
        let logo_body = &logo_buf[split_pos + 4..];
        assert!(logo_head.starts_with("HTTP/1.1 200 OK"), "{logo_head}");
        assert!(
            logo_head.contains("Content-Type: image/jpeg"),
            "{logo_head}"
        );
        assert_eq!(logo_body.len(), ASSET_LOGO_JPG.len());
        assert_eq!(logo_body, ASSET_LOGO_JPG);

        // 3. Cross-site POST without Bearer is blocked (403) by Sec-Fetch-Site.
        let (csrf, _) = in_memory_exchange(
            &hub,
            "POST /api/v1/command HTTP/1.1\r\nHost: 127.0.0.1\r\nSec-Fetch-Site: cross-site\r\nContent-Length: 18\r\n\r\n{\"command\":\"hi!!\"}",
        );
        assert!(csrf.starts_with("HTTP/1.1 403 Forbidden"), "{csrf}");

        // 3b. Spoofed internal subagent proposal command prefix is blocked (400).
        let (spoof, _) = in_memory_exchange(
            &hub,
            "POST /api/v1/command HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer test-api-key-1234567890\r\nContent-Length: 41\r\n\r\n{\"command\":\"/__onyx_subagent_proposal {}\"}",
        );
        assert!(spoof.starts_with("HTTP/1.1 400 Bad Request"), "{spoof}");

        // 4. Login issues HttpOnly cookie and grants /api/v1/state access.
        let login_body = r#"{"api_key":"test-api-key-1234567890"}"#;
        let login_req = format!(
            "POST /api/v1/auth/login HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: {}\r\n\r\n{}",
            login_body.len(),
            login_body
        );
        let (login_resp, _) = in_memory_exchange(&hub, &login_req);
        assert!(login_resp.starts_with("HTTP/1.1 200 OK"), "{login_resp}");
        let set_cookie = login_resp
            .lines()
            .find(|l| l.starts_with("Set-Cookie: onyx_session="))
            .expect("expected HttpOnly session cookie");
        assert!(set_cookie.contains("HttpOnly") && set_cookie.contains("SameSite=Lax"));
        let cookie_pair = set_cookie
            .trim_start_matches("Set-Cookie: ")
            .split(';')
            .next()
            .unwrap();

        let (authed_cookie, _) = in_memory_exchange(
            &hub,
            &format!(
                "GET /api/v1/state HTTP/1.1\r\nHost: 127.0.0.1\r\nCookie: {cookie_pair}\r\n\r\n"
            ),
        );
        assert!(
            authed_cookie.starts_with("HTTP/1.1 200 OK"),
            "{authed_cookie}"
        );
        assert!(
            authed_cookie.contains("\"schema\":\"onyx.web.state.v1\""),
            "{authed_cookie}"
        );

        // 5. HTTPS proxy login issues `__Host-onyx_session` with `Secure`.
        let https_login_req = format!(
            "POST /api/v1/auth/login HTTP/1.1\r\nHost: vps.example.com\r\nX-Forwarded-Proto: https\r\nContent-Length: {}\r\n\r\n{}",
            login_body.len(),
            login_body
        );
        let (https_login_resp, _) = in_memory_exchange(&hub, &https_login_req);
        assert!(
            https_login_resp.contains("Set-Cookie: __Host-onyx_session=")
                && https_login_resp.contains("Secure"),
            "{https_login_resp}"
        );

        // 6. Bearer + allowlisted CORS origin works for external Svelte SPA.
        let (bearer_cors, _) = in_memory_exchange(
            &hub,
            "GET /api/v1/state HTTP/1.1\r\nHost: 127.0.0.1\r\nOrigin: http://localhost:5173\r\nAuthorization: Bearer test-api-key-1234567890\r\n\r\n",
        );
        assert!(bearer_cors.starts_with("HTTP/1.1 200 OK"), "{bearer_cors}");
        assert!(
            bearer_cors.contains("Access-Control-Allow-Origin: http://localhost:5173"),
            "{bearer_cors}"
        );

        // 7. Logout emits Clear-Site-Data.
        let (logout, _) = in_memory_exchange(
            &hub,
            "POST /api/v1/auth/logout HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 0\r\n\r\n",
        );
        assert!(logout.starts_with("HTTP/1.1 200 OK"), "{logout}");
        assert!(
            logout.contains("Clear-Site-Data: \"cookies\", \"storage\", \"cache\""),
            "{logout}"
        );
    }

    #[test]
    fn serve_end_to_end_command_approval_and_sse_stream_without_tick_leaks() {
        let dir = std::env::temp_dir().join(format!("openonyx_web_e2e_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("hello.txt"), "hello world\n").unwrap();

        let policy = WorkspacePolicy::new(&dir).unwrap();
        let tools = WorkspaceTools::new(policy.clone());
        let cfg = WebServerConfig::resolve(
            "127.0.0.1:0",
            Some("e2e-web-api-key-987654321".to_string()),
            None,
        )
        .unwrap();

        let mut host = AgentHost::builder(1)
            .config(AgentConfig {
                default_max_steps: 16,
                default_timeout_ticks: 64,
                ..AgentConfig::default()
            })
            .system(Box::new(WorkspaceLedgerSystem::new()))
            .system(Box::new(crate::command::CommandSpawnerSystem::new(false)))
            .build()
            .unwrap();
        let ws_vocab = onyx_workspace::WorkspaceVocab::resolve_with(|n| host.attr(n));
        let web_driver = WebDriver::start(host.vocab(), &cfg, Some(dir.clone()), Some(policy))
            .unwrap()
            .with_tools(tools.clone())
            .with_workspace_vocab(ws_vocab)
            .with_model_info("test-model", None);
        let addr = web_driver.local_addr();
        let hub = Arc::clone(&web_driver.hub);
        let web_handle = web_driver.handle();
        let ws_bridge = WorkspaceToolBridge::new(host.vocab(), tools)
            .with_mode(WorkspaceMode::Build)
            .with_workspace_vocab(ws_vocab)
            .require_read_before_edit(true)
            .wrapping(web_driver);

        let mut step_num = 0usize;
        let mut driver = RecordingBridge::new(host.vocab(), move |view: &AgentView| {
            step_num += 1;
            match step_num {
                1 => Some(Reply::call_structured(
                    view.agent,
                    view.request,
                    "fs_read",
                    r#"{"path":"hello.txt"}"#,
                )),
                2 => Some(Reply::call_structured(
                    view.agent,
                    view.request,
                    "fs_edit",
                    r#"{"path":"hello.txt","old_text":"hello world","new_text":"hello onyx web"}"#,
                )),
                _ => Some(Reply::done(view.agent, view.request, "edited via web ui")),
            }
        })
        .wrapping(ws_bridge);

        // Verify SSE endpoint upgrades when authenticated and rejects unauthenticated requests.
        let (unauth_sse, is_sse_unauth) = in_memory_exchange(
            &hub,
            "GET /api/v1/events HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n",
        );
        assert!(!is_sse_unauth && unauth_sse.starts_with("HTTP/1.1 401"));
        let (_, is_sse_authed) = in_memory_exchange(
            &hub,
            "GET /api/v1/events HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer e2e-web-api-key-987654321\r\n\r\n",
        );
        assert!(
            is_sse_authed,
            "authenticated GET /api/v1/events must upgrade to SSE"
        );

        // When loopback sockets are allowed by the host OS/sandbox, also verify delayed TCP GET
        // (regression guard for Windows Winsock2 non-blocking accept inheritance) and live SSE.
        web_handle.render(host.graph());
        if let Ok(mut delayed_stream) = TcpStream::connect(addr) {
            delayed_stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            thread::sleep(Duration::from_millis(60));
            delayed_stream
                .write_all(b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
                .unwrap();
            let mut html_buf = [0u8; 512];
            let n = delayed_stream.read(&mut html_buf).unwrap();
            let html_head = String::from_utf8_lossy(&html_buf[..n]);
            assert!(html_head.starts_with("HTTP/1.1 200 OK"), "{html_head}");
        }
        if let Ok(mut sse_stream) = TcpStream::connect(addr) {
            sse_stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            sse_stream
                .write_all(
                    b"GET /api/v1/events HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer e2e-web-api-key-987654321\r\n\r\n",
                )
                .unwrap();
            let mut sse_buf = [0u8; 2048];
            let n = sse_stream.read(&mut sse_buf).unwrap();
            let sse_head = String::from_utf8_lossy(&sse_buf[..n]);
            assert!(sse_head.contains("text/event-stream"), "{sse_head}");
        }

        // Send a goal via POST /api/v1/command.
        let cmd_body = r#"{"command":"edit hello.txt"}"#;
        let (cmd_resp, _) = in_memory_exchange(
            &hub,
            &format!(
                "POST /api/v1/command HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer e2e-web-api-key-987654321\r\nContent-Length: {}\r\n\r\n{}",
                cmd_body.len(),
                cmd_body
            ),
        );
        assert!(cmd_resp.starts_with("HTTP/1.1 200 OK"), "{cmd_resp}");

        // Step until approval is pending in Web UI (no idle tick leaks).
        let mut approved = false;
        for _ in 0..16 {
            host.step(&mut driver);
            let (state_resp, _) = in_memory_exchange(
                &hub,
                "GET /api/v1/state HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer e2e-web-api-key-987654321\r\n\r\n",
            );
            if state_resp.contains("\"pending_approval\":{") {
                assert!(state_resp.contains("hello onyx web"), "{state_resp}");
                assert!(state_resp.contains("\"subagents\":"), "{state_resp}");
                let body_start = state_resp.find("\r\n\r\n").unwrap() + 4;
                let parsed: serde_json::Value =
                    serde_json::from_str(&state_resp[body_start..]).unwrap();
                let aid = parsed["pending_approval"]["agent_id"].as_u64().unwrap();
                let req_id = parsed["pending_approval"]["request"].as_u64().unwrap();
                let app_body = serde_json::json!({
                    "agent_id": aid,
                    "request": req_id,
                    "approve": true
                })
                .to_string();
                let (app_resp, _) = in_memory_exchange(
                    &hub,
                    &format!(
                        "POST /api/v1/approval HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer e2e-web-api-key-987654321\r\nContent-Length: {}\r\n\r\n{}",
                        app_body.len(),
                        app_body
                    ),
                );
                assert!(app_resp.starts_with("HTTP/1.1 200 OK"), "{app_resp}");
                approved = true;
                break;
            }
        }
        assert!(
            approved,
            "expected pending approval to be surfaced and approved via Web API"
        );

        // Verify @file search endpoint returns hello.txt.
        let (files_resp, _) = in_memory_exchange(
            &hub,
            "GET /api/v1/files?q=hello HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer e2e-web-api-key-987654321\r\n\r\n",
        );
        assert!(files_resp.contains("hello.txt"), "{files_resp}");

        assert!(host.run_until_settled(&mut driver, 16));
        assert_eq!(
            std::fs::read_to_string(dir.join("hello.txt")).unwrap(),
            "hello onyx web\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
