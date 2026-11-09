//! Configuración del framework, pensada para viajar en `SystemSpec::params`.
//!
//! El `Loader` (blueprint) es el sitio donde la app ajusta el framework: los
//! mismos parámetros se pasan a todos los sistemas del stage y cada uno lee
//! los que le importan. Las claves desconocidas se ignoran, de modo que la app
//! puede añadir las suyas en los mismos `params`.

use onyx_core::{Attr, AttrRegistry, OnyxError, Value};

/// Prefijo por defecto de todos los nombres que el framework registra.
pub const DEFAULT_PREFIX: &str = "agent";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentConfig {
    /// Prefijo de los nombres de atributo, relación y evento (`<prefix>.goal`...).
    /// Permite convivir con otro modelo de agentes o con otra instancia.
    pub vocab_prefix: String,
    /// Tope de agentes vivos. Los `Done`/`Failed` no cuentan.
    pub max_agents: usize,
    /// Presupuesto de decisiones si el `AgentSpec` no fija uno.
    pub default_max_steps: i64,
    /// Ticks de espera si el `AgentSpec` no fija un plazo.
    pub default_timeout_ticks: i64,
    /// Entradas de memoria que se conservan; 0 = sin poda.
    /// Lo aplica [`crate::DefaultMemory`] al construir el runtime.
    pub memory_keep_last: usize,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            vocab_prefix: DEFAULT_PREFIX.to_string(),
            max_agents: 64,
            default_max_steps: 16,
            default_timeout_ticks: 8,
            memory_keep_last: 0,
        }
    }
}

fn bad(key: &str, expected: &str) -> OnyxError {
    OnyxError::Format(format!("param `{key}` must be {expected}"))
}

fn positive(key: &str, v: &Value) -> Result<i64, OnyxError> {
    match v {
        Value::Int(i) if *i >= 1 => Ok(*i),
        _ => Err(bad(key, "an integer >= 1")),
    }
}

fn saturate(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

/// Convierte sin truncar: en plataformas de 32 bits un `i64` grande no cabe en `usize`.
fn count(key: &str, i: i64, expected: &str) -> Result<usize, OnyxError> {
    usize::try_from(i).map_err(|_| bad(key, expected))
}

impl AgentConfig {
    /// Lee los parámetros conocidos de un `SystemSpec`. Un valor inválido es
    /// un error (el sistema no se construye); una clave desconocida se ignora.
    /// Si una clave se repite, gana la última aparición.
    ///
    /// Claves: `vocab_prefix` (Text), `max_agents`, `default_max_steps`,
    /// `default_timeout_ticks` (Int >= 1) y `memory_keep_last` (Int >= 0).
    pub fn from_params(params: &[(String, Value)]) -> Result<Self, OnyxError> {
        let mut c = Self::default();
        for (key, value) in params {
            match key.as_str() {
                "vocab_prefix" => match value {
                    Value::Text(s) if !s.is_empty() && !s.contains(char::is_whitespace) => {
                        c.vocab_prefix = s.clone()
                    }
                    _ => return Err(bad(key, "non-empty text without whitespace")),
                },
                "max_agents" => {
                    c.max_agents = count(key, positive(key, value)?, "an integer >= 1")?
                }
                "default_max_steps" => c.default_max_steps = positive(key, value)?,
                "default_timeout_ticks" => c.default_timeout_ticks = positive(key, value)?,
                "memory_keep_last" => match value {
                    Value::Int(i) if *i >= 0 => {
                        c.memory_keep_last = count(key, *i, "an integer >= 0")?
                    }
                    _ => return Err(bad(key, "an integer >= 0")),
                },
                _ => {}
            }
        }
        Ok(c)
    }

    /// Inversa de `from_params`: para escribir el blueprint desde código.
    /// Un contador mayor que `i64::MAX` se satura a `i64::MAX`, que sigue siendo
    /// un valor válido al volver a leerlo.
    pub fn to_params(&self) -> Vec<(String, Value)> {
        vec![
            (
                "vocab_prefix".into(),
                Value::Text(self.vocab_prefix.clone()),
            ),
            ("max_agents".into(), Value::Int(saturate(self.max_agents))),
            (
                "default_max_steps".into(),
                Value::Int(self.default_max_steps),
            ),
            (
                "default_timeout_ticks".into(),
                Value::Int(self.default_timeout_ticks),
            ),
            (
                "memory_keep_last".into(),
                Value::Int(saturate(self.memory_keep_last)),
            ),
        ]
    }

    /// Atributo propio de la app bajo el mismo prefijo: `<prefix>.<name>`.
    /// Es el punto de extensión del vocabulario de la app.
    pub fn attr(&self, attrs: &mut dyn AttrRegistry, name: &str) -> Attr {
        attrs.attr(&format!("{}.{}", self.vocab_prefix, name))
    }
}
