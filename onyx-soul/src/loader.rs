//! Cargador de blueprints desde código Rust.
//! Implementa `BlueprintLoader`.

use onyx_core::graph::OnyxError;
use onyx_core::system::{BlueprintLoader, StageBlueprint};

/// Cargador que resuelve blueprints registrados en código.
#[derive(Default)]
pub struct RustLoader {
    blueprints: std::collections::HashMap<String, StageBlueprint>,
}

impl RustLoader {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registra un blueprint bajo un nombre.
    pub fn register(&mut self, blueprint: StageBlueprint) {
        self.blueprints.insert(blueprint.name.clone(), blueprint);
    }

    /// Atajo para registrar varios.
    pub fn register_all(&mut self, blueprints: impl IntoIterator<Item = StageBlueprint>) {
        for bp in blueprints {
            self.register(bp);
        }
    }
}

impl BlueprintLoader for RustLoader {
    fn load(&self, name: &str) -> Result<StageBlueprint, OnyxError> {
        self.blueprints
            .get(name)
            .cloned()
            .ok_or(OnyxError::Missing)
    }
}