//! API de verificación, disponible solo con la feature `conformance`.
//!
//! Recibe fábricas de implementaciones y comprueba su comportamiento observable.
//! No forma parte de la estabilidad de la API v0: los casos pueden crecer o
//! endurecerse entre versiones. No se incluye en la compilación por defecto.

pub mod graph;
