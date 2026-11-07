//! Idioma del cerebro.
//! Todo lo que el core entiende es un `OnyxEvent`.
//! Los drivers traducen el mundo externo a este idioma.

use crate::graph::{Attr, ConceptId, Value};

/// Todo lo que el cerebro entiende.
#[derive(Debug, Clone)]
pub enum OnyxEvent {
    /// Ciclo de vida del motor. Lo emite el tick o los sistemas.
    System(SystemEvent),

    /// Entrada de plataforma. Solo lo deben emitir los drivers.
    Ingress(IngressEvent),

    /// Hechos del dominio. El core no interpreta el `kind`.
    Domain(DomainEvent),
}

/// Eventos de ciclo de vida del cerebro.
#[derive(Debug, Clone)]
pub enum SystemEvent {
    /// Contador monotónico. No es tiempo de reloj.
    Tick {
        n: u64,
    },

    StageLoaded(String),
    /// Petición de guardar el grafo.
    SnapshotRequested(String),
    /// Petición de cargar e importar un snapshot.
    RestoreRequested(String),
    /// Notificación de importación correcta. No solicita otro restore.
    SnapshotRestored(String),

    /// Arranca un sistema por nombre.
    Start(String),
    /// Para un sistema por nombre.
    Stop(String),

    Shutdown,

    /// Error recuperable reportado como evento.
    Fault(String),
}

/// Entrada normalizada de plataforma.
/// Los drivers solo deben emitir variantes de este enum.
#[derive(Debug, Clone)]
pub enum IngressEvent {
    Key(Key),
    Resize {
        w: u32,
        h: u32,
    },
    /// Línea de texto o comando ya agrupado por el driver.
    Command(String),
    /// Datos crudos de un puerto (red, MQTT, etc.).
    Raw {
        source: String,
        bytes: Vec<u8>,
    },
}

/// Tecla neutral. Independiente de crossterm, web o cualquier backend.
#[derive(Debug, Clone, Copy)]
pub struct Key {
    pub code: KeyCode,
    pub mods: Mods,
}

#[derive(Debug, Clone, Copy)]
pub enum KeyCode {
    Char(char),
    Enter,
    Esc,
    Tab,
    Backspace,
    Up,
    Down,
    Left,
    Right,
    F(u8),
}

/// Modificadores. Bitflags manuales.
#[derive(Debug, Clone, Copy, Default)]
pub struct Mods(pub u8);

impl Mods {
    pub const SHIFT: u8 = 1;
    pub const CTRL: u8 = 2;
    pub const ALT: u8 = 4;
}

/// Evento de dominio.
/// `kind` se resuelve vía `Graph::attr`.
#[derive(Debug, Clone)]
pub struct DomainEvent {
    pub kind: Attr,
    pub subject: Option<ConceptId>,
    pub data: Vec<(Attr, Value)>,
}
