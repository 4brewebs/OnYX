//! Contratos del estado y de los puertos de persistencia y plataforma.
//! El soul decide cómo almacena y presenta el estado.
//! El grafo expresa conceptos, propiedades y relaciones dirigidas.
//! No aporta inferencia ni validación ontológica automática.

/// Identificador de un concepto: entidad, relación, tipo o evento reificado.
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
pub struct ConceptId(pub u64);

/// Atributo o tipo de relación interned.
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
pub struct Attr(pub u32);

/// Valor común del lenguaje del cerebro.
#[derive(Debug, Clone)]
pub enum Value {
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
    Concept(ConceptId),
}

/// Error unificado de los puertos.
#[derive(Debug)]
pub enum OnyxError {
    Missing,
    Type,
    Io(String),
    Format(String),
}

/// Contrato del estado, independiente de su representación en el soul.
/// Todas las implementaciones deben conservar la semántica de las relaciones.
pub trait Graph {
    /// Resuelve o registra un atributo por nombre.
    /// Se llama al cargar, no en el hot path.
    fn attr(&mut self, name: &str) -> Attr;

    /// Crea un nuevo concepto.
    fn create(&mut self) -> ConceptId;

    /// Elimina el concepto y todas sus propiedades.
    /// Elimina también todas sus relaciones entrantes y salientes.
    /// En los demás conceptos, elimina cada propiedad cuyo valor sea
    /// `Value::Concept(id)`, no el concepto propietario de esa propiedad.
    /// No quedan aristas ni valores que hagan referencia al concepto eliminado.
    /// El soul decide cómo busca esas referencias, sin un coste exigido.
    fn destroy(&mut self, id: ConceptId);

    /// Lee una propiedad del concepto.
    fn get(&self, id: ConceptId, attr: Attr) -> Option<&Value>;

    /// Escribe una propiedad del concepto.
    fn set(&mut self, id: ConceptId, attr: Attr, value: Value);

    /// Añade una relación dirigida entre dos conceptos.
    /// La terna `(from, rel, to)` es única: repetirla no crea duplicados.
    fn relate(&mut self, from: ConceptId, rel: Attr, to: ConceptId);

    /// Elimina exactamente la terna `(from, rel, to)`.
    /// Si la terna no existe, no modifica el grafo.
    fn unrelate(&mut self, from: ConceptId, rel: Attr, to: ConceptId);

    /// Devuelve los destinos de las relaciones de tipo `rel` desde `from`.
    /// El resultado no tiene duplicados y sigue el orden ascendente de `ConceptId.0`.
    fn related(&self, from: ConceptId, rel: Attr) -> Vec<ConceptId>;

    /// Consulta conceptos por presencia de atributos.
    /// El resultado sigue el orden ascendente de `ConceptId.0`.
    fn query(&self, with: &[Attr]) -> Vec<ConceptId>;

    /// Exporta una captura atómica del estado como bytes opacos.
    /// El formato es responsabilidad del soul.
    fn export(&self) -> Vec<u8>;

    /// Importa el estado de forma atómica.
    /// Si falla, conserva el estado anterior sin cambios parciales.
    /// Tras una importación correcta, los `Attr` obtenidos antes pueden invalidarse.
    /// El formato y su compatibilidad son responsabilidad del soul.
    /// Un snapshot incompatible se rechaza con un error, nunca se borra automáticamente.
    fn import(&mut self, bytes: &[u8]) -> Result<(), OnyxError>;
}

/// Persistencia de bytes. No conoce el formato del grafo.
pub trait SnapshotStore {
    fn save(&mut self, key: &str, bytes: &[u8]) -> Result<(), OnyxError>;
    fn load(&self, key: &str) -> Result<Vec<u8>, OnyxError>;
}

/// Puerto de plataforma.
/// Solo vuelca ingress y proyecta el grafo, nunca lo escribe.
pub trait PlatformDriver {
    /// Vuelca eventos de entrada de la plataforma.
    fn poll(&mut self, out: &mut Vec<crate::OnyxEvent>);

    /// La fachada solo proyecta mediante una referencia de lectura al grafo.
    fn present(&mut self, view: &dyn Graph);
}
