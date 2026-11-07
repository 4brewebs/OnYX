//! Contratos del estado y de los puertos de persistencia y plataforma.
//! El soul decide cómo almacena e indexa conceptos, propiedades y relaciones.
//! El core no aporta inferencia ni validación ontológica automática.

use std::fmt;

/// Identificador de un concepto dentro de un grafo.
/// Una copia externa debe descartarse y obtenerse de nuevo tras un import.
/// El grafo no distingue una copia antigua de un id restaurado con igual número.
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
pub struct ConceptId(pub u64);

/// Identificador internado de un atributo o tipo de relación.
/// Su numeración pertenece al registro del grafo, no es portable entre grafos.
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
pub struct Attr(u32);

impl Attr {
    /// Construye un identificador para una implementación de `AttrRegistry`.
    /// No registra un nombre ni comprueba que el número pertenezca al registro.
    /// Las aplicaciones deben obtener sus atributos mediante `AttrRegistry::attr`.
    pub const fn from_raw(raw: u32) -> Self {
        Self(raw)
    }

    /// Devuelve el número local, sin convertirlo en un identificador portable.
    pub const fn raw(self) -> u32 {
        self.0
    }
}

/// Valor común del lenguaje del cerebro.
/// La igualdad distingue variantes y compara `Float` mediante `f64::to_bits`:
/// un NaN es igual solo con el mismo patrón, y `+0.0` es distinto de `-0.0`.
#[derive(Debug, Clone)]
pub enum Value {
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
    Concept(ConceptId),
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        match self {
            Self::Bool(a) => matches!(other, Self::Bool(b) if a == b),
            Self::Int(a) => matches!(other, Self::Int(b) if a == b),
            Self::Float(a) => matches!(other, Self::Float(b) if a.to_bits() == b.to_bits()),
            Self::Text(a) => matches!(other, Self::Text(b) if a == b),
            Self::Concept(a) => matches!(other, Self::Concept(b) if a == b),
        }
    }
}

impl Eq for Value {}

/// Error unificado de los puertos.
#[derive(Debug)]
pub enum OnyxError {
    Missing,
    Type,
    Io(String),
    Format(String),
}

impl fmt::Display for OnyxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing => f.write_str("missing value"),
            Self::Type => f.write_str("type mismatch"),
            Self::Io(message) => write!(f, "I/O error: {message}"),
            Self::Format(message) => write!(f, "invalid format: {message}"),
        }
    }
}

impl std::error::Error for OnyxError {}

/// Registro de nombres, sin acceso a los conceptos del grafo.
/// Un mismo nombre conserva su `Attr` durante la vida de la instancia,
/// también después de importar. Nombres distintos tienen ids distintos.
/// Registrar puede asignar memoria; no modifica conceptos ni relaciones.
///
/// Este puerto no permite leer ni modificar el mundo:
/// ```compile_fail
/// use onyx_core::{AttrRegistry, ConceptId};
/// fn read_world(attrs: &mut dyn AttrRegistry) {
///     let name = attrs.attr("name");
///     attrs.get(ConceptId(1), name);
/// }
/// ```
/// ```compile_fail
/// use onyx_core::AttrRegistry;
/// fn create_world(attrs: &mut dyn AttrRegistry) {
///     attrs.create();
/// }
/// ```
pub trait AttrRegistry {
    /// Resuelve o registra un nombre. El registro es idempotente.
    fn attr(&mut self, name: &str) -> Attr;
}

/// Lectura del grafo sin registro de atributos ni operaciones mutantes.
/// Las consultas observan las escrituras completadas antes de la llamada.
/// No se exige un almacenamiento, índice ni coste concretos.
///
/// ```compile_fail
/// use onyx_core::{ConceptId, GraphView};
/// fn destroy_world(view: &mut dyn GraphView) {
///     view.destroy(ConceptId(1));
/// }
/// ```
pub trait GraphView {
    /// Indica si el concepto está vivo, aunque no tenga propiedades.
    fn exists(&self, id: ConceptId) -> bool;

    /// Devuelve una propiedad poseída, sin exigir una caché residente al soul.
    /// Devuelve `None` si el concepto no existe o no tiene ese atributo.
    /// `exists` permite distinguir ambos casos.
    fn get(&self, id: ConceptId, attr: Attr) -> Option<Value>;

    /// Devuelve los destinos de la relación desde `from`, sin duplicados,
    /// en orden ascendente de `ConceptId.0`. Si `from` no existe, devuelve vacío.
    fn related(&self, from: ConceptId, rel: Attr) -> Vec<ConceptId>;

    /// Consulta conceptos que tienen todos los atributos indicados (AND).
    /// Repetir un filtro no cambia el resultado. No consulta tipos de relación.
    /// `query(&[])` devuelve todos los conceptos vivos, incluidos los vacíos.
    /// El resultado no tiene duplicados y sigue el orden ascendente de ids.
    fn query(&self, with: &[Attr]) -> Vec<ConceptId>;

    /// Enumera las propiedades por nombre de atributo, en orden lexicográfico.
    /// Cada nombre aparece una vez. Un concepto inexistente devuelve vacío.
    /// Los nombres, no los números `Attr`, permiten comparar distintos registros.
    fn props(&self, id: ConceptId) -> Vec<(String, Value)>;

    /// Enumera las relaciones salientes, sin ternas duplicadas.
    /// Ordena por nombre de relación y, a igualdad de nombre, por id de destino.
    /// Un concepto inexistente devuelve vacío. No incluye propiedades `Concept`.
    fn links(&self, id: ConceptId) -> Vec<(String, ConceptId)>;
}

/// Contrato del estado, independiente de su representación en el soul.
/// Las mutaciones sin `Result` no ofrecen errores recuperables. El contrato
/// no promete latencia acotada, ausencia de asignaciones ni recuperación de
/// pánicos. Un almacenamiento falible debe resolver esa política en su soul;
/// devolver `()` no permite comunicar un fallo recuperable ni fingir éxito.
pub trait Graph: GraphView + AttrRegistry {
    /// Crea un concepto vacío con id mayor que cualquier id emitido previamente
    /// en esta ejecución o conservado por una importación. Incluye ids destruidos.
    /// No se permite reciclar ids ni hacer wraparound al agotar su representación.
    fn create(&mut self) -> ConceptId;

    /// Elimina el concepto, sus propiedades y sus relaciones entrantes y salientes.
    /// En los demás conceptos elimina propiedades iguales a `Value::Concept(id)`,
    /// no sus propietarios. No quedan referencias al concepto eliminado.
    /// Si no existe, no modifica nada. Su id no se reutiliza en esta ejecución.
    fn destroy(&mut self, id: ConceptId);

    /// Escribe o sustituye una propiedad. Si `id` no existe, no modifica nada.
    /// Si `value` es `Value::Concept(destino)` y el destino no existe,
    /// no modifica nada, incluida la propiedad anterior.
    /// Nunca crea un concepto implícitamente ni guarda referencias colgantes.
    fn set(&mut self, id: ConceptId, attr: Attr, value: Value);

    /// Añade una relación dirigida. Repetir `(from, rel, to)` es idempotente.
    /// Si cualquiera de los extremos no existe, no modifica nada.
    fn relate(&mut self, from: ConceptId, rel: Attr, to: ConceptId);

    /// Elimina exactamente `(from, rel, to)`.
    /// Si falta algún extremo o la terna no existe, no modifica nada.
    fn unrelate(&mut self, from: ConceptId, rel: Attr, to: ConceptId);

    /// Exporta una captura atómica como bytes opacos, en el formato del soul.
    /// Conserva conceptos, propiedades, relaciones, nombres y el máximo id
    /// emitido, incluidos los conceptos destruidos. No impone números `Attr`
    /// al receptor ni un formato compartido entre souls, proyectos o versiones.
    /// El llamador conserva los metadatos de ejecución de la misma frontera;
    /// esta captura por sí sola no garantiza la reproducción de una ejecución.
    fn export(&self) -> Vec<u8>;

    /// Importa atómicamente el estado de una captura compatible.
    /// Si falla, el grafo completo queda intacto, incluido el registro de nombres
    /// y el máximo id emitido. Una captura incompatible se rechaza, nunca se borra.
    ///
    /// Los nombres ya registrados conservan el `Attr` del receptor; los datos
    /// entrantes se remapean. Los nombres nuevos reciben ids de atributo frescos.
    /// Tras el éxito existen exactamente los conceptos de la captura.
    /// El consumidor descarta y vuelve a obtener sus referencias externas:
    /// no se puede detectar una copia antigua cuyo número coincide con uno válido.
    ///
    /// El siguiente id creado es mayor que el máximo de la captura y el máximo
    /// ya emitido por el proceso, incluidos los destruidos. Una caída que pierde
    /// el máximo posterior a la captura puede permitir reemitir aquellos ids.
    ///
    /// Solo importa el grafo: el bus, los hooks y los metadatos de ejecución
    /// pertenecen al protocolo de recuperación, no a esta operación del puerto.
    fn import(&mut self, bytes: &[u8]) -> Result<(), OnyxError>;
}

/// Vista explícita para evitar upcasting de traits, no disponible en Rust 1.85.
/// Reenvía las consultas al mismo grafo, sin copiar ni capturar su estado.
pub struct ReadOnly<'a>(pub &'a dyn Graph);

impl GraphView for ReadOnly<'_> {
    fn exists(&self, id: ConceptId) -> bool {
        self.0.exists(id)
    }

    fn get(&self, id: ConceptId, attr: Attr) -> Option<Value> {
        self.0.get(id, attr)
    }

    fn related(&self, from: ConceptId, rel: Attr) -> Vec<ConceptId> {
        self.0.related(from, rel)
    }

    fn query(&self, with: &[Attr]) -> Vec<ConceptId> {
        self.0.query(with)
    }

    fn props(&self, id: ConceptId) -> Vec<(String, Value)> {
        self.0.props(id)
    }

    fn links(&self, id: ConceptId) -> Vec<(String, ConceptId)> {
        self.0.links(id)
    }
}

/// Acceso explícito al registro sin exponer el grafo al receptor del puerto.
/// Se pasa como `&mut dyn AttrRegistry`, no como el adaptador concreto.
pub struct AttrOnly<'a>(pub &'a mut dyn Graph);

impl AttrRegistry for AttrOnly<'_> {
    fn attr(&mut self, name: &str) -> Attr {
        self.0.attr(name)
    }
}

/// Persistencia de bytes. No conoce el formato del grafo.
pub trait SnapshotStore {
    fn save(&mut self, key: &str, bytes: &[u8]) -> Result<(), OnyxError>;
    fn load(&self, key: &str) -> Result<Vec<u8>, OnyxError>;
}


