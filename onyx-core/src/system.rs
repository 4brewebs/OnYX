//! Sistemas, contexto y blueprints.
//! Los sistemas solo ven el `Ctx`. Nunca el bus ni el soul.

use crate::detrng::DetRng;
use crate::event::OnyxEvent;
use crate::graph::{Graph, OnyxError, Value};

/// Contexto que recibe cada sistema en cada tick.
/// Es la única ventana al mundo.
pub struct Ctx<'a> {
    /// Estado del cerebro. Lectura y escritura.
    pub graph: &'a mut dyn Graph,
    /// Eventos llegados en este tick.
    pub inbox: &'a [OnyxEvent],
    /// Eventos para el siguiente ciclo, salvo las peticiones de guardar, restaurar,
    /// Start y Stop, que se atienden al final de este tick.
    pub outbox: &'a mut Vec<OnyxEvent>,
    /// Generador del tick, compartido en orden de ejecución de sistemas.
    pub rng: &'a mut DetRng,
}

/// Estado de ejecución que conserva el supervisor entre ticks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SystemState {
    /// Puede ejecutar `run`.
    Active,
    /// No ejecuta `run`. Un Start lo activa.
    Stopped,
    /// `init` falló. No ejecuta `run` ni hooks y Start no lo activa.
    /// Solo un `init` correcto lo devuelve a Stopped.
    Blocked,
}

/// Contrato de un sistema.
/// No conoce otros sistemas ni el bus.
pub trait OnyxSystem {
    /// Nombre único. Usado para Start/Stop y logs.
    fn name(&self) -> &str;

    /// Se llama al construir el sistema y tras cada importación correcta.
    /// Resuelve los `Attr` sin reiniciar el estado durable del grafo.
    /// Debe poder repetirse también mientras el sistema está parado o bloqueado.
    /// Si falla, el runtime lo marca Blocked sin llamar hooks.
    /// Un éxito desbloquea a Stopped y necesita un Start explícito para activarse.
    /// Si no estaba bloqueado, un re-init correcto conserva su estado de ejecución.
    fn init(&mut self, graph: &mut dyn Graph) -> Result<(), OnyxError>;

    /// Se llama en cada tick mientras el sistema esté activo.
    fn run(&mut self, ctx: &mut Ctx);

    /// Hooks opcionales de ciclo de vida.
    fn on_start(&mut self, _graph: &mut dyn Graph) {}
    fn on_stop(&mut self, _graph: &mut dyn Graph) {}
}

/// Blueprint = solo datos.
/// No contiene código ni tipos del soul.
#[derive(Debug, Clone)]
pub struct StageBlueprint {
    pub name: String,
    pub systems: Vec<SystemSpec>,
    pub entities: Vec<EntitySpec>,
}

/// Especificación de un sistema a instanciar.
#[derive(Debug, Clone)]
pub struct SystemSpec {
    /// Identificador que el `SystemRegistry` sabe construir.
    pub kind: String,
    /// Parámetros de configuración.
    pub params: Vec<(String, Value)>,
    /// Si sobrevive al cambio de stage.
    pub persistent: bool,
}

/// Especificación de conceptos iniciales.
#[derive(Debug, Clone)]
pub struct EntitySpec {
    pub attrs: Vec<(String, Value)>,
}

/// Carga un blueprint desde algún origen.
/// El core no sabe si viene de Rust, TOML, red, etc.
pub trait BlueprintLoader {
    fn load(&self, name: &str) -> Result<StageBlueprint, OnyxError>;
}

/// Convierte un `SystemSpec` en un sistema concreto.
/// Lo aporta el soul o el usuario.
pub trait SystemRegistry {
    fn build(&self, spec: &SystemSpec) -> Result<Box<dyn OnyxSystem>, OnyxError>;
}
