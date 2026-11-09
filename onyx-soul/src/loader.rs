//! Cargador de blueprints y sistemas desde código Rust.
//! Implementa `BlueprintLoader` y facilita el arranque de aplicaciones.

use std::collections::HashMap;

use onyx_core::graph::OnyxError;
use onyx_core::system::{BlueprintLoader, OnyxSystem, StageBlueprint, SystemRegistry};
// `SystemSpec` no se importa por nombre: `load_systems` recorre
// `blueprint.systems` y entrega cada spec a `SystemRegistry::build`, que ya
// conoce el tipo. Leer campos como `spec.persistent` tampoco lo requiere;
// solo hará falta `use onyx_core::system::SystemSpec;` si se nombra el tipo,
// por ejemplo en la firma de una función auxiliar.

use crate::registry::{InMemorySystemRegistry, SystemFactory};

/// Cargador que resuelve blueprints registrados en código
/// y construye los sistemas de un stage.
#[derive(Default)]
pub struct RustLoader {
    blueprints: HashMap<String, StageBlueprint>,
}

impl RustLoader {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registra un blueprint bajo su nombre.
    pub fn register(&mut self, blueprint: StageBlueprint) {
        self.blueprints.insert(blueprint.name.clone(), blueprint);
    }

    /// Atajo para registrar varios blueprints.
    pub fn register_all(&mut self, blueprints: impl IntoIterator<Item = StageBlueprint>) {
        for bp in blueprints {
            self.register(bp);
        }
    }

    /// Registra fábricas de sistemas y blueprints, y devuelve
    /// los sistemas ya construidos del stage indicado.
    ///
    /// Este es el punto de entrada principal para una aplicación.
    pub fn load_app(
        &mut self,
        registry: &mut InMemorySystemRegistry,
        system_factories: &[(&str, SystemFactory)],
        stages: &[StageBlueprint],
        initial_stage: &str,
    ) -> Result<Vec<Box<dyn OnyxSystem>>, OnyxError> {
        // 1. Registrar fábricas de sistemas
        for (kind, factory) in system_factories {
            registry.register(*kind, *factory);
        }

        // 2. Registrar stages
        for stage in stages {
            self.register(stage.clone());
        }

        // 3. Cargar el stage inicial y construir sus sistemas
        self.load_systems(initial_stage, registry)
    }

    /// Carga un stage ya registrado y construye sus sistemas.
    ///
    /// Solo usa `SystemSpec::kind` y `SystemSpec::params`, que interpreta la
    /// fábrica registrada. Quedan sin implementar dos partes del blueprint:
    ///
    /// - TODO: `SystemSpec::persistent` se ignora. Ningún código de soul
    ///   decide todavía qué sistemas sobreviven a un cambio de stage.
    /// - TODO: `StageBlueprint::entities` se ignora. Los conceptos iniciales
    ///   del stage no se crean en el grafo; la aplicación debe hacerlo.
    ///
    /// Ambos afectan a la semántica del cambio de stage y del snapshot, así
    /// que conviene acordarlos en el contrato antes de implementarlos.
    pub fn load_systems(
        &self,
        stage_name: &str,
        registry: &dyn SystemRegistry,
    ) -> Result<Vec<Box<dyn OnyxSystem>>, OnyxError> {
        let blueprint = self.load(stage_name)?;
        blueprint
            .systems
            .iter()
            .map(|spec| registry.build(spec))
            .collect()
    }
}

impl BlueprintLoader for RustLoader {
    fn load(&self, name: &str) -> Result<StageBlueprint, OnyxError> {
        self.blueprints.get(name).cloned().ok_or(OnyxError::Missing)
    }
}
