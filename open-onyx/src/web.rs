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
use std::io::{self, Read, Write};
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
    WorkspacePolicy, WorkspaceTools, WorkspaceVocab, detect_sandbox_capabilities,
    graph_subagent_records, graph_workspace_plan, graph_workspace_services, preview_tool_diff,
};
use serde::{Deserialize, Serialize};

use crate::context::{ReasoningEffort, SessionTelemetryHandle};
use crate::gemini::StreamPreviewHandle;
use crate::llm_profile::{LlmProfileHandle, LlmProvider};
use crate::pack::SharedPackStore;
use crate::session::UiIdlePump;
use crate::subagent::{
    MergePreviewState, SUBAGENT_PROPOSAL_CMD_PREFIX, SubagentHandle, effective_merge_decision,
};
use crate::time_debug::TimeDebugState;
use crate::tui_input::WorkspaceFileIndex;
use crate::vault::session_display_label;

pub const DEFAULT_WEB_BIND: &str = "127.0.0.1:8787";
pub const MIN_EXPLICIT_API_KEY_LEN: usize = 16;
const MAX_HTTP_HEADER_BYTES: usize = 16 * 1024;
const MAX_HTTP_BODY_BYTES: usize = 64 * 1024;
const MAX_SSE_CLIENTS: usize = 32;
const STREAM_SSE_THROTTLE_MS: u64 = 120;
const MAX_STREAM_THOUGHT_BYTES: usize = 4 * 1024;
/// FIX-09: tope de hilos de conexión simultáneas (una conexión aparcada sin datos
/// pendientes NO cuenta: no tiene hilo). Dimensionado para el peor caso legítimo
/// conocido: 150 slowloris con datos pendientes + cliente legítimo + SSE
/// long-lived (`MAX_SSE_CLIENTS + margen`). El exceso recibe `503` sin crear hilo.
const MAX_CONCURRENT_CONNECTIONS: usize = 160;
/// FIX-10: tope de sesiones web vivas (evita DoS de memoria con logins válidos).
const MAX_LIVE_SESSIONS: usize = 16;
/// FIX-08/FIX-09: presupuesto TOTAL de lectura de la petición (línea + cabeceras +
/// cuerpo) desde que se acepta la conexión. El timeout de 10 s por `read` no basta:
/// un byte goteado cada <10 s lo renueva para siempre (slowloris).
const TOTAL_REQUEST_READ_DEADLINE: std::time::Duration = std::time::Duration::from_secs(10);
/// FIX-14: ventana deslizante del rate limit de login (por IP y global).
const LOGIN_FAIL_WINDOW: std::time::Duration = std::time::Duration::from_secs(60);
const LOGIN_MAX_FAILURES_PER_IP: usize = 5;
const LOGIN_MAX_FAILURES_GLOBAL: usize = 20;
/// FIX-36: HSTS en modo expuesto (bind no loopback o `--behind-tls`).
const HSTS_VALUE: &str = "max-age=31536000; includeSubDomains";

const ASSET_INDEX_HTML: &str = include_str!("../web/index.html");
const ASSET_STYLES_CSS: &str = include_str!("../web/styles.css");
const ASSET_APP_JS: &str = include_str!("../web/app.js");
const ASSET_MANIFEST: &str = include_str!("../web/manifest.webmanifest");
const ASSET_ICON_SVG: &str = include_str!("../web/icon.svg");
const ASSET_LOGO_JPG: &[u8] = include_bytes!("../web/logo.jpg");

const CSP_HEADER: &str = "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; connect-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'; require-trusted-types-for 'script'";

// ── Ctrl+C limpio en `serve` (4C-audit) ───────────────────────────────────────

/// 1er pulso de Ctrl+C/SIGTERM: el handler de señal la levanta (solo atomics,
/// async-signal-safe) y un watcher ([`WebDriver::spawn_shutdown_watcher`]) la
/// propaga a `WebHub::quit` + `notify_all` sobre los condvars para que
/// `run_session` (`StopPolicy::Quit`) termine el bucle y guarde snapshot y
/// transcript en menos de ~35 ms, sin deadlock.
static SERVE_SHUTDOWN: AtomicBool = AtomicBool::new(false);

/// Simula el 1er pulso de Ctrl+C/SIGTERM para tests: misma semántica que el
/// handler de señal (store `Release` sobre `SERVE_SHUTDOWN`).
#[cfg(test)]
pub(crate) fn mark_serve_shutdown_request() {
    SERVE_SHUTDOWN.store(true, Ordering::Release);
}

/// Solo para tests: restablece la bandera tras una prueba.
#[cfg(test)]
pub(crate) fn reset_serve_shutdown_request() {
    SERVE_SHUTDOWN.store(false, Ordering::Relaxed);
}

/// RAII: instala el handler de Ctrl+C/SIGTERM para `openonyx serve`.
///
/// - 1er pulso: levanta `SERVE_SHUTDOWN`; el proceso sigue vivo (apago limpio).
/// - 2º pulso: comportamiento por defecto (el SO termina el proceso).
/// - `Drop` restaura el handler previo; nunca hace `std::process::exit`.
#[cfg(windows)]
pub struct CtrlCGuard;

#[cfg(unix)]
pub struct CtrlCGuard {
    prev_sigint: usize,
    prev_sigterm: usize,
}

#[cfg(windows)]
mod ctrl_c {
    use std::sync::atomic::Ordering;

    type CtrlHandler = unsafe extern "system" fn(u32) -> i32;

    unsafe extern "system" {
        fn SetConsoleCtrlHandler(handler: Option<CtrlHandler>, add_handler: i32) -> i32;
    }

    const CTRL_C_EVENT: u32 = 0;
    const CTRL_BREAK_EVENT: u32 = 1;
    const CTRL_CLOSE_EVENT: u32 = 2;

    /// Solo `CTRL_C_EVENT`/`CTRL_BREAK_EVENT`/`CTRL_CLOSE_EVENT`: 1er pulso →
    /// flag + `1` (manejado); 2º → `0` (default: terminar). Cualquier otro
    /// evento (p. ej. `CTRL_LOGOFF_EVENT`) se delega al default (`0`).
    unsafe extern "system" fn handler(ctrl_type: u32) -> i32 {
        if !matches!(
            ctrl_type,
            CTRL_C_EVENT | CTRL_BREAK_EVENT | CTRL_CLOSE_EVENT
        ) {
            return 0;
        }
        if super::SERVE_SHUTDOWN.swap(true, Ordering::AcqRel) {
            0
        } else {
            1
        }
    }

    pub(super) fn attach() {
        unsafe {
            SetConsoleCtrlHandler(Some(handler), 1);
        }
    }

    pub(super) fn detach() {
        unsafe {
            SetConsoleCtrlHandler(Some(handler), 0);
        }
    }
}

#[cfg(unix)]
mod ctrl_c {
    use std::sync::atomic::Ordering;

    const SIGINT: i32 = 2;
    const SIGTERM: i32 = 15;
    const SIG_DFL: usize = 0;

    // FFI directo a libc (sin añadir la crate `libc`: el `Cargo.lock` es
    // invariante de SDD-04).
    unsafe extern "C" {
        fn signal(signum: i32, handler: usize) -> usize;
    }

    /// Handler async-signal-safe: solo un store atómico (`Release`,
    /// emparejado con los `Acquire` del watcher) + restaurar `SIG_DFL` para
    /// que el 2º pulso termine el proceso por defecto.
    extern "C" fn handler(sig: i32) {
        super::SERVE_SHUTDOWN.store(true, Ordering::Release);
        unsafe {
            signal(sig, SIG_DFL);
        }
    }

    pub(super) fn attach() -> (usize, usize) {
        unsafe {
            (
                signal(SIGINT, handler as usize),
                signal(SIGTERM, handler as usize),
            )
        }
    }

    pub(super) fn detach(prev_sigint: usize, prev_sigterm: usize) {
        unsafe {
            signal(SIGINT, prev_sigint);
            signal(SIGTERM, prev_sigterm);
        }
    }
}

impl CtrlCGuard {
    pub fn new() -> Self {
        // `SDD-04` 4C-audit: parte de estado limpio por si una `serve` previa
        // en el mismo proceso (p. ej. tests) dejó la bandera levantada.
        SERVE_SHUTDOWN.store(false, Ordering::Release);
        #[cfg(windows)]
        {
            ctrl_c::attach();
            Self
        }
        #[cfg(unix)]
        {
            let (prev_sigint, prev_sigterm) = ctrl_c::attach();
            Self {
                prev_sigint,
                prev_sigterm,
            }
        }
    }
}

impl Drop for CtrlCGuard {
    fn drop(&mut self) {
        #[cfg(windows)]
        ctrl_c::detach();
        #[cfg(unix)]
        ctrl_c::detach(self.prev_sigint, self.prev_sigterm);
    }
}

/// Configuración validada para arrancar `openonyx serve`.
#[derive(Clone, Debug)]
pub struct WebServerConfig {
    pub bind_addr: SocketAddr,
    pub api_key: String,
    pub generated_key: bool,
    pub cors_origin: Option<String>,
    /// FIX-16/FIX-36: modo explícito «detrás de proxy TLS» (`--behind-tls`).
    /// Es la única vía (junto con el propio bind no loopback) para el modo
    /// seguro: cookie `__Host-…; Secure` + HSTS. La cabecera `X-Forwarded-Proto`
    /// del cliente **nunca** se usa para decidirlo (FIX-16).
    pub behind_tls: bool,
    /// FIX-17: lista de `Host` permitidos (`--allowed-hosts a,b`). `None` =
    /// sin validación (comportamiento por defecto; el Host del cliente solo se
    /// compara, nunca se usa para generar URLs).
    pub allowed_hosts: Option<Vec<String>>,
}

impl WebServerConfig {
    /// FIX-36: el modo seguro es explícito — o el bind no es loopback (VPS),
    /// o el arranque declara `--behind-tls`.
    pub fn secure_mode(&self) -> bool {
        !self.bind_addr.ip().is_loopback() || self.behind_tls
    }
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
            // FIX-37: fuera de loopback un origen `http://` no protege la cookie
            // `Secure`: se rechaza al arrancar (solo `https://` o sin CORS).
            if !is_loopback && origin.starts_with("http://") {
                return Err(format!(
                    "refusing to bind non-loopback address `{bind_addr}` with `--cors-origin {origin}`: only `https://` origins are allowed in exposed mode (or omit `--cors-origin`)"
                ));
            }
        }

        Ok(Self {
            bind_addr,
            api_key,
            generated_key,
            cors_origin: clean_cors,
            behind_tls: false,
            allowed_hosts: None,
        })
    }

    /// FIX-16/FIX-36: declare que hay un proxy que termina TLS delante (la cookie
    /// sale con `__Host-…; Secure` y la respuesta con HSTS, sin depender de la
    /// cabecera `X-Forwarded-Proto` del cliente).
    pub fn behind_tls(mut self) -> Self {
        self.behind_tls = true;
        self
    }

    /// FIX-17: lista de hosts `Host` permitidos (comparación exacta, minúsculas).
    pub fn with_allowed_hosts(mut self, hosts: Vec<String>) -> Self {
        let hosts = hosts
            .into_iter()
            .map(|h| h.trim().trim_end_matches('/').to_ascii_lowercase())
            .filter(|h| !h.is_empty())
            .collect();
        self.allowed_hosts = Some(hosts);
        self
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

/// Metadatos públicos de un perfil LLM registrado (**SDD-03** Fase 4).
///
/// **Invariante de seguridad:** solo metadatos — `api_key`/`api_key_env`
/// nunca salen del handle; este struct no puede contenerlos por construcción.
#[derive(Clone, Debug, Serialize)]
pub struct WebLlmProfileSnapshot {
    pub id: String,
    pub provider: String,
    pub model: String,
    pub effort: String,
    pub available: bool,
    pub unavailable_reason: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct WebSubagentSnapshot {
    pub job_id: String,
    pub name: String,
    /// Backend que ejecuta el job: `"native"` o el `id` del harness CLI externo (`SDD-10`).
    pub driver: String,
    pub status: String,
    pub ok: Option<bool>,
    pub spawn_tick: u64,
    pub branch: String,
    pub goal: String,
    pub touched_files: Vec<String>,
    pub summary: String,
}

/// `SDD-07` Fase 4A: proyección web del depurador temporal (`TimeFreeze`).
/// `pending_revert_files` sale de `live_effects` (grafo vivo, 0 I/O): es
/// exacto aunque `agents` proyecte el `preview_graph(cursor)`.
#[derive(Clone, Debug, Serialize)]
pub struct WebTimeDebugSnapshot {
    pub frozen: bool,
    pub cursor_tick: u64,
    pub present_tick: u64,
    pub available_ticks: Vec<u64>,
    pub pending_revert_files: Vec<String>,
    /// Sinergia `SDD-05` §9.4: comando copiable `openonyx bench --snapshot-dir <dir> --tick <cursor> --models <provider>:<model>` (solo si hay store de snapshots y objetivo LLM vivo con provider+modelo).
    pub bench_hint: Option<String>,
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
    /// Drivers disponibles para `/spawn` (`"native"` + ids de `[[external_drivers]]`, `SDD-10`).
    pub drivers: Vec<String>,
    pub effort: String,
    /// `SDD-03` Fase 4: objetivo LLM vivo (selector en Web) — salen del
    /// `LlmProfileHandle` (hot-swap en frontera de tick); sin handle, el
    /// arranque estático y listas vacías.
    pub profile_id: String,
    pub provider: String,
    pub profiles: Vec<WebLlmProfileSnapshot>,
    pub available_providers: Vec<String>,
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
    /// `SDD-07` Fase 4A: paridad TimeFreeze (TUI F1): `None` si la sesión no
    /// tiene depurador temporal (p. ej. `run`/`replay` headless).
    pub time_debug: Option<WebTimeDebugSnapshot>,
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

/// FIX-14: ventana deslizante de fallos de login, por dirección de origen y global.
/// El global existe porque detrás de un proxy el por-IP se puede distribuir
/// (y el coste de `blake3` de cada intento no se quiere multiplicar).
struct LoginRateLimiter {
    per_ip: BTreeMap<std::net::IpAddr, VecDeque<Instant>>,
    global: VecDeque<Instant>,
}

impl LoginRateLimiter {
    fn new() -> Self {
        Self {
            per_ip: BTreeMap::new(),
            global: VecDeque::new(),
        }
    }

    fn prune(&mut self, now: Instant) {
        while let Some(front) = self.global.front() {
            if now.duration_since(*front) > LOGIN_FAIL_WINDOW {
                self.global.pop_front();
            } else {
                break;
            }
        }
        for v in self.per_ip.values_mut() {
            while let Some(front) = v.front() {
                if now.duration_since(*front) > LOGIN_FAIL_WINDOW {
                    v.pop_front();
                } else {
                    break;
                }
            }
        }
        self.per_ip.retain(|_, v| !v.is_empty());
    }

    /// Devuelve los segundos hasta que la ventana vuelva a permitir (para
    /// `Retry-After`); `None` si no hay límite activo.
    fn limited_for(&mut self, ip: &std::net::IpAddr, now: Instant) -> Option<u64> {
        self.prune(now);
        let per_ip_limited = self
            .per_ip
            .get(ip)
            .is_some_and(|v| v.len() >= LOGIN_MAX_FAILURES_PER_IP);
        let global_limited = self.global.len() >= LOGIN_MAX_FAILURES_GLOBAL;
        if !per_ip_limited && !global_limited {
            return None;
        }
        let oldest = self
            .global
            .front()
            .copied()
            .or_else(|| self.per_ip.get(ip).and_then(|v| v.front().copied()));
        let wait = oldest
            .map(|o| {
                (LOGIN_FAIL_WINDOW - now.duration_since(o))
                    .as_secs()
                    .saturating_add(1)
            })
            .unwrap_or(60);
        Some(wait.clamp(1, 120))
    }

    fn record_failure(&mut self, ip: std::net::IpAddr, now: Instant) {
        self.prune(now);
        self.per_ip.entry(ip).or_default().push_back(now);
        self.global.push_back(now);
    }
}

struct WebHub {
    api_key: String,
    /// FIX-10: almacén de sesiones vivas (token aleatorio por login, tope
    /// `MAX_LIVE_SESSIONS`). Reemplaza el token único derivado por proceso:
    /// cada login emite uno nuevo, el logout lo revoca y un token filtrado
    /// deja de valer sin reiniciar `serve`.
    sessions: Mutex<Vec<String>>,
    /// FIX-14: rate limit de `POST /api/v1/auth/login`.
    login_rate: Mutex<LoginRateLimiter>,
    /// FIX-09: conexiones TCP activas (RAII en `ConnectionSlot`).
    conn_active: AtomicUsize,
    /// FIX-16/36: cookie `__Host-…; Secure` + HSTS (explícito, nunca por cabecera).
    secure_mode: bool,
    /// FIX-17: `Host` permitidos (`None` = sin validación).
    allowed_hosts: Option<Vec<String>>,
    cors_origin: Option<String>,
    workspace_root: Option<PathBuf>,
    policy: Option<WorkspacePolicy>,
    file_index: Mutex<WorkspaceFileIndex>,
    state: Mutex<HubState>,
    state_cv: Condvar,
    input_cv: Condvar,
    quit: Arc<AtomicBool>,
    sse_clients: AtomicUsize,
    /// `SDD-07` Fase 4A: depurador temporal (mismo estado del bucle de
    /// sesión): `POST /api/v1/time/scrub` y el 409 de `POST /api/v1/command`
    /// bajo freeze lo leen directamente del hub. `Mutex` porque el
    /// `WebDriver` se construye **antes** del bucle de sesión: el
    /// `TimeDebugState` se inyecta en caliente vía `with_time_debug`
    /// (wiring diferido, 0 reconstrucción del hub).
    time_debug: Mutex<Option<TimeDebugState>>,
    /// `SDD-07` Fase 4A: scrub pendiente de republicar. El endpoint lo
    /// levanta (+`input_cv`); `pump_idle` lo consume **solo** cuando el
    /// `graph` recibido ya refleja el nuevo cursor (véase la lógica de
    /// defer dentro de `pump_idle`).
    time_scrub_dirty: AtomicBool,
}

/// Rechazo de `POST /api/v1/command`. `BadRequest` → `400`; `TimeFrozen` →
/// `409 Conflict` (paridad TimeFreeze, `SDD-07` Fase 4A).
enum WebCommandRejection {
    BadRequest(String),
    TimeFrozen(String),
}

impl WebHub {
    fn new(
        config: &WebServerConfig,
        workspace_root: Option<PathBuf>,
        policy: Option<WorkspacePolicy>,
    ) -> Result<Arc<Self>, String> {
        Ok(Arc::new(Self {
            api_key: config.api_key.clone(),
            sessions: Mutex::new(Vec::new()),
            login_rate: Mutex::new(LoginRateLimiter::new()),
            conn_active: AtomicUsize::new(0),
            secure_mode: config.secure_mode(),
            allowed_hosts: config.allowed_hosts.clone(),
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
            time_debug: Mutex::new(None),
            time_scrub_dirty: AtomicBool::new(false),
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

    /// FIX-10: emite un token de sesión aleatorio de 256 bits. Si el almacén está
    /// a tope (`MAX_LIVE_SESSIONS`), rechaza en vez de expulsar (no se rompe una
    /// sesión legítima por llegar otra).
    fn create_session(&self) -> Result<String, String> {
        let mut bytes = [0u8; 32];
        fill_os_random(&mut bytes).map_err(|e| format!("failed to generate session token: {e}"))?;
        let mut token = String::with_capacity(64);
        for b in bytes {
            use std::fmt::Write as _;
            let _ = write!(token, "{b:02x}");
        }
        let mut sessions = self.sessions.lock().unwrap();
        if sessions.len() >= MAX_LIVE_SESSIONS {
            return Err(format!(
                "too many active web sessions (max {MAX_LIVE_SESSIONS}); log out a browser and retry"
            ));
        }
        sessions.push(token.clone());
        Ok(token)
    }

    /// FIX-10: revoca una sesión (logout). `false` si no estaba viva.
    fn revoke_session(&self, token: &str) -> bool {
        let mut sessions = self.sessions.lock().unwrap();
        let before = sessions.len();
        sessions.retain(|t| !constant_time_eq(t.as_bytes(), token.as_bytes()));
        sessions.len() != before
    }

    /// FIX-10: comparación en tiempo constante contra el almacén (se recorren
    /// TODAS las sesiones aunque una casa, para no filtrar posición por timing).
    fn verify_session_cookie(&self, candidate: &str) -> bool {
        let sessions = self.sessions.lock().unwrap();
        sessions
            .iter()
            .any(|t| constant_time_eq(t.as_bytes(), candidate.as_bytes()))
    }

    /// `SDD-07` Fase 4A: paridad TimeFreeze en Web (igual que el `F1 Chat`
    /// del TUI). Bajo congelación (o con el cursor anclado en el pasado) un
    /// prompt **plano** no se encola al bus — un turno desordenado rompería la
    /// bit-a-bit del replay. Los comandos `/…` y `!…` sí se encolan (vías de
    /// escape: `/go`, `/inject`, `/graph`, `/step`, `/step-llm`).
    ///
    /// Devuelve `Err(TimeFrozenRejection)` si el prompt plano está bloqueado
    /// (el handler lo mapea a `409 Conflict`); el resto de rechazos (prefix
    /// reservado, vacío) siguen siendo `400`.
    fn push_web_command(&self, raw_cmd: &str) -> Result<(), WebCommandRejection> {
        let cmd = raw_cmd.trim();
        if cmd.is_empty() {
            return Err(WebCommandRejection::BadRequest(
                "command cannot be empty".to_string(),
            ));
        }
        if cmd.starts_with(SUBAGENT_PROPOSAL_CMD_PREFIX.trim_end()) || cmd.starts_with("/__onyx_") {
            return Err(WebCommandRejection::BadRequest(
                "reserved internal command prefix".to_string(),
            ));
        }
        // `SDD-07` Fase 4A: el handle se clona bajo lock breve (Arcs baratos);
        // el resto de la lectura es thread-safe en `TimeDebugState` (atomics).
        let td = self
            .time_debug
            .lock()
            .map(|guard| guard.as_ref().cloned())
            .unwrap_or(None);
        if let Some(td) = td {
            if (td.is_frozen() || td.cursor() < td.present())
                && !cmd.starts_with('/')
                && !cmd.starts_with('!')
            {
                return Err(WebCommandRejection::TimeFrozen(
                    "Paused in TIME FROZEN: use /go to resume or /inject to stage memory (or return to present)"
                        .to_string(),
                ));
            }
        }
        let mut guard = self
            .state
            .lock()
            .map_err(|_| WebCommandRejection::BadRequest("web hub lock poisoned".to_string()))?;
        if guard.root_busy && !crate::command::is_immediate_session_command(cmd) {
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

    /// `SDD-07` Fase 4A: aplica un scrub efímero al depurador temporal y
    /// levanta `time_scrub_dirty` + `input_cv` para el push SSE inmediato.
    ///
    /// Payload flexible (JSON):
    /// - `{ "action": "freeze" | "unfreeze" | "return_to_present" |
    ///   "scrub_to" | "scrub_delta", "tick"?: u64, "delta"?: i64 }`
    /// - Shorthands: `{ "freeze": true }`, `{ "tick": 5 }`, `{ "delta": -1 }`.
    ///
    /// **100% efímero**: solo muta `TimeDebugState`; nunca escribe en
    /// `ONYXTR2` ni avanza ticks. Devuelve el snapshot `time_debug` actualizado
    /// (para el body `{"ok":true,"time_debug":...}`) o `Err` accionable
    /// (p. ej. tick purgado/futuro → `400`).
    fn apply_time_scrub(&self, body: &[u8]) -> Result<WebTimeDebugSnapshot, String> {
        let td = self
            .time_debug
            .lock()
            .map_err(|_| "web hub time_debug lock poisoned".to_string())?
            .as_ref()
            .cloned()
            .ok_or_else(|| "time debug is not available in this session".to_string())?;
        let v: serde_json::Value = serde_json::from_slice(body)
            .map_err(|_| "expected a JSON object for time/scrub".to_string())?;
        if !v.is_object() {
            return Err("expected a JSON object for time/scrub".to_string());
        }
        let action = v
            .get("action")
            .and_then(|a| a.as_str())
            .map(|a| a.to_ascii_lowercase());
        let tick = v.get("tick").and_then(|t| t.as_u64());
        let delta = v.get("delta").and_then(|d| d.as_i64());

        match action.as_deref() {
            // `action` explícito (forma canónica).
            Some(a) => match a {
                "freeze" => td.freeze(),
                "unfreeze" => td.unfreeze(),
                "return_to_present" => td.return_to_present(),
                "scrub_to" => {
                    let t = tick.ok_or("scrub_to requires `tick`")?;
                    td.scrub_to(t).map_err(|e| e.0.clone())?;
                }
                "scrub_delta" => {
                    let d = delta.ok_or("scrub_delta requires `delta`")?;
                    td.scrub_delta(d).map_err(|e| e.0.clone())?;
                }
                other => {
                    return Err(format!(
                        "unknown time/scrub action `{other}` (expected freeze | unfreeze | \
                         return_to_present | scrub_to | scrub_delta)"
                    ));
                }
            },
            // Shorthands: `{ "freeze": true }` / `{ "tick": N }` / `{ "delta": N }`.
            None => {
                if v.get("freeze").and_then(|f| f.as_bool()) == Some(true) {
                    td.freeze();
                } else if v.get("unfreeze").and_then(|f| f.as_bool()) == Some(true) {
                    td.unfreeze();
                } else if v.get("return_to_present").and_then(|f| f.as_bool()) == Some(true) {
                    td.return_to_present();
                } else if let Some(t) = tick {
                    td.scrub_to(t).map_err(|e| e.0.clone())?;
                } else if let Some(d) = delta {
                    td.scrub_delta(d).map_err(|e| e.0.clone())?;
                } else {
                    return Err(
                        "time/scrub needs an `action` (or shorthand `freeze`/`tick`/`delta`)"
                            .to_string(),
                    );
                }
            }
        };

        // Push SSE inmediato: despierta `pump_idle`, que republica el
        // `WebStateSnapshot` con el nuevo `preview_graph(cursor)` (0 ticks,
        // 0 `ONYXTR2`).
        self.time_scrub_dirty.store(true, Ordering::Release);
        self.input_cv.notify_all();

        Ok(web_time_debug_snapshot(&td, None))
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
    /// `SDD-03` Fase 3: fuente live del objetivo LLM (hot-swap en frontera de
    /// tick). Si existe, `model`/`effort` del snapshot salen de aquí; si no,
    /// del arranque estático (`model_name`/`reasoning_effort`).
    llm_profile: Option<LlmProfileHandle>,
    model_name: String,
    reasoning_effort: Option<ReasoningEffort>,
    session_label: Option<String>,
    snapshot_store: Option<SharedPackStore>,
    /// `SDD-07` Fase 4A: depurador temporal (para `publish_snapshot`:
    /// `WebStateSnapshot.time_debug`, proyección del `preview_graph(cursor)`
    /// + `pending_revert_files` desde `live_effects`).
    time_debug: Option<TimeDebugState>,
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
        // `SDD-07` Fase 4A: el `graph` recibido fue calculado por `run_session`
        // justo antes de llamar (puede ser el `preview_graph(cursor)`). Captura
        // el `(frozen, cursor, present)` de `TimeDebugState` en ese momento: si
        // cambia DURANTE el wait, el `graph` queda obsoleto y la republicación
        // debe esperar a la siguiente iteración del bucle (que recalcula el
        // `preview_graph`), no a esta llamada.
        // `TimeDebugState` es clonable (Arcs internos): se clona bajo lock breve
        // y el resto de la lógica opera sobre el handle thread-safe.
        let td = self
            .hub
            .time_debug
            .lock()
            .map(|guard| guard.as_ref().cloned())
            .unwrap_or(None);
        let key_at_call = td
            .as_ref()
            .map(|t| (t.is_frozen(), t.cursor(), t.present()));
        let scrub_pending = self.hub.time_scrub_dirty.load(Ordering::Acquire);

        // Espera breve sobre Condvar (35ms): despierta al instante si llega un comando web,
        // sin consumir CPU y sin avanzar `host.ticks()`. Con un scrub pendiente
        // no se espera: se regresa al bucle para que `run_session` recalculé el
        // `preview_graph` y la siguiente llamada republicá de inmediato.
        if let Ok(guard) = self.hub.state.lock() {
            // `SDD-04` 4C-audit: `Acquire` emparejado con el `Release` del
            // watcher de apago (`spawn_shutdown_watcher`).
            let needs_wait = guard.incoming_events.is_empty()
                && !self.hub.quit.load(Ordering::Acquire)
                && !scrub_pending;
            if needs_wait {
                let _ = self
                    .hub
                    .input_cv
                    .wait_timeout(guard, Duration::from_millis(35));
            }
        }
        // ¿El depurador cambió durante el wait? (scrub/`/go`/`/rewind` en caliente)
        let key_after = td.map(|t| (t.is_frozen(), t.cursor(), t.present()));
        let graph_is_fresh = match (key_at_call, key_after) {
            (Some(a), Some(b)) => a == b,
            _ => true,
        };
        if let Ok(mut inner) = self.inner.lock() {
            let stream_dirty = inner.stream_preview.take_dirty();
            let telemetry_dirty = inner.telemetry.take_dirty();
            // `SDD-03` Fase 3: un cambio LLM en caliente marca dirty → la
            // cabecera (model/effort) se actualiza sin tocar el bucle determinista.
            let llm_dirty = inner
                .llm_profile
                .as_ref()
                .is_some_and(LlmProfileHandle::take_dirty);
            let dispatched = inner.dispatch_queued_if_idle(graph);
            let hub_dirty = self.hub.take_state_dirty();

            // `SDD-07` Fase 4A: se consume el flag de scrub SOLO si el `graph`
            // es fresco; si no, se deja puesto para que la siguiente llamada
            // (con el `preview_graph` recalculado) haga el push SSE.
            let scrub_dirty = if graph_is_fresh {
                self.hub.time_scrub_dirty.swap(false, Ordering::AcqRel)
            } else {
                false
            };

            // Solo serializamos y difundimos `event: state` completo cuando realmente cambió
            // el estado de sesión/telemetría/cola/objetivo LLM/TimeFreeze (0 trabajo de grafo en reposo o durante streaming).
            if telemetry_dirty || dispatched || hub_dirty || llm_dirty || scrub_dirty {
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

    /// `SDD-07` Fase 2: encola el comando sellado; el próximo
    /// `WebDriver::poll` drena `incoming_events` vía `bridge.push`
    /// (queda grabado en `ONYXTR2`).
    fn queue_ingress(&self, event: OnyxEvent) {
        if let Ok(mut guard) = self.hub.state.lock() {
            guard.incoming_events.push_back(event);
            self.hub.input_cv.notify_all();
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
            llm_profile: None,
            model_name: String::new(),
            reasoning_effort: None,
            session_label: None,
            snapshot_store: None,
            time_debug: None,
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

    /// `SDD-03` Fase 3: conecta el handle LLM vivo a la cabecera — `model`/
    /// `effort` del snapshot siguen el hot-swap (`/profile`/`/provider`/
    /// `/model`/`/effort` en frontera de tick); sin handle, fallback al
    /// arranque estático de [`WebDriver::with_model_info`].
    pub fn with_llm_profile(self, profile: LlmProfileHandle) -> Self {
        if let Ok(mut inner) = self.inner.lock() {
            inner.llm_profile = Some(profile);
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

    /// `SDD-07` Fase 4A: paridad TimeFreeze en Web. El mismo `TimeDebugState`
    /// del bucle de sesión (clon barato: Arcs internos) alimenta
    /// `WebStateSnapshot.time_debug` (preview + `pending_revert_files` desde
    /// `live_effects`) y el endpoint efímero `POST /api/v1/time/scrub`.
    pub fn with_time_debug(self, time_debug: TimeDebugState) -> Self {
        if let Ok(mut guard) = self.hub.time_debug.lock() {
            *guard = Some(time_debug.clone());
        }
        if let Ok(mut inner) = self.inner.lock() {
            inner.time_debug = Some(time_debug);
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

    /// Watcher ligero (polling 5 ms) que, al ver `SERVE_SHUTDOWN` (1er Ctrl+C),
    /// propaga el apago a `WebHub::quit` y despierta los condvars para que
    /// `pump_idle` y el bucle `StopPolicy::Quit` terminen sin deadlock.
    /// También termina si `WebHub::quit` ya fue puesto (p. ej. `drop` del
    /// `WebDriver`) sin que haya Ctrl+C: no queda un hilo colgado.
    pub fn spawn_shutdown_watcher(&self) -> thread::JoinHandle<()> {
        let hub = Arc::clone(&self.hub);
        thread::Builder::new()
            .name("onyx-serve-shutdown".into())
            .spawn(move || {
                while !SERVE_SHUTDOWN.load(Ordering::Acquire) && !hub.quit.load(Ordering::Acquire) {
                    thread::sleep(Duration::from_millis(5));
                }
                if SERVE_SHUTDOWN.load(Ordering::Acquire) {
                    hub.quit.store(true, Ordering::Release);
                    hub.state_cv.notify_all();
                    hub.input_cv.notify_all();
                }
            })
            .expect("failed to spawn serve shutdown watcher")
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

    /// Drivers disponibles para `/spawn`: `"native"` + los ids de `[[external_drivers]]`
    /// resueltos en la policy cargada (`SDD-10` Fase 4). En un workspace **no confiado**
    /// no se expone ningún driver externo (defense in depth, alineado con `main.rs` y
    /// `subagent.rs`): `spawn_job` lo bloquearía igualmente en runtime.
    fn available_driver_ids(&self) -> Vec<String> {
        let mut ids = vec!["native".to_string()];
        if let Some(pol) = &self.policy {
            if pol.trust.config_trusted {
                ids.extend(pol.external_drivers().iter().map(|d| d.id.clone()));
            }
        }
        ids
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
                    // `SDD-10` Fase 4: misma decisión efectiva que `SubagentManagerDriver`
                    // (gobernanza → `Ask` obligatorio); si no es `Ask`, el driver auto-aprueba
                    // y el banner no debe mostrarse.
                    Some(MergePreviewState::Clean(ticket)) => {
                        if effective_merge_decision(static_dec, &ticket) != PermissionDecision::Ask
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
                    // `SDD-10` Fase 3: Control Plane → sin banner; `SubagentManagerDriver`
                    // niega la solicitud de plano (disco de `root` intacto).
                    Some(MergePreviewState::ControlPlaneDenied { .. }) => continue,
                    // `SDD-10` Fase 4: alineado con `CliApprovalDriver`: sin preview no hay
                    // banner; el driver auto-aprueba para que `subagent_merge` devuelva el
                    // error exacto a `root` (evita un banner que el driver no espera).
                    None => continue,
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
                    driver: s.driver,
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
                    driver: r.driver,
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
        // `SDD-03` Fase 3/4: cabecera viva + selector en Web — el objetivo
        // LLM actual (id/perfil/provider/modelo/esfuerzo/perfiles
        // registrados) sale del handle (hot-swap en frontera de tick); sin
        // handle, el arranque estático y listas vacías (degrada al perfil CLI).
        let llm = self
            .llm_profile
            .as_ref()
            .map(|handle| handle.public_snapshot());
        let (profile_id, provider, model, effort, profiles, available_providers) =
            match llm.as_ref() {
                Some(s) => (
                    s.active_profile_id.clone(),
                    s.provider.clone(),
                    s.model.clone(),
                    s.effort.clone(),
                    s.profiles
                        .iter()
                        .map(|e| WebLlmProfileSnapshot {
                            id: e.id.clone(),
                            provider: e.provider.clone(),
                            model: e.model.clone(),
                            effort: e.effort.clone(),
                            available: e.available,
                            unavailable_reason: e.unavailable_reason.clone(),
                        })
                        .collect(),
                    s.available_providers.clone(),
                ),
                None => (
                    String::from("default"),
                    String::new(),
                    self.model_name.clone(),
                    self.reasoning_effort
                        .map(ReasoningEffort::as_str)
                        .unwrap_or("auto")
                        .to_string(),
                    Vec::new(),
                    Vec::new(),
                ),
            };
        // `SDD-07` Fase 4A: paridad TimeFreeze. `agents` ya proyecta el
        // `preview_graph(cursor)` (0 reconstrucción en JS); `pending_revert_files`
        // sale de `live_effects` (grafo vivo) y `bench_hint` es la sinergia
        // `SDD-05` §9.4 (solo con store de snapshots + objetivo LLM vivo).
        let time_debug_snap = self
            .time_debug
            .as_ref()
            .map(|td| web_time_debug_snapshot(td, time_debug_bench_hint(td, &provider, &model)));
        let snap = WebStateSnapshot {
            schema: "onyx.web.state.v1",
            version: next_version,
            tick: guard.tick,
            mode: current_mode.as_label().to_string(),
            sandbox_badge,
            untrusted,
            model,
            drivers: self.available_driver_ids(),
            effort,
            profile_id,
            provider,
            profiles,
            available_providers,
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
            time_debug: time_debug_snap,
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

/// `SDD-07` Fase 4A: proyección `TimeDebugState` → [`WebTimeDebugSnapshot`].
/// 0 I/O: solo atomics + `live_effects` (RAM).
fn web_time_debug_snapshot(
    td: &TimeDebugState,
    bench_hint: Option<String>,
) -> WebTimeDebugSnapshot {
    WebTimeDebugSnapshot {
        frozen: td.is_frozen(),
        cursor_tick: td.cursor(),
        present_tick: td.present(),
        available_ticks: td.available_ticks(),
        pending_revert_files: td.pending_revert_file_names(),
        bench_hint,
    }
}

/// `SDD-05` §9.4 (sinergia `SDD-07` Fase 4A): comando copiable para comparar
/// el modelo vivo contra un `bench` determinista desde el mismo cursor:
/// `openonyx bench --snapshot-dir <dir> --tick <cursor> --models <provider>:<model>`.
/// `None` si no hay store de snapshots (sin `--snapshot-dir`) o el objetivo
/// LLM no expone `provider`+`model` (el candidato vivo exige ambos).
fn time_debug_bench_hint(td: &TimeDebugState, provider: &str, model: &str) -> Option<String> {
    let dir = td.pack_store()?.current_dir();
    // El candidato vivo de `bench` exige `provider:model` con un provider real;
    // el parser del LLM es la fuente única de qué providers existen (el
    // fallback estático `default` no califica).
    if dir.as_os_str().is_empty() || LlmProvider::parse(provider).is_err() || model.is_empty() {
        return None;
    }
    Some(format!(
        "openonyx bench --snapshot-dir {} --tick {} --models {provider}:{model}",
        dir.display(),
        td.cursor()
    ))
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
    /// Dirección del socket cliente (FIX-14: origen del rate limit de login
    /// cuando no hay proxy de confianza delante).
    peer: Option<std::net::IpAddr>,
}

/// Estado de una conexión aparcada (FIX-09), sonda con `peek` no bloqueante.
enum ParkedProbe {
    /// Hay datos en el buffer del kernel: toca crear el hilo que la atienda.
    Data,
    /// Sin datos todavía: sigue aparcada (0 hilos, solo un fd).
    Idle,
    /// El cliente cerró o el socket está roto: se descarta.
    Closed,
}

/// Sonda no destructiva (no consume bytes) si un socket no bloqueante tiene
/// datos pendientes de leer.
fn probe_stream(stream: &TcpStream) -> ParkedProbe {
    let mut one = [0u8; 1];
    match stream.peek(&mut one) {
        Ok(n) if n > 0 => ParkedProbe::Data,
        Ok(_) => ParkedProbe::Closed,
        Err(e) if e.kind() == io::ErrorKind::WouldBlock => ParkedProbe::Idle,
        Err(_) => ParkedProbe::Closed,
    }
}

fn accept_loop(listener: TcpListener, hub: Arc<WebHub>) {
    // `SDD-04` 4C-audit: `Acquire` emparejado con el `Release` del watcher
    // de apago (`spawn_shutdown_watcher`).
    //
    // FIX-09: una conexión que llega **sin datos pendientes** se aparca sin
    // crear hilo (300 sockets ociosos ocupan 0 hilos, no solo un fd y el
    // buffer del kernel). En cuanto el `peek` detecta datos, se promociona
    // a hilo normal (con tope `MAX_CONCURRENT_CONNECTIONS`).
    let mut parked: Vec<TcpStream> = Vec::new();
    while !hub.quit.load(Ordering::Acquire) {
        loop {
            match listener.accept() {
                Ok((stream, _addr)) => match probe_stream(&stream) {
                    ParkedProbe::Data => {
                        if stream.set_nonblocking(false).is_ok() {
                            spawn_connection(stream, &hub, &mut default_connection_spawner);
                        }
                    }
                    ParkedProbe::Idle => parked.push(stream),
                    ParkedProbe::Closed => {}
                },
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(_) => {
                    thread::sleep(Duration::from_millis(50));
                    break;
                }
            }
        }
        if !parked.is_empty() {
            let waiting = std::mem::take(&mut parked);
            for stream in waiting {
                match probe_stream(&stream) {
                    ParkedProbe::Data => {
                        if stream.set_nonblocking(false).is_ok() {
                            spawn_connection(stream, &hub, &mut default_connection_spawner);
                        }
                    }
                    ParkedProbe::Idle => parked.push(stream),
                    ParkedProbe::Closed => {}
                }
            }
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// FIX-35: el spawner real. `thread::Builder::spawn` devuelve `Err` (EAGAIN por
/// RLIMIT_NPROC / TasksMax / pids.max) donde el `thread::spawn` antiguo hacía
/// pánico DENTRO del hilo de accept y lo mataba para siempre (el proceso
/// seguía vivo pero ya no aceptaba: caída permanente sin autenticar).
fn default_connection_spawner(
    stream: TcpStream,
    hub: Arc<WebHub>,
) -> io::Result<thread::JoinHandle<()>> {
    thread::Builder::new()
        .name("openonyx-conn".to_string())
        .spawn(move || {
            let _slot = ConnectionSlot(Arc::clone(&hub));
            let _ = handle_connection(stream, hub);
        })
}

/// FIX-09: slot RAII del contador de conexiones activas (lo libera al terminar
/// la conexión, por el camino que sea).
struct ConnectionSlot(Arc<WebHub>);

impl Drop for ConnectionSlot {
    fn drop(&mut self) {
        self.0.conn_active.fetch_sub(1, Ordering::AcqRel);
    }
}

/// FIX-09/FIX-35: acepta UNA conexión. El exceso sobre `MAX_CONCURRENT_CONNECTIONS`
/// cierra la conexión con `503` sin crear hilo y SIN matar al aceptador; si el
/// `spawner` falla (EAGAIN), el stream se cierra igualmente y el aceptador
/// sigue vivo. El `spawner` se inyecta para poder probar el camino de fallo de
/// spawn de forma determinista (test unitario).
fn spawn_connection(
    mut stream: TcpStream,
    hub: &Arc<WebHub>,
    spawner: &mut impl FnMut(TcpStream, Arc<WebHub>) -> io::Result<thread::JoinHandle<()>>,
) {
    if hub.conn_active.fetch_add(1, Ordering::AcqRel) >= MAX_CONCURRENT_CONNECTIONS {
        hub.conn_active.fetch_sub(1, Ordering::AcqRel);
        respond_capacity_exceeded(&mut stream);
        return;
    }
    // FIX-35: si el spawn falla, el stream (movido al closure) se cierra por el
    // drop del closure: el cliente recibe EOF y el aceptador **sigue vivo**
    // (antes, `thread::spawn` hacía pánico aquí y mataba el hilo de accept).
    if let Err(e) = spawner(stream, Arc::clone(hub)) {
        hub.conn_active.fetch_sub(1, Ordering::AcqRel);
        eprintln!(
            "openonyx serve: no se pudo crear el hilo de conexión ({e}); se cierra esa conexión"
        );
    }
}

fn respond_capacity_exceeded(stream: &mut TcpStream) {
    let _ = write_http_response(
        stream,
        503,
        "Service Unavailable",
        "text/plain; charset=utf-8",
        b"too many concurrent connections",
        &[],
    );
    let _ = stream.shutdown(std::net::Shutdown::Write);
}

fn handle_connection(mut stream: TcpStream, hub: Arc<WebHub>) -> io::Result<()> {
    // En Windows (Winsock2) y BSD/macOS, los sockets aceptados heredan el modo no bloqueante
    // de `TcpListener::set_nonblocking(true)`. Sin desactivarlo aquí, `read_line()` falla al
    // instante con `WSAEWOULDBLOCK` antes de que lleguen los bytes HTTP del navegador.
    stream.set_nonblocking(false)?;
    let _ = stream.set_nodelay(true);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));

    // FIX-09: presupuesto TOTAL por petición (slowloris: el timeout por read se
    // renueva con cada fragmento goteado; el deadline absoluto no).
    let deadline = Instant::now() + TOTAL_REQUEST_READ_DEADLINE;
    let peer_ip = stream.peer_addr().ok().map(|p| p.ip());
    let req = match read_http_request(stream.try_clone()?, peer_ip, deadline) {
        Ok(Some(req)) => req,
        Ok(None) => return Ok(()),
        // FIX-11/FIX-41: los errores de framing se contestan (400/408/413/431)
        // en vez de cerrar en silencio; el fallo de `read` del cuerpo (cuerpo
        // incompleto goteado) sigue cortando sin responder (no hay nada que
        // procesar y responder podría enmascarar el framing).
        Err(HttpReadError::Io(e)) => return Err(e),
        Err(other) => {
            let (status, reason) = match other {
                HttpReadError::LineTooLong => (431, "Request Header Fields Too Large"),
                HttpReadError::BodyTooLarge => (413, "Content Too Large"),
                HttpReadError::Deadline => (408, "Request Timeout"),
                HttpReadError::Malformed => (400, "Bad Request"),
                HttpReadError::Io(_) => unreachable!(),
            };
            let _ = write_json_error(&mut stream, status, reason, other.to_string().as_str(), &[]);
            let _ = stream.shutdown(std::net::Shutdown::Write);
            return Ok(());
        }
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
    // FIX-17: con lista de hosts permitidos (`--allowed-hosts`), un `Host`
    // ajeno (o ausente) se rechaza con 421. Comparación exacta en minúsculas;
    // el `Host` del cliente NUNCA se usa para generar URLs ni cookies.
    if let Some(allowed) = &hub.allowed_hosts {
        let host = req
            .headers
            .get("host")
            .map(|h| h.trim().trim_end_matches('/').to_ascii_lowercase());
        if !host.is_some_and(|h| allowed.contains(&h)) {
            write_json_error(out, 421, "Misdirected Request", "Host not allowed", &[])?;
            return Ok(None);
        }
    }

    let cors_header = resolve_cors_origin(req, hub.cors_origin.as_deref());

    // FIX-36: en modo expuesto (bind no loopback o `--behind-tls`) toda
    // respuesta lleva HSTS. No depende de la cabecera del proxy.
    let mut base_extra: Vec<(&'static str, String)> = Vec::new();
    if hub.secure_mode {
        base_extra.push(("Strict-Transport-Security", HSTS_VALUE.to_string()));
    }
    if let Some(orig) = cors_header.clone() {
        base_extra.push(("Access-Control-Allow-Origin", orig));
        base_extra.push(("Access-Control-Allow-Credentials", "true".to_string()));
    }

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

    // Guardas CSRF/auth de los `POST` mutantes (antes de auth per modern-web-guidance `security`):
    // - FIX-37: si la petición trae `Authorization: Bearer`, o se valida o se responde
    //   401. No hay fallback silencioso a la cookie (antes, CUALQUIER Bearer presente
    //   desactivaba la guarda Sec-Fetch-Site y la cookie autentica).
    // - FIX-94: sin `Sec-Fetch-Site`, la guarda mira `Origin`: un Origin ajeno al
    //   propio host ni al `--cors-origin` configurado se rechaza (antes solo se
    //   mitigaba con SameSite=Lax).
    // - FIX-42: un POST autenticado por cookie exige `Content-Type: application/json`
    //   (415): un formulario HTML cross-site `enctype="text/plain"` sin Fetch
    //   Metadata no puede ejecutar comandos.
    if req.method == "POST" {
        // FIX-37: Bearer trivalente: presente pero inválido → 401, sin fallback a
        // cookie. (El bug original: la guarda miraba si HABÍA Bearer, no si era
        // válido, y la cookie auténtica saltaba la guarda cross-site.)
        let bearer_valid = match extract_bearer_token(req) {
            Some(b) if hub.verify_api_key(b) => true,
            Some(_) => {
                write_json_error(out, 401, "Unauthorized", "invalid API key", &base_extra)?;
                return Ok(None);
            }
            None => false,
        };
        let cookie_auth = extract_session_cookie(req).is_some();
        if let Some(site) = req.headers.get("sec-fetch-site") {
            let s = site.trim().to_ascii_lowercase();
            // Cross-site / same-site POST: solo se permite con Bearer VÁLIDO cuyo
            // `Origin` sea el `--cors-origin` configurado (`cors_header` solo lleva
            // valor cuando el Origin de la petición lo empareja) o Fetch Metadata
            // same-origin/none.
            let allowed = matches!(s.as_str(), "same-origin" | "none")
                || (bearer_valid && cors_header.is_some());
            if !allowed {
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
        if cookie_auth {
            if let Some(origin) = req.headers.get("origin") {
                // FIX-94: sin Fetch Metadata, la guarda mira `Origin`: debe ser el
                // propio host o el `--cors-origin` configurado.
                if !origin_matches_own_host(origin, req, hub.secure_mode) && cors_header.is_none() {
                    write_json_error(
                        out,
                        403,
                        "Forbidden",
                        "POST blocked: foreign Origin",
                        &base_extra,
                    )?;
                    return Ok(None);
                }
            }
            // FIX-42: un POST autenticado por cookie exige
            // `Content-Type: application/json` (un formulario HTML cross-site
            // `enctype="text/plain"` sin Fetch Metadata no puede ejecutar
            // comandos).
            if !content_type_is_json(req) {
                write_json_error(
                    out,
                    415,
                    "Unsupported Media Type",
                    "API POST requires Content-Type: application/json",
                    &base_extra,
                )?;
                return Ok(None);
            }
        }
    }

    // Autenticación login / logout
    if req.method == "POST" && req.path == "/api/v1/auth/login" {
        // FIX-14: rate limit deslizante por origen + global ANTES de comprobar
        // la clave (un 429 inmediato no debe multiplicar el coste de blake3).
        // Detrás de proxy (`secure_mode`) se usa el último hop de
        // `X-Forwarded-For` (el que añade el proxy, nunca a ciegas); sin proxy,
        // la dirección del socket.
        let source_ip = resolve_login_source_ip(req, hub.secure_mode);
        let mut retry_after: Option<u64> = None;
        {
            let mut rl = hub.login_rate.lock().unwrap();
            if let Some(wait) = rl.limited_for(&source_ip, Instant::now()) {
                retry_after = Some(wait);
            }
        }
        if let Some(wait) = retry_after {
            let mut extra = base_extra.clone();
            extra.push(("Retry-After", wait.to_string()));
            write_json_error(
                out,
                429,
                "Too Many Requests",
                "too many failed login attempts, retry later",
                &extra,
            )?;
            return Ok(None);
        }

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
            hub.login_rate
                .lock()
                .unwrap()
                .record_failure(source_ip, Instant::now());
            write_json_error(out, 401, "Unauthorized", "invalid API key", &base_extra)?;
            return Ok(None);
        }

        // FIX-10: cada login emite un token de sesión NUEVO y aleatorio.
        let token = match hub.create_session() {
            Ok(t) => t,
            Err(msg) => {
                write_json_error(out, 503, "Service Unavailable", &msg, &base_extra)?;
                return Ok(None);
            }
        };
        // FIX-16/FIX-36: el modo seguro es explícito (`secure_mode`: bind no
        // loopback o `--behind-tls`). La cabecera `X-Forwarded-Proto` del
        // cliente NO se mira: un cliente directo ya no puede forzar (o
        // desforzar) el modo HTTPS.
        let cookie = if hub.secure_mode {
            format!(
                "__Host-onyx_session={}; HttpOnly; SameSite=Lax; Secure; Path=/",
                token
            )
        } else {
            format!("onyx_session={}; HttpOnly; SameSite=Lax; Path=/", token)
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
        // FIX-10: el logout revoca la sesión EN EL SERVIDOR (antes solo caducaba
        // la cookie en el cliente: una cookie copiada antes del logout seguía
        // autenticando hasta reiniciar el proceso).
        if let Some(cookie) = extract_session_cookie(req) {
            let _ = hub.revoke_session(cookie);
        }
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
    // FIX-37: un `Authorization: Bearer` presente pero inválido da 401 directo,
    // sin fallback a la cookie (antes podía enmascarar o saltarse la cookie).
    match check_auth(req, hub) {
        AuthCheck::Authenticated => {}
        AuthCheck::InvalidBearer => {
            base_extra.push(("WWW-Authenticate", "Bearer realm=\"openOnyx\"".to_string()));
            write_json_error(out, 401, "Unauthorized", "invalid API key", &base_extra)?;
            return Ok(None);
        }
        AuthCheck::Anonymous => {
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
                Err(WebCommandRejection::BadRequest(msg)) => {
                    write_json_error(out, 400, "Bad Request", &msg, &base_extra)?
                }
                // `SDD-07` Fase 4A: prompt plano bajo TimeFreeze → 409 (no se
                // encola al bus); `/…` y `!…` pasan normal.
                Err(WebCommandRejection::TimeFrozen(msg)) => {
                    write_json_error(out, 409, "Conflict", &msg, &base_extra)?
                }
            }
            Ok(None)
        }
        ("POST", "/api/v1/time/scrub") => {
            // `SDD-07` Fase 4A: el único endpoint efímero del Time Debugger.
            // 100% efímero: opera solo sobre `TimeDebugState` (0 escritura en
            // `ONYXTR2`, 0 avance de ticks) y levanta `time_scrub_dirty` +
            // `input_cv` para el push SSE inmediato de `pump_idle`.
            match hub.apply_time_scrub(&req.body) {
                Ok(snapshot) => {
                    let body =
                        serde_json::json!({ "ok": true, "time_debug": snapshot }).to_string();
                    write_http_response(
                        out,
                        200,
                        "OK",
                        "application/json; charset=utf-8",
                        body.as_bytes(),
                        &base_extra,
                    )?
                }
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
    // FIX-66: `X-Accel-Buffering: no` solo en el stream (no en `write_http_response`
    // genérico): nginx/Caddy con ese header no bufferizan el SSE (el «Live»
    // avanzaba a rachas si el proxy bufferizaba).
    let mut head = String::from(
        "HTTP/1.1 200 OK\r\n\
         Content-Type: text/event-stream; charset=utf-8\r\n\
         Cache-Control: no-cache, no-store\r\n\
         Connection: keep-alive\r\n\
         X-Accel-Buffering: no\r\n\
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

    // `SDD-04` 4C-audit: `Acquire` emparejado con el `Release` del watcher
    // de apago (`spawn_shutdown_watcher`).
    while !hub.quit.load(Ordering::Acquire) {
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
            // FIX-67: el keep-alive era un comentario SSE (`: ping`), invisible a
            // JavaScript: el cliente no podía distinguir «sigo vivo» de «el proxy
            // me tiene colgado». Ahora es un evento NOMBREADO (`event: ping`)
            // que un watchdog con `addEventListener('ping', …)` puede ver; el
            // comentario se mantiene por compatibilidad con intermediarios.
            if stream.write_all(sse_keepalive_frame().as_bytes()).is_err()
                || stream.flush().is_err()
            {
                break;
            }
            last_ping = Instant::now();
        }
    }

    Ok(())
}

/// FIX-67: frame de keep-alive SSE: comentario (intermediarios) + evento
/// `ping` visible para el cliente (watchdog).
fn sse_keepalive_frame() -> &'static str {
    ": ping\nevent: ping\ndata: {}\n\n"
}

/// Error de lectura de la petición HTTP/1.1 (FIX-08/09/11/41/12). Todo salvo
/// `Io` se CONTESTA con un 4xx (el cliente no recibe un cierre en silencio);
/// `Io` (cuerpo incompleto, conexión perdida) corta sin responder.
#[derive(Debug)]
pub(crate) enum HttpReadError {
    /// Línea de petición o bloque de cabeceras > 16 KiB (FIX-08) → 431.
    LineTooLong,
    /// Cuerpo declarado > 64 KiB (FIX-11) → 413.
    BodyTooLarge,
    /// Vencido el presupuesto total de lectura (slowloris, FIX-09) → 408.
    Deadline,
    /// Framing mal formado (FIX-41) o cabecera/Content-Length duplicado (FIX-12) → 400.
    Malformed,
    Io(io::Error),
}

impl std::fmt::Display for HttpReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HttpReadError::LineTooLong => write!(
                f,
                "request line or header block exceeds {MAX_HTTP_HEADER_BYTES} bytes"
            ),
            HttpReadError::BodyTooLarge => {
                write!(f, "body larger than {MAX_HTTP_BODY_BYTES} bytes")
            }
            HttpReadError::Deadline => write!(f, "request read deadline exceeded"),
            HttpReadError::Malformed => write!(f, "malformed HTTP/1.1 request"),
            HttpReadError::Io(e) => write!(f, "io error: {e}"),
        }
    }
}

/// Buffer de bytes con posición. `R::read` puede devolver VARIAS líneas de
/// golpe (el kernel ya tiene la petición entera; en memoria, el `Cursor` la
/// devuelve toda en la primera lectura), así que NUNCA se descarta lo que
/// va después del primer `\n`: le pertenece a la siguiente línea o al cuerpo.
struct BufLine<R: Read> {
    inner: R,
    data: Vec<u8>,
    pos: usize,
}

impl<R: Read> BufLine<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            data: Vec::with_capacity(8 * 1024),
            pos: 0,
        }
    }

    fn pending(&self) -> bool {
        self.pos < self.data.len()
    }

    /// Recarga el buffer (solo cuando no hay datos pendientes). `Ok(false)` = EOF.
    fn fill(&mut self) -> Result<bool, HttpReadError> {
        let mut chunk = [0u8; 8 * 1024];
        let n = self.inner.read(&mut chunk).map_err(HttpReadError::Io)?;
        self.data.clear();
        self.data.extend_from_slice(&chunk[..n]);
        self.pos = 0;
        Ok(n > 0)
    }

    /// Lee UNA línea terminada en CRLF con tope de bytes DURANTE la lectura
    /// (FIX-08: `read_line` sin tope dejaba acumular una línea sin `\n` sin
    /// límite de memoria, sin autenticar) y exige CRLF estricto (FIX-41: el
    /// parser antiguo aceptaba solo LF). `Ok(None)`: EOF limpio sin bytes.
    fn read_capped_line(
        &mut self,
        deadline: Instant,
        cap: usize,
    ) -> Result<Option<String>, HttpReadError> {
        let mut line: Vec<u8> = Vec::with_capacity(128);
        loop {
            if Instant::now() > deadline {
                return Err(HttpReadError::Deadline);
            }
            if !self.pending() && !self.fill()? {
                return if line.is_empty() {
                    Ok(None)
                } else {
                    Err(HttpReadError::Malformed) // línea truncada (sin CRLF)
                };
            }
            let b = self.data[self.pos];
            self.pos += 1;
            line.push(b);
            if line.len() > cap {
                return Err(HttpReadError::LineTooLong);
            }
            if b == b'\n' {
                if line.len() < 2 || line[line.len() - 2] != b'\r' {
                    return Err(HttpReadError::Malformed); // solo LF → 400
                }
                line.pop(); // '\n'
                line.pop(); // '\r'
                return Ok(Some(String::from_utf8_lossy(&line).into_owned()));
            }
        }
    }

    /// Lee EXACTAMENTE `len` bytes de cuerpo. El buffer pendiente se consume
    /// antes (el cliente puede haber enviado cuerpo y basura a la vez); un
    /// EOF a medias es error, nunca 0 (FIX-11).
    fn read_body(&mut self, deadline: Instant, len: usize) -> Result<Vec<u8>, HttpReadError> {
        let mut body = Vec::with_capacity(len.min(64 * 1024));
        while body.len() < len {
            if Instant::now() > deadline {
                return Err(HttpReadError::Deadline);
            }
            if !self.pending() && !self.fill()? {
                return Err(HttpReadError::Io(io::ErrorKind::UnexpectedEof.into()));
            }
            let take = (len - body.len()).min(self.data.len() - self.pos);
            body.extend_from_slice(&self.data[self.pos..self.pos + take]);
            self.pos += take;
        }
        Ok(body)
    }
}

/// FIX-41: línea de petición estricta `METHOD SP TARGET SP HTTP/1.x`
/// (sin versión → 400, método alfabético, sin espacios extra).
fn parse_request_line(line: &str) -> Result<(String, String), HttpReadError> {
    let mut parts = line.split(' ');
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("");
    let version = parts.next().unwrap_or("");
    let extra = parts.next();
    if extra.is_some() || method.is_empty() || target.is_empty() || version.is_empty() {
        return Err(HttpReadError::Malformed);
    }
    if !method.bytes().all(|b| b.is_ascii_alphabetic()) {
        return Err(HttpReadError::Malformed);
    }
    if version != "HTTP/1.1" && version != "HTTP/1.0" {
        return Err(HttpReadError::Malformed);
    }
    Ok((method.to_ascii_uppercase(), target.to_string()))
}

fn read_http_request<R: Read>(
    input: R,
    peer_ip: Option<std::net::IpAddr>,
    deadline: Instant,
) -> Result<Option<HttpRequest>, HttpReadError> {
    let mut reader = BufLine::new(input);
    let Some(request_line) = reader.read_capped_line(deadline, MAX_HTTP_HEADER_BYTES)? else {
        return Ok(None);
    };
    let (method, target) = parse_request_line(&request_line)?;

    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target.to_string(), String::new()),
    };

    let mut headers = BTreeMap::new();
    let mut content_len = 0usize;
    let mut total_header_bytes = request_line.len();
    loop {
        let line = match reader.read_capped_line(deadline, MAX_HTTP_HEADER_BYTES)? {
            Some(line) => line,
            None => return Err(HttpReadError::Malformed), // sin línea vacía final
        };
        if line.is_empty() {
            break;
        }
        total_header_bytes += line.len() + 2;
        if total_header_bytes > MAX_HTTP_HEADER_BYTES {
            return Err(HttpReadError::LineTooLong);
        }
        if line.starts_with(' ') || line.starts_with('\t') {
            return Err(HttpReadError::Malformed); // obs-fold (FIX-41)
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(HttpReadError::Malformed); // campo sin ':' (o ':' mal ubicado)
        };
        let name = name.to_ascii_lowercase();
        // FIX-41: nombre de campo token estricto — nada de espacio antes de ':'
        // (`Content-Length : 5`), ni control chars.
        if name.is_empty()
            || name
                .chars()
                .any(|c| c == ' ' || !('\u{21}'..='\u{7e}').contains(&c))
        {
            return Err(HttpReadError::Malformed);
        }
        // FIX-12: cabecera duplicada → 400 (RFC 9110 la exige para
        // Content-Length; se aplica a todas: en peticiones HTTP/1.1 no hay
        // cabeceras legítimas multi-valor).
        if headers.contains_key(&name) {
            return Err(HttpReadError::Malformed);
        }
        let value = value.trim_matches([' ', '\t']);
        if value.bytes().any(|b| (b < 0x20 && b != 0x09) || b == 0x7f) {
            return Err(HttpReadError::Malformed);
        }
        if name == "transfer-encoding" {
            // FIX-41: no se soporta chunked (TE+CL era ambiguo: «gana CL»).
            return Err(HttpReadError::Malformed);
        }
        if name == "content-length" {
            // FIX-41: solo dígitos ASCII (ni negativo ni no numérico: antes se
            // tomaba como 0).
            if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
                return Err(HttpReadError::Malformed);
            }
            let len: u64 = value.parse().map_err(|_| HttpReadError::Malformed)?;
            if len > MAX_HTTP_BODY_BYTES as u64 {
                return Err(HttpReadError::BodyTooLarge);
            }
            content_len = len as usize;
        }
        headers.insert(name, value.to_string());
    }

    let body = reader.read_body(deadline, content_len)?;

    Ok(Some(HttpRequest {
        method,
        path,
        query,
        headers,
        body,
        peer: peer_ip,
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

/// FIX-37: resultado de la comprobación de autenticación. Un Bearer PRESENTE
/// pero inválido es un estado distinto de «sin credenciales»: antes se
/// caía silenciosamente a la cookie, así que un Bearer erróneo podía
/// enmascarar (o validar sobre) una cookie válida.
enum AuthCheck {
    Anonymous,
    Authenticated,
    InvalidBearer,
}

fn check_auth(req: &HttpRequest, hub: &WebHub) -> AuthCheck {
    if let Some(bearer) = extract_bearer_token(req) {
        return if hub.verify_api_key(bearer) {
            AuthCheck::Authenticated
        } else {
            AuthCheck::InvalidBearer
        };
    }
    if let Some(cookie) = extract_session_cookie(req) {
        if hub.verify_session_cookie(cookie) {
            return AuthCheck::Authenticated;
        }
    }
    AuthCheck::Anonymous
}

/// FIX-42: un POST autenticado por cookie debe declarar `application/json`
/// (con parámetros opcionales, p. ej. `charset`): un formulario HTML
/// cross-site `enctype="text/plain"` sin Fetch Metadata no puede ejecutar
/// comandos sobre la API.
fn content_type_is_json(req: &HttpRequest) -> bool {
    req.headers.get("content-type").is_some_and(|ct| {
        ct.split(';')
            .next()
            .map(str::trim)
            .is_some_and(|m| m.eq_ignore_ascii_case("application/json"))
    })
}

/// FIX-94: el `Origin` coincide con el propio origen del servidor: mismo
/// host (el que envió el cliente, ignorando puertos: detrás de proxy el
/// navegador ve el puerto público, no el interno) y el esquema según el
/// modo seguro (`https` en expuesto/`--behind-tls`, `http` en loopback).
fn origin_matches_own_host(origin: &str, req: &HttpRequest, secure_mode: bool) -> bool {
    let Some((scheme, rest)) = origin.split_once("://") else {
        return false;
    };
    let scheme_ok = if secure_mode {
        scheme.eq_ignore_ascii_case("https")
    } else {
        scheme.eq_ignore_ascii_case("http")
    };
    if !scheme_ok {
        return false;
    }
    let origin_host = rest.split('/').next().unwrap_or("").trim_end_matches('/');
    let host = req
        .headers
        .get("host")
        .map(|h| h.trim().trim_end_matches('/'))
        .unwrap_or("");
    // Comparación de host ignorando el puerto de ambos lados.
    fn host_portion(addr: &str) -> &str {
        // IPv6 `[::1]:8080` → `[::1]`; IPv4/hostname `h:8080` → `h`.
        if let Some(inner) = addr.strip_prefix('[') {
            return inner.split(']').next().unwrap_or(inner);
        }
        addr.rsplit_once(':')
            .filter(|(h, _p)| !h.contains(':'))
            .map(|(h, _)| h)
            .unwrap_or(addr)
    }
    host_portion(host).eq_ignore_ascii_case(host_portion(origin_host))
}

/// FIX-14: origen del intento de login para el rate limiter. Sin modo seguro
/// (ni proxy) se usa la dirección del socket. En modo seguro (`--behind-tls`
/// o bind expuesto) se lee `X-Forwarded-For` DESPRENDIENDO de la derecha los
/// hops de confianza (loopback: la dirección privada que añade el proxy);
/// el primer hop no loopback es el cliente. Sin `X-Forwarded-For`, el socket.
fn resolve_login_source_ip(req: &HttpRequest, secure_mode: bool) -> std::net::IpAddr {
    let socket = req
        .peer
        .unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED));
    if !secure_mode {
        return socket;
    }
    let Some(xff) = req.headers.get("x-forwarded-for") else {
        return socket;
    };
    let hops: Vec<std::net::IpAddr> = xff
        .split(',')
        .filter_map(|h| h.trim().parse().ok())
        .collect();
    for hop in hops.iter().rev() {
        if hop.is_loopback() {
            continue;
        }
        return *hop;
    }
    socket
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
    use onyx_soul_agents::{
        AgentConfig, AgentHost, AgentView, AttrRegistry, Ctx, Graph, HeadlessDriver, OnyxError,
        OnyxSystem, RecordingBridge, Reply,
    };
    use onyx_workspace::{WorkspaceLedgerSystem, WorkspaceMode, WorkspaceToolBridge};
    use std::env;

    fn in_memory_exchange(hub: &Arc<WebHub>, raw_req: &str) -> (String, bool) {
        let req = read_http_request(
            io::Cursor::new(raw_req.as_bytes()),
            None,
            Instant::now() + Duration::from_secs(10),
        )
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
        let logo_req = read_http_request(
            io::Cursor::new(b"GET /logo.jpg HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n".as_slice()),
            None,
            Instant::now() + Duration::from_secs(10),
        )
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

        // 5. FIX-16/36: el modo seguro es EXPLÍCITO (`--behind-tls` o bind no
        // loopback). La cabecera `X-Forwarded-Proto` del cliente ya no se
        // consulta (un cliente directo podía forzarla).
        // 5a. Loopback sin flag: `X-Forwarded-Proto: https` sola no basta.
        let (plain_xff, _) = in_memory_exchange(
            &hub,
            &format!(
                "POST /api/v1/auth/login HTTP/1.1\r\nHost: vps.example.com\r\nX-Forwarded-Proto: https\r\nContent-Length: {}\r\n\r\n{}",
                login_body.len(),
                login_body
            ),
        );
        assert!(
            plain_xff.contains("Set-Cookie: onyx_session=")
                && !plain_xff.contains("__Host-onyx_session"),
            "X-Forwarded-Proto sola no debe activar el modo seguro: {plain_xff}"
        );
        // 5b. Hub con `--behind-tls`: cookie `__Host-…; Secure` + HSTS.
        let secure_cfg = cfg.clone().behind_tls();
        let secure_driver = WebDriver::start(host.vocab(), &secure_cfg, None, None).unwrap();
        let secure_hub = Arc::clone(&secure_driver.hub);
        let (https_login_resp, _) = in_memory_exchange(
            &secure_hub,
            &format!(
                "POST /api/v1/auth/login HTTP/1.1\r\nHost: vps.example.com\r\nContent-Length: {}\r\n\r\n{}",
                login_body.len(),
                login_body
            ),
        );
        assert!(
            https_login_resp.contains("Set-Cookie: __Host-onyx_session=")
                && https_login_resp.contains("Secure"),
            "{https_login_resp}"
        );
        assert!(
            https_login_resp.contains("Strict-Transport-Security"),
            "el modo expuesto debe emitir HSTS: {https_login_resp}"
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

    /// `SDD-03` Fase 3: la cabecera `model`/`effort` del snapshot web sigue el
    /// objetivo LLM vivo (hot-swap en frontera de tick), no el campo estático
    /// de arranque (regresión: la cabecera "mentía" hasta reiniciar).
    #[test]
    fn web_state_snapshot_model_and_effort_follow_live_llm_handle() {
        use crate::llm_profile::{LlmConfig, LlmProvider};

        let cfg = WebServerConfig::resolve(
            "127.0.0.1:0",
            Some("web-live-llm-key-123456789".to_string()),
            None,
        )
        .unwrap();
        let host = AgentHost::builder(1).build().unwrap();
        let startup = LlmConfig {
            provider: LlmProvider::Gemini,
            api_key: "startup-key".to_string(),
            model: "gemini-4-hydrogen".to_string(),
            effort: None,
            thinking_budget: None,
            api_base: None,
            project_instructions: String::new(),
        };
        let handle = LlmProfileHandle::new(&startup, &[], false);
        let web_driver = WebDriver::start(host.vocab(), &cfg, None, None)
            .unwrap()
            // Campo estático deliberadamente antiguo: con handle presente debe
            // quedar opacado por el objetivo vivo.
            .with_model_info("stale-static-model", None)
            .with_llm_profile(handle.clone());
        let web_handle = web_driver.handle();
        let hub = Arc::clone(&web_driver.hub);
        let snapshot_json = |hub: &Arc<WebHub>| {
            hub.state
                .lock()
                .map(|g| g.snapshot_json.clone())
                .unwrap_or_else(|_| "{}".to_string())
        };

        // Tick 0: la cabecera muestra el objetivo de arranque del handle.
        web_handle.render(host.graph());
        let json = snapshot_json(&hub);
        assert!(json.contains("\"model\":\"gemini-4-hydrogen\""), "{json}");
        assert!(json.contains("\"effort\":\"auto\""), "{json}");

        // Hot-swap (lo que hace `apply_llm_target` en la frontera de tick):
        // el próximo render publica la cabecera actualizada.
        handle
            .set_model("gemini-4-deuterium")
            .expect("set_model sobre el perfil por defecto");
        handle.set_effort(Some(ReasoningEffort::High));
        web_handle.render(host.graph());
        let json = snapshot_json(&hub);
        assert!(json.contains("\"model\":\"gemini-4-deuterium\""), "{json}");
        assert!(json.contains("\"effort\":\"high\""), "{json}");
    }

    /// `SDD-03` Fase 4: el selector en Web lee el snapshot vivo —
    /// `profile_id`/`provider`/`profiles`/`available_providers` salen del
    /// handle (con perfil no disponible y su motivo), y **ninguna
    /// `api_key`/`api_key_env` aparece en el JSON serializado** (invariante
    /// de seguridad: el snapshot web solo lleva metadatos).
    #[test]
    fn web_state_snapshot_exposes_llm_profiles_without_credentials() {
        use crate::llm_profile::{LlmConfig, LlmProvider};
        use onyx_workspace::LlmProfileConfig;

        let cfg = WebServerConfig::resolve(
            "127.0.0.1:0",
            Some("web-llm-profiles-key-123456".to_string()),
            None,
        )
        .unwrap();
        let host = AgentHost::builder(1).build().unwrap();
        let startup = LlmConfig {
            provider: LlmProvider::Gemini,
            api_key: "startup-key-secret-do-not-leak".to_string(),
            model: "gemini-4-hydrogen".to_string(),
            effort: None,
            thinking_budget: None,
            api_base: None,
            project_instructions: String::new(),
        };
        // Perfil OpenAI sin `api_key_env` resuelta → no disponible, con motivo.
        let profiles = vec![LlmProfileConfig {
            id: "openai-local".to_string(),
            provider: "openai".to_string(),
            model: "gpt-4.1-mini".to_string(),
            effort: None,
            api_base: None,
            api_key_env: Some("ONYX_TEST_LLM_OPENAI_SECRET".to_string()),
        }];
        // Determinismo: aislar los envs de credenciales/base del CI para que
        // `available_providers` salga solo del registro.
        let saved = [
            "ONYX_TEST_LLM_OPENAI_SECRET",
            "OPENAI_API_KEY",
            "OPENAI_API_BASE",
            "OPENAI_BASE_URL",
            "ONYX_API_KEY",
        ]
        .into_iter()
        .map(|name| (name, env::var(name).ok()))
        .collect::<Vec<_>>();
        unsafe {
            for (name, _) in &saved {
                env::remove_var(name);
            }
        }
        let handle = LlmProfileHandle::new(&startup, &profiles, false);
        let web_driver = WebDriver::start(host.vocab(), &cfg, None, None)
            .unwrap()
            .with_model_info("stale-static-model", None)
            .with_llm_profile(handle);
        let web_handle = web_driver.handle();
        let hub = Arc::clone(&web_driver.hub);
        let snapshot_json = |hub: &Arc<WebHub>| {
            hub.state
                .lock()
                .map(|g| g.snapshot_json.clone())
                .unwrap_or_else(|_| "{}".to_string())
        };

        web_handle.render(host.graph());
        let json = snapshot_json(&hub);

        // Objetivo vivo: `profile_id` + `provider` + modelo/esfuerzo.
        assert!(json.contains("\"profile_id\":\"default\""), "{json}");
        assert!(json.contains("\"provider\":\"gemini\""), "{json}");
        assert!(json.contains("\"model\":\"gemini-4-hydrogen\""), "{json}");
        assert!(json.contains("\"effort\":\"auto\""), "{json}");
        // Con OpenAI sin credenciales, solo Gemini queda disponible.
        assert!(
            json.contains("\"available_providers\":[\"gemini\"]"),
            "{json}"
        );

        // Perfiles registrados: el no disponible sale deshabilitado con motivo.
        assert!(json.contains("\"profiles\":["), "{json}");
        assert!(json.contains("\"id\":\"openai-local\""), "{json}");
        assert!(json.contains("\"available\":false"), "{json}");
        assert!(
            json.contains("ONYX_TEST_LLM_OPENAI_SECRET"),
            "motivo accionable del perfil no disponible: {json}"
        );

        unsafe {
            for (name, value) in saved {
                match value {
                    Some(v) => env::set_var(name, v),
                    None => env::remove_var(name),
                }
            }
        }

        // Invariante de seguridad: 0 credenciales en el JSON serializado.
        assert!(!json.contains("api_key"), "api_key en snapshot web: {json}");
        assert!(
            !json.contains("api_key_env"),
            "api_key_env en snapshot web: {json}"
        );
        assert!(
            !json.contains("startup-key-secret-do-not-leak"),
            "valor de credencial en snapshot web: {json}"
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

    /// `SDD-04` 4C-audit: el 1er Ctrl+C (simulado con la misma llamada que
    /// hace el handler de señal) propaga `SERVE_SHUTDOWN` → `WebHub::quit`
    /// mediante el watcher, y `pump_idle` se sale sin el wait de 35 ms
    /// (sin deadlock). El guard RAII restaura el handler previo al salir.
    #[test]
    fn serve_shutdown_request_propagates_to_quit_flag_and_wakes_idle_pump() {
        reset_serve_shutdown_request();
        let cfg = WebServerConfig::resolve(
            "127.0.0.1:0",
            Some("test-api-key-1234567890".to_string()),
            None,
        )
        .unwrap();
        let host = AgentHost::builder(1).build().unwrap();
        let driver = WebDriver::start(host.vocab(), &cfg, None, None).unwrap();
        let handle = driver.handle();
        let quit = driver.quit_flag();

        let guard = CtrlCGuard::new();
        let watcher = driver.spawn_shutdown_watcher();
        assert!(!quit.load(Ordering::Acquire));

        // Simula el 1er Ctrl+C: la misma llamada que ejecuta el handler.
        mark_serve_shutdown_request();

        // El watcher (polling 5 ms) propaga la bandera a `WebHub::quit`.
        let deadline = Instant::now() + Duration::from_millis(500);
        while !quit.load(Ordering::Acquire) {
            assert!(
                Instant::now() < deadline,
                "el watcher debe propagar SERVE_SHUTDOWN a quit"
            );
            thread::sleep(Duration::from_millis(2));
        }

        // Con `quit` activo, `pump_idle` no hace el wait de 35 ms.
        let t0 = Instant::now();
        handle.pump_idle(host.graph());
        let elapsed = t0.elapsed();
        assert!(
            elapsed < Duration::from_millis(30),
            "pump_idle debe salir sin el wait de 35 ms al apagar, tardó {elapsed:?}"
        );

        drop(guard);
        watcher
            .join()
            .expect("el watcher termina tras propagar el apago");
        reset_serve_shutdown_request();
    }

    // ─────────────────────────────────────────────────────────────────────────────
    // `SDD-07` Fase 4A — TEST-07.7: paridad Web del Time Debugger (0 navegador
    // real: exchanges HTTP en memoria sobre el hub + contratos estáticos PWA).
    // ─────────────────────────────────────────────────────────────────────────────

    const TD_KEY: &str = "td-web-api-key-1122334455";

    /// `TimeDebugState` con ticks 0..=2 en el anillo (present = 2, cursor = 2).
    fn td_state_with_ticks() -> TimeDebugState {
        let td = TimeDebugState::new(None);
        td.record_tick(0, &[0]);
        td.record_tick(1, &[0]);
        td.record_tick(2, &[0]);
        td
    }

    fn web_driver_with_time_debug(
        host: &AgentHost,
        td: TimeDebugState,
    ) -> (WebDriver, Arc<WebHub>) {
        let cfg = WebServerConfig::resolve("127.0.0.1:0", Some(TD_KEY.to_string()), None).unwrap();
        let driver = WebDriver::start(host.vocab(), &cfg, None, None)
            .unwrap()
            .with_time_debug(td);
        let hub = Arc::clone(&driver.hub);
        (driver, hub)
    }

    fn td_post(hub: &Arc<WebHub>, body: &str, extra_headers: &str) -> String {
        let (resp, _) = in_memory_exchange(
            hub,
            &format!(
                "POST /api/v1/time/scrub HTTP/1.1\r\nHost: 127.0.0.1\r\n{extra_headers}Content-Length: {}\r\n\r\n{body}",
                body.len()
            ),
        );
        resp
    }

    fn td_cmd(hub: &Arc<WebHub>, command: &str) -> String {
        let body = format!("{{\"command\":{}}}", serde_json::json!(command));
        let (resp, _) = in_memory_exchange(
            hub,
            &format!(
                "POST /api/v1/command HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {TD_KEY}\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            ),
        );
        resp
    }

    #[test]
    fn test_07_7_1_time_scrub_endpoint_auth_csrf_ephemeral_and_actionable_400() {
        let host = AgentHost::builder(1).build().unwrap();
        let td = td_state_with_ticks();
        td.scrub_to(1).unwrap(); // ancla en el pasado → auto-freeze (cursor=1, present=2)

        let (_driver, hub) = web_driver_with_time_debug(&host, td.clone());

        // 1a. 401 sin credenciales (el endpoint hereda el guard de auth).
        let unauth = td_post(&hub, "{\"action\":\"scrub_to\",\"tick\":2}", "");
        assert!(unauth.starts_with("HTTP/1.1 401 Unauthorized"), "{unauth}");
        assert_eq!(td.cursor(), 1, "el 401 no debe mutar el depurador");

        // 1b. 403 cross-site por el guard CSRF (Sec-Fetch-Site), sin Bearer.
        let csrf = td_post(
            &hub,
            "{\"action\":\"scrub_to\",\"tick\":2}",
            "Sec-Fetch-Site: cross-site\r\n",
        );
        assert!(csrf.starts_with("HTTP/1.1 403 Forbidden"), "{csrf}");

        // 1c. 200 con scrub válido: el body lleva el snapshot `time_debug`
        // actualizado (cursor en el presente, descongelado).
        let ok = td_post(
            &hub,
            "{\"action\":\"scrub_to\",\"tick\":2}",
            &format!("Authorization: Bearer {TD_KEY}\r\n"),
        );
        assert!(ok.starts_with("HTTP/1.1 200 OK"), "{ok}");
        assert!(ok.contains("\"ok\":true"), "{ok}");
        assert!(ok.contains("\"time_debug\":{"), "{ok}");
        assert!(ok.contains("\"cursor_tick\":2"), "{ok}");
        assert!(ok.contains("\"frozen\":false"), "{ok}");
        assert_eq!(td.cursor(), 2, "el scrub debe mutar el estado compartido");

        // 1d. 400 accionable: tick futuro.
        let future = td_post(
            &hub,
            "{\"action\":\"scrub_to\",\"tick\":99}",
            "Authorization: Bearer td-web-api-key-1122334455\r\n",
        );
        assert!(future.starts_with("HTTP/1.1 400 Bad Request"), "{future}");
        assert!(future.contains("futuro"), "{future}");

        // 1e. 400 accionable: acción desconocida.
        let bad_action = td_post(
            &hub,
            "{\"action\":\"warp\"}",
            "Authorization: Bearer td-web-api-key-1122334455\r\n",
        );
        assert!(
            bad_action.starts_with("HTTP/1.1 400 Bad Request"),
            "{bad_action}"
        );
        assert!(
            bad_action.contains("unknown time/scrub action"),
            "{bad_action}"
        );

        // 1f. 100% efímero: 0 eventos encolados en el bus por el scrub.
        let queued = hub
            .state
            .lock()
            .map(|g| g.incoming_events.len())
            .unwrap_or(usize::MAX);
        assert_eq!(queued, 0, "el scrub no encola eventos en incoming_events");
    }

    #[test]
    fn test_07_7_2_time_frozen_command_409_and_escape_slashes_pass() {
        let host = AgentHost::builder(1).build().unwrap();
        let td = td_state_with_ticks();
        td.scrub_to(1).unwrap(); // congelado mirando el pasado

        let (_driver, hub) = web_driver_with_time_debug(&host, td);

        // 2a. Prefix interno reservado → 400 (aun bajo freeze).
        let reserved = td_cmd(&hub, "/__onyx_time_warp");
        assert!(
            reserved.starts_with("HTTP/1.1 400 Bad Request"),
            "{reserved}"
        );
        assert!(
            reserved.contains("reserved internal command prefix"),
            "{reserved}"
        );

        // 2b. Prompt plano bajo TIME FROZEN → 409 Conflict accionable.
        let frozen = td_cmd(&hub, "hello onyx, edit the file");
        assert!(frozen.starts_with("HTTP/1.1 409 Conflict"), "{frozen}");
        assert!(frozen.contains("Paused in TIME FROZEN"), "{frozen}");

        // 2c. Vías de escape selladas pasan → 200.
        let go = td_cmd(&hub, "/go");
        assert!(go.starts_with("HTTP/1.1 200 OK"), "{go}");
        let inspect = td_cmd(&hub, "/graph inspect mem");
        assert!(inspect.starts_with("HTTP/1.1 200 OK"), "{inspect}");
        let bang = td_cmd(&hub, "!echo hi");
        assert!(bang.starts_with("HTTP/1.1 200 OK"), "{bang}");

        // 2d. Los escapes sí encolan comandos en el bus (el 409 no encoló nada).
        let queued = hub
            .state
            .lock()
            .map(|g| g.incoming_events.len())
            .unwrap_or(usize::MAX);
        assert_eq!(
            queued, 3,
            "/go + /graph + !echo encolan; el 409 y el 400 no"
        );
    }

    /// `SDD-07` Fase 4B (fix de auditoría — regresión del deadlock): con el
    /// agente raíz ocupado (`root_busy`, Thinking) **y** TIME FROZEN (el
    /// breakpoint principal pre-LLM de SDD-07), las vías de escape deben
    /// llegar al bus inmediato (`incoming_events`), NO a la cola de prompts
    /// pendientes (donde esperarían un idle que nunca llega mientras el
    /// tiempo sigue congelado → el usuario queda atrapado en el freeze).
    #[test]
    fn test_07_7_5_escape_commands_immediate_while_root_busy_frozen() {
        let host = AgentHost::builder(1).build().unwrap();
        let td = td_state_with_ticks();
        td.scrub_to(1).unwrap(); // frozen + cursor < present (breakpoint pre-LLM)

        let (_driver, hub) = web_driver_with_time_debug(&host, td);
        // Escenario crítico: el agente está pausado en Thinking y congelado.
        hub.state.lock().unwrap().root_busy = true;

        for cmd in ["/go", "/step-llm", "/graph drop-mem 1", "!echo hi"] {
            let resp = td_cmd(&hub, cmd);
            assert!(resp.starts_with("HTTP/1.1 200 OK"), "{cmd} → {resp}");
        }

        let guard = hub.state.lock().unwrap();
        assert_eq!(
            guard.incoming_events.len(),
            4,
            "las 4 vías de escape entran al bus inmediato"
        );
        assert!(
            guard.queued_prompts.is_empty(),
            "nada se encola en queued_prompts (deadlock: idle que nunca llega)"
        );
    }

    /// Sistema de test: siembra 1 efecto registrado (`hello.txt`, tick 2) en el
    /// grafo vivo para que `sync_live_effects` capture algo real (0 I/O).
    struct TdEffectSeeder {
        wv: Option<WorkspaceVocab>,
        seeded: bool,
    }

    impl OnyxSystem for TdEffectSeeder {
        fn name(&self) -> &str {
            "test_td_effect_seeder"
        }

        fn init(&mut self, attrs: &mut dyn AttrRegistry) -> Result<(), OnyxError> {
            self.wv = Some(WorkspaceVocab::resolve(attrs));
            Ok(())
        }

        fn run(&mut self, ctx: &mut Ctx) {
            if self.seeded {
                return;
            }
            let Some(wv) = self.wv else {
                return;
            };
            let id = ctx.graph.create();
            ctx.graph.set(id, wv.effect_tick, Value::Int(2));
            ctx.graph.set(id, wv.effect_agent, Value::Int(1));
            ctx.graph
                .set(id, wv.effect_tool, Value::Text("fs_edit".to_string()));
            ctx.graph
                .set(id, wv.effect_target, Value::Text("hello.txt".to_string()));
            ctx.graph
                .set(id, wv.effect_reversibility, Value::Text("git".to_string()));
            ctx.graph.set(
                id,
                wv.effect_summary,
                Value::Text("test effect".to_string()),
            );
            self.seeded = true;
        }
    }

    #[test]
    fn test_07_7_3_state_snapshot_time_debug_full_after_scrub_with_live_effects() {
        let mut host = AgentHost::builder(1)
            .system(Box::new(TdEffectSeeder {
                wv: None,
                seeded: false,
            }))
            .build()
            .unwrap();
        let ws_vocab = onyx_workspace::WorkspaceVocab::resolve_with(|n| host.attr(n));
        host.run(&mut HeadlessDriver::new(), 3); // el seeder escribe el efecto (tick 2)

        let td = TimeDebugState::new(None);
        td.record_tick(0, &host.graph().export());
        td.record_tick(1, &host.graph().export());
        td.record_tick(2, &host.graph().export());
        // Borde de sesión: captura los efectos del grafo vivo (fuente de
        // `pending_revert_files` — el preview(cursor) no los contiene).
        td.sync_live_effects(host.graph(), &ws_vocab);
        td.scrub_to(1).unwrap(); // ancla en el pasado → auto-freeze

        let (driver, hub) = web_driver_with_time_debug(&host, td);

        // Publica el snapshot con la proyección del cursor scrubbeado
        // (render = publish_snapshot sin broadcast SSE).
        driver.handle().render(host.graph());

        let (resp, _) = in_memory_exchange(
            &hub,
            &format!(
                "GET /api/v1/state HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {TD_KEY}\r\n\r\n"
            ),
        );
        assert!(resp.starts_with("HTTP/1.1 200 OK"), "{resp}");
        assert!(
            resp.contains("\"time_debug\":{"),
            "el snapshot debe llevar time_debug, {resp}"
        );
        assert!(resp.contains("\"frozen\":true"), "{resp}");
        assert!(resp.contains("\"cursor_tick\":1"), "{resp}");
        assert!(resp.contains("\"present_tick\":2"), "{resp}");
        assert!(
            resp.contains("\"available_ticks\":[0,1,2]"),
            "available = anillo ∪ presente, {resp}"
        );
        assert!(
            resp.contains("\"pending_revert_files\":[\"hello.txt\"]"),
            "pending_revert_files sale de live_effects (tick 2 > cursor 1), {resp}"
        );
    }

    #[test]
    fn test_07_7_4_pwa_assets_contract_time_debugger() {
        // index.html: `#tick-pill` es un <button> (la etiqueta de apertura
        // más cercana antes del id debe ser <button sin cierre intermedio).
        let pos = ASSET_INDEX_HTML
            .find("id=\"tick-pill\"")
            .expect("tick-pill");
        let before = &ASSET_INDEX_HTML[..pos];
        let btn_start = before
            .rfind("<button")
            .expect("debe existir un <button antes del id tick-pill");
        assert!(
            !before[btn_start..].contains("</button>"),
            "tick-pill debe ser el id de un <button> abierto (no de un span)"
        );
        // Barra ámbar + diálogo de edición de memoria + controles.
        for needle in [
            "id=\"time-debug-bar\"",
            "id=\"tdb-status\"",
            "id=\"tdb-slider\"",
            "id=\"tdb-prev\"",
            "id=\"tdb-next\"",
            "id=\"tdb-freeze\"",
            "id=\"tdb-step\"",
            "id=\"tdb-step-llm\"",
            "id=\"tdb-resume\"",
            "id=\"tdb-revert-warn\"",
            "id=\"tdb-bench-copy\"",
            "id=\"edit-mem-dialog\"",
            "id=\"edit-mem-input\"",
        ] {
            assert!(
                ASSET_INDEX_HTML.contains(needle),
                "index.html falta {needle}"
            );
        }

        // app.js: endpoint scrub, comandos sellados, hints y 0 innerHTML.
        for needle in [
            "/api/v1/time/scrub",
            "scrub_delta",
            "return_to_present",
            "/graph drop-mem",
            "/graph edit-mem",
            "{ key: '/time',",
            "{ key: '/go',",
            "{ key: '/step',",
            "{ key: '/step-llm',",
            "{ key: '/read',",
            "{ key: '/inject',",
            "TIME FROZEN",
            "attachMemActions",
        ] {
            assert!(ASSET_APP_JS.contains(needle), "app.js falta {needle}");
        }
        assert!(
            !ASSET_APP_JS.contains("innerHTML"),
            "app.js no puede usar innerHTML (CSP estricto)"
        );

        // styles.css: barra ámbar, pill congelada y botones de memoria.
        for needle in [
            ".time-debug-bar",
            "#tick-pill.is-frozen",
            ".mem-actions",
            ".tdb-bench",
        ] {
            assert!(
                ASSET_STYLES_CSS.contains(needle),
                "styles.css falta {needle}"
            );
        }
    }

    // ─────────────────────────────────────────────────────────────────────────────
    // FIX-35 (unitario determinista): el fallo del spawner (EAGAIN por RLIMIT_NPROC)
    // no puede matar al aceptador; el camino se prueba inyectando un spawner que
    // falla SIEMPRE, sin depender de `unshare`/`prlimit` (ver vps_audit para el
    // repro de integración bajo userns).
    // ─────────────────────────────────────────────────────────────────────────────

    fn unit_hub() -> Arc<WebHub> {
        let cfg = WebServerConfig::resolve(
            "127.0.0.1:0",
            Some("test-api-key-1234567890".to_string()),
            None,
        )
        .unwrap();
        WebHub::new(&cfg, None, None).unwrap()
    }

    /// FIX-35: `spawn_connection` con un spawner que siempre falla (EAGAIN):
    /// sin pánico, el stream se cierra (el cliente ve EOF) y el contador de
    /// conexiones activas vuelve a 0: el aceptador puede seguir sirviendo.
    #[test]
    fn spawn_connection_failing_spawner_keeps_acceptor_alive() {
        let hub = unit_hub();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let mut client = TcpStream::connect(addr).unwrap();
        client
            .write_all(b"GET /api/v1/health HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
            .unwrap();
        let (stream, _) = listener.accept().unwrap();

        let mut always_fail =
            |_: TcpStream, _: Arc<WebHub>| -> io::Result<thread::JoinHandle<()>> {
                Err(io::ErrorKind::Other.into())
            };
        // No debe pánicar (antes, `thread::spawn` lo hacía DENTRO del aceptador).
        spawn_connection(stream, &hub, &mut always_fail);
        assert_eq!(
            hub.conn_active.load(Ordering::Acquire),
            0,
            "el contador de conexiones activas debe volver a 0"
        );

        // El cliente ve el cierre de esa conexión: EOF (FIN) o RST (el kernel
        // manda RST si el servidor cierra con bytes leídos pendientes, como
        // aquí: la petición nunca se leyó). Ambas formas cierran la conexión.
        client
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut buf = [0u8; 16];
        match client.read(&mut buf) {
            Ok(0) => {}
            Err(e) if e.kind() == io::ErrorKind::ConnectionReset => {}
            other => panic!("la conexión con spawn fallido debe cerrarse: {other:?}"),
        }

        // El listener sigue aceptando (el aceptador no ha muerto).
        drop(client);
        let mut client2 = TcpStream::connect(addr).unwrap();
        assert!(client2.write_all(b"GET / HTTP/1.1\r\n\r\n").is_ok());
    }

    /// FIX-09 (unitario): por encima de `MAX_CONCURRENT_CONNECTIONS` la conexión
    /// recibe `503` SIN crear hilo (el spawner no se llama) y el contador no se
    /// corrompe.
    #[test]
    fn spawn_connection_over_capacity_answers_503_without_thread() {
        let hub = unit_hub();
        hub.conn_active
            .store(MAX_CONCURRENT_CONNECTIONS, Ordering::Release);

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let mut client = TcpStream::connect(addr).unwrap();
        let (stream, _) = listener.accept().unwrap();

        let mut never = |_: TcpStream, _: Arc<WebHub>| -> io::Result<thread::JoinHandle<()>> {
            panic!("el spawner no debe llamarse sin capacidad")
        };
        spawn_connection(stream, &hub, &mut never);
        assert_eq!(
            hub.conn_active.load(Ordering::Acquire),
            MAX_CONCURRENT_CONNECTIONS,
            "el exceso no debe corromper el contador"
        );

        let mut buf = Vec::new();
        let _ = client.read_to_end(&mut buf);
        let resp = String::from_utf8_lossy(&buf);
        assert!(resp.starts_with("HTTP/1.1 503"), "{resp}");
    }

    // ─────────────────────────────────────────────────────────────────────────────
    // FIX-12 / FIX-16 / FIX-17 / FIX-67: tests deterministas nuevos (en main no
    // había ningún test de estos; el verde anterior no los cubría).
    // ─────────────────────────────────────────────────────────────────────────────

    /// FIX-12: una cabecera duplicada (en CUALQUIER orden) → `Malformed`, que el
    /// handler contesta con 400 (no cierre en silencio). RFC 9110 la exige para
    /// `Content-Length`; la aplicamos a cualquier cabecera: en peticiones
    /// HTTP/1.1 no hay cabeceras legítimas multi-valor.
    #[test]
    fn fix12_duplicate_headers_are_rejected_in_any_order() {
        let deadline = Instant::now() + Duration::from_secs(5);
        let dup = |first: &str, second: &str| {
            read_http_request(
                io::Cursor::new(
                    format!(
                        "GET /api/v1/health HTTP/1.1\r\nHost: 127.0.0.1\r\n{first}\r\n{second}\r\n\r\n"
                    )
                    .as_bytes(),
                ),
                None,
                deadline,
            )
        };
        assert!(matches!(
            dup("Content-Length: 0", "Content-Length: 0"),
            Err(HttpReadError::Malformed)
        ));
        // Duplicada que NO es Content-Length (cualquier cabecera): igual 400.
        assert!(matches!(
            dup("X-Forwarded-For: 1.2.3.4", "X-Forwarded-For: 5.6.7.8"),
            Err(HttpReadError::Malformed)
        ));
        assert!(matches!(
            dup("Content-Length: 1", "Content-Length: 2"),
            Err(HttpReadError::Malformed)
        ));
        // Control: `Content-Length` único sigue siendo válido.
        let single = read_http_request(
            io::Cursor::new(
                b"GET /api/v1/health HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 0\r\n\r\n",
            ),
            None,
            deadline,
        );
        assert!(matches!(single, Ok(Some(_))));
    }

    /// FIX-16: el modo seguro es EXPLÍCITO (bind no loopback o `--behind-tls`):
    /// cookie `__Host-…; Secure` + HSTS en TODAS las respuestas. La cabecera
    /// `X-Forwarded-Proto` de un cliente directo ya no se consulta (podía
    /// forzar o desforzar el modo HTTPS).
    fn session_cookie_line(resp: &str) -> &str {
        resp.lines()
            .find(|l| l.starts_with("Set-Cookie: "))
            .unwrap_or_else(|| panic!("no Set-Cookie en la respuesta del login: {resp}"))
    }

    #[test]
    fn fix16_secure_cookie_and_hsts_only_when_explicit() {
        let body = r#"{"api_key":"test-api-key-1234567890"}"#;
        let login = |hub: &Arc<WebHub>| {
            in_memory_exchange(
                hub,
                &format!(
                    "POST /api/v1/auth/login HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                ),
            )
            .0
        };

        // 1. Loopback sin flag: cookie plana, sin `Secure`, sin HSTS — ni
        //    aunque el cliente mande `X-Forwarded-Proto: https` forzada.
        let local = unit_hub();
        let resp = login(&local);
        assert!(resp.starts_with("HTTP/1.1 200 OK"), "{resp}");
        let cookie = session_cookie_line(&resp);
        assert!(
            cookie.contains("onyx_session=")
                && !cookie.contains("__Host-")
                && !cookie.contains("Secure"),
            "{cookie}"
        );
        assert!(
            !resp.contains("Strict-Transport-Security"),
            "loopback sin flag no debe emitir HSTS: {resp}"
        );
        let forged = in_memory_exchange(
            &local,
            &format!(
                "POST /api/v1/auth/login HTTP/1.1\r\nHost: 127.0.0.1\r\nX-Forwarded-Proto: https\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            ),
        )
        .0;
        let forged_cookie = session_cookie_line(&forged);
        assert!(
            forged_cookie.contains("onyx_session=") && !forged_cookie.contains("__Host-"),
            "X-Forwarded-Proto forzada no debe activar el modo seguro: {forged}"
        );

        // 2. Loopback + `--behind-tls`: `__Host-…; Secure` + HSTS en el login
        //    Y en cualquier otra respuesta.
        let secured = WebHub::new(
            &WebServerConfig::resolve(
                "127.0.0.1:0",
                Some("test-api-key-1234567890".to_string()),
                None,
            )
            .unwrap()
            .behind_tls(),
            None,
            None,
        )
        .unwrap();
        let resp = login(&secured);
        let cookie = session_cookie_line(&resp);
        assert!(
            cookie.contains("__Host-onyx_session=")
                && cookie.contains("Secure")
                && cookie.contains("Path=/"),
            "{cookie}"
        );
        assert!(
            resp.contains("Strict-Transport-Security: max-age=31536000; includeSubDomains"),
            "{resp}"
        );
        let health = in_memory_exchange(
            &secured,
            "GET /api/v1/health HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n",
        )
        .0;
        assert!(
            health.contains("Strict-Transport-Security"),
            "HSTS debe ir en toda respuesta en modo expuesto: {health}"
        );

        // 3. Bind no loopback sin flag: modo expuesto (despliegue VPS).
        let exposed = WebServerConfig::resolve(
            "0.0.0.0:0",
            Some("test-api-key-1234567890".to_string()),
            None,
        )
        .unwrap();
        assert!(exposed.secure_mode());
        let exposed_hub = WebHub::new(&exposed, None, None).unwrap();
        let resp = login(&exposed_hub);
        let cookie = session_cookie_line(&resp);
        assert!(
            cookie.contains("__Host-onyx_session=") && cookie.contains("Secure"),
            "{cookie}"
        );
        assert!(resp.contains("Strict-Transport-Security"), "{resp}");
    }

    /// FIX-17: `--allowed-hosts` es OPT-IN. Con la lista, un `Host` ajeno o
    /// ausente → 421 (comparación exacta en minúsculas); sin lista, no hay
    /// validación (los tests de VPS mandan `Host: x` y deben seguir pasando).
    #[test]
    fn fix17_allowed_hosts_reject_foreign_or_missing_host() {
        let cfg = WebServerConfig::resolve(
            "127.0.0.1:0",
            Some("test-api-key-1234567890".to_string()),
            None,
        )
        .unwrap()
        .with_allowed_hosts(vec![
            "vps.example.com".to_string(),
            "ui.example.com".to_string(),
        ]);
        let hub = WebHub::new(&cfg, None, None).unwrap();
        let get = |host: Option<&str>| {
            let hdr = host.map(|h| format!("Host: {h}\r\n")).unwrap_or_default();
            in_memory_exchange(&hub, &format!("GET /api/v1/health HTTP/1.1\r\n{hdr}\r\n")).0
        };
        assert!(get(Some("vps.example.com")).starts_with("HTTP/1.1 200"));
        // La comparación es insensible a mayúsculas.
        assert!(get(Some("VPS.EXAMPLE.COM")).starts_with("HTTP/1.1 200"));
        // Host ajeno → 421 Misdirected Request.
        let evil = get(Some("evil.example.com"));
        assert!(evil.starts_with("HTTP/1.1 421"), "{evil}");
        // Host ausente con lista configurada → 421 (misdirección es la norma).
        let none = get(None);
        assert!(none.starts_with("HTTP/1.1 421"), "{none}");

        // Control: sin lista, cualquier `Host` se atiende.
        let open = unit_hub();
        let r = in_memory_exchange(&open, "GET /api/v1/health HTTP/1.1\r\nHost: x\r\n\r\n").0;
        assert!(r.starts_with("HTTP/1.1 200"), "{r}");
    }

    /// FIX-67: el keep-alive SSE es un evento NOMBREADO (`event: ping`),
    /// visible para JS con `addEventListener('ping', …). Un comentario puro
    /// (`: ping`) era invisible al cliente: «proxy que me tiene colgado» y
    /// «servidor lento» eran indistinguibles. El comentario se mantiene por
    /// compatibilidad con intermediarios. `app.js` solo escucha los eventos
    /// `state`/`stream`, así que un `ping` nunca choca con `onmessage`.
    #[test]
    fn fix67_sse_keepalive_frame_is_a_named_event() {
        assert_eq!(sse_keepalive_frame(), ": ping\nevent: ping\ndata: {}\n\n");
        assert!(sse_keepalive_frame().contains("event: ping"));
        assert!(!sse_keepalive_frame().contains("event: message"));
    }
}
