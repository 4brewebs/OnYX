//! Registro de sistemas.
//! Convierte un `SystemSpec` (datos) en un `Box<dyn OnyxSystem>`.

use std::collections::HashMap;

use onyx_core::graph::OnyxError;
use onyx_core::system::{OnyxSystem, SystemRegistry, SystemSpec};

/// Constructor de un sistema a partir de su spec.
pub type SystemFactory = fn(&SystemSpec) -> Result<Box<dyn OnyxSystem>, OnyxError>;

/// Registro concreto.
/// El soul (o la app) registra las fábricas que sabe construir.
#[derive(Default)]
pub struct InMemorySystemRegistry {
    factories: HashMap<String, SystemFactory>,
}

impl InMemorySystemRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registra una fábrica bajo un `kind`.
    pub fn register(&mut self, kind: impl Into<String>, factory: SystemFactory) {
        self.factories.insert(kind.into(), factory);
    }
}

impl SystemRegistry for InMemorySystemRegistry {
    fn build(&self, spec: &SystemSpec) -> Result<Box<dyn OnyxSystem>, OnyxError> {
        let factory = self.factories.get(&spec.kind).ok_or(OnyxError::Missing)?;

        factory(spec)
    }
}
