# OnYX

**Un Lego de cerebros, no una solución cerrada.**

OnYX separa un núcleo de contratos, pequeño y sin dependencias, de las almas que lo cumplen. Si tu realidad es distinta, escribe tu propia alma: el contrato es lo que importa.

> **Estado: v0, en diseño activo.** El contrato público aún no está cerrado y cambiará de forma rompedora. No lo uses donde un fallo tenga consecuencias. Este README separa lo que existe hoy de lo que solo está diseñado.

```
onyx-core   = contratos + tick + RNG determinista            · 0 dependencias
onyx-soul   = implementaciones por defecto de esos contratos · 0 dependencias externas
adaptadores = TUI, WASM, TOML, Redis, Kafka...               · (aún no existen)
```

## Estado al 7 de noviembre de 2026

La última modificación de código de este cierre es `c22927e`.
Esta actualización del README no cambia los contratos ni la implementación.
El workspace contiene solo `onyx-core` y `onyx-soul`, sin dependencias externas.

### Verificación del cierre

Comprobado con Rust 1.85.1 y edición 2024:

- Pasan 63 pruebas y 3 doctests tanto en debug como en release.
- Las 63 pruebas son 29 de contratos de core, 8 controles defectuosos,
  3 de bus, 13 de la suite sobre WorldGraph y 10 privadas del codec.
- Cada control defectuoso pasa el mismo caso con el defecto desactivado y
  provoca el fallo de la suite al activarlo. Cubren reutilización de IDs,
  consultas desordenadas, creación implícita desde `set`, importación parcial,
  fuga aislada de nombres, avance aislado del máximo ID y las dos omisiones
  de limpieza de `destroy`: propiedades `Concept` y relaciones entrantes.
- La API `conformance` no está disponible en la compilación por defecto.
  Al activar la feature sí está disponible. Soul la activa solo para pruebas.
- Clippy conserva un único aviso conocido: `tick` recibe 8 argumentos y
  supera el umbral de 7 de `too_many_arguments`. No se ha silenciado.

Comandos para repetir las comprobaciones desde la raíz:

```sh
cargo fmt --all -- --check
cargo check --workspace --lib
cargo check --workspace --all-targets
cargo clippy --workspace --all-targets
cargo test --workspace
cargo test --workspace --release
cargo test -p onyx-core --features conformance --test conformance_probes
cargo test -p onyx-core --no-default-features
cargo doc --workspace --no-deps --all-features
cargo tree --workspace
```

### Correspondencia entre contrato y pruebas

Los casos están en [la suite de grafo](onyx-core/src/conformance/graph.rs).
WorldGraph los ejecuta en [sus pruebas de conformidad](onyx-soul/tests/graph_contract.rs).

| Garantía del contrato | Casos de la suite |
| --- | --- |
| Nombres exactos, registro idempotente y estabilidad de `Attr` tras importar | `registry`, `import_remaps_attributes` |
| Conceptos vacíos, IDs ascendentes sin reutilización y consultas AND por presencia | `creation_and_queries`, `high_water` |
| `set` no crea conceptos ni guarda destinos inexistentes; `get` devuelve texto poseído | `set_and_missing_references` |
| Orden, unicidad, separación de propiedades y relaciones y eliminación de la terna exacta | `ordering_and_relations` |
| `destroy` limpia referencias sin borrar propietarios ni datos ajenos | `destruction` |
| Conservación de variantes, extremos enteros y bits de flotantes | `values_roundtrip`, `float_bits` |
| Autorref, ciclos e importación repetida | `cyclic_roundtrip` |
| Sustitución exacta, remapeo, máximo de ambos grafos y continuidad del grafo restaurado | `import_remaps_attributes`, `high_water`, `continuation` |
| Rechazo sin cambios de conceptos, nombres ni máximo ID | `rejected_import_is_atomic`, `modified_import_preserves_state_on_error` |

Los casos de errores reciben una sonda privada suministrada por el soul.
No presuponen que cualquier byte modificado sea inválido ni comparan números
`Attr` entre instancias. WorldGraph usa su exportación privada como sonda sobre
la misma instancia. No se impone una serialización canónica a otros souls.

Los [controles defectuosos](onyx-core/tests/conformance_probes.rs) comprueban
la detección de los ocho fallos anteriores. Las [pruebas privadas del codec](onyx-soul/src/graph/snapshot.rs)
cubren truncados, corrupciones, longitudes, duplicados, referencias colgantes,
fallos tardíos y agotamiento de `u64` sin modificar el estado.
Los adaptadores de capacidades se comprueban también en
[graph_api](onyx-core/tests/graph_api.rs) y en los tres doctests de compilación fallida.

### Límites y punto de retomada

- Estas pruebas no demuestran el intercambio con un segundo soul ni la
  recuperación completa del runtime. Los dobles defectuosos no son otro soul.
- No se ha ejecutado el agotamiento real del registro de atributos `u32`.
  La implementación usa una conversión comprobada antes de añadir un nombre.
- `destroy` recorre los conceptos restantes y sus referencias. No hay mediciones
  de rendimiento ni garantías de latencia o de ausencia de asignaciones.
- El formato del snapshot es privado y no incorpora suma de comprobación.
  Una alteración que siga describiendo un grafo válido puede aceptarse.
- Siguen pendientes `SystemTable`, `TickOutcome`, `Shutdown`, `TickState`,
  la unidad completa de recuperación y el gestor de stages. Tampoco hay
  implementaciones de `SnapshotStore` ni de `PlatformDriver`.
- Para retomar, el siguiente diseño propuesto es `SystemTable` y `TickOutcome`
  con la política de `Shutdown`, antes de implementar la recuperación completa.
  El tratamiento de eventos pendientes en la captura aún debe acordarse.
  Cualquier cambio del contrato requiere acuerdo previo y actualización de este README.

## Criterio de diseño

El core fija el **significado observable** (conceptos, atributos, relaciones, orden y reglas de ejecución), no cómo se almacena o indexa. Un alma puede usar SQL, ECS u OCS si respeta ese significado. Core también fija restricciones de capacidades y de ejecución (por ejemplo, qué puede hacer cada hook), pero nunca la representación interna del almacenamiento. **Si no es observable ni una restricción de ejecución, no es obligación del core.**

Las estructuras internas del soul por defecto, su esquema de pertenencia a stages y su formato de snapshot son decisiones del soul, no del core.

## Compromisos del modelo

OnYX es opinionado en el modelo de ejecución y agnóstico en la representación. Los compromisos de ejecución son parte del diseño; las decisiones de tipos y firmas de v0 son revisables.

**Compromisos del modelo.** Cambiarlos exige rediseñar y acordar de nuevo el contrato:

- Cerebro síncrono, ordenado y sin reloj de pared.
- El cerebro solo habla `OnyxEvent`, y la fachada solo proyecta.
- Ciclo de vida de sistemas con estados y fronteras de tick.
- Reproducibilidad basada en semilla explícita y `n` como contador.
- Modelo de conceptos, atributos y relaciones.

**Decisiones revisables de v0.** Pueden cambiar en una versión mayor:

- `Value` como enum cerrado, `ConceptId(u64)` y `Attr(u32)`.
- `drain` devuelve un `Vec`, y las mutaciones del grafo son infalibles.
- Un solo grafo y un solo hilo de cerebro.
- Sin backpressure ni particionado, y sin garantías de tiempo real: v0 no las ofrece, y tampoco se promete que baste con cambiar el soul para obtenerlas.

**Agnóstico en:** almacenamiento e indexación del grafo, formato de snapshot y de store, plataforma, origen de los blueprints y concurrencia dentro de los adaptadores.

## Qué existe hoy

**`onyx-core`**

- `OnyxEvent` (System / Ingress / Domain).
- Puerto `EventBus` (FIFO, sin backpressure).
- Puertos `Graph`, `SnapshotStore` y `PlatformDriver` (core define sus contratos; soul ya aporta `WorldGraph`). `Graph` separa lectura (`GraphView`) y registro de atributos (`AttrRegistry`), con adaptadores `ReadOnly` y `AttrOnly` compatibles con Rust 1.85.1.
- `get` devuelve `Option<Value>`. El contrato documenta consultas, IDs e importación; `exists`, `props` y `links` permiten inspeccionar el estado. `Value` compara flotantes por sus bits, y `OnyxError` implementa `Display` y `Error`.
- `OnyxSystem`, estados `Active` / `Stopped` / `Blocked`, y tipos de blueprint (solo datos).
- `init` idempotente recibe solo `AttrRegistry`. Tras importar, el tick llama a `init` en todos los sistemas: solo los Active con éxito reciben `on_restore`, con `GraphView` y nunca `on_start`. Un Blocked con éxito pasa a Stopped, sin hook. El recorrido sigue el orden de sistemas: `init` y el posible `on_restore` de uno terminan antes del `init` del siguiente.
- `PlatformDriver::present` recibe `GraphView`: expone solo consultas, sin registro de atributos ni métodos mutantes.
- Puertos `BlueprintLoader` y `SystemRegistry` (definidos, el tick aún no los usa).
- `tick` síncrono, ordenado y sin reloj de pared, y `DetRng` (RNG no criptográfico con semilla explícita).

**`onyx-soul`**

- `InMemoryBus`.
- `WorldGraph`: grafo EAV en memoria con conceptos, propiedades, relaciones dirigidas, consultas ordenadas y exportación/importación atómica en un formato privado. Su [diseño y límites](docs/world-graph.md) no forman parte del contrato de otros souls.

**Suite de grafo (`onyx-core`, feature `conformance`)**

- Casos genéricos que reciben fábricas, sin exigir `Default`. La feature está apagada por defecto y no añade dependencias. `onyx-soul` la activa solo en `[dev-dependencies]`.
- API pública de verificación, fuera de la estabilidad de v0: sus casos pueden crecer o endurecerse entre versiones. Ocho grafos defectuosos de prueba demuestran que detecta reutilización de IDs, consultas desordenadas, creación implícita desde `set` e importación no atómica, incluidas fugas que solo alteran nombres o el contador, además de referencias Concept y relaciones entrantes que `destroy` no limpia. Cada caso pasa primero con el defecto desactivado.

Verificación reproducible: `cargo test --workspace` y `cargo test --workspace --release`. `WorldGraph` pasa los casos genéricos de consultas, referencias, destrucción, remapeo de atributos y continuidad tras restaurar, además de pruebas privadas de rechazo atómico de snapshots inválidos. Las mutaciones de bytes no se presuponen inválidas: solo si `import` devuelve `Err` se exige conservación del observable y de una sonda de estado privado suministrada por el soul, que cubre registro completo y contador. La suite no exige igualdad de snapshots entre instancias. Esto no demuestra todavía intercambio con una segunda implementación de grafo ni recuperación completa del runtime. También se prueban RNG, tick, bus y capacidades de los puertos. La idempotencia de `init` se comprueba con una fábrica de sistemas consumidores de prueba, no para cualquier sistema externo. `cargo tree --workspace` permite comprobar que no hay dependencias externas. Para ejecutar solo los controles defectuosos: `cargo test -p onyx-core --features conformance --test conformance_probes`.

## Qué NO existe todavía

El soul ya aporta grafo y bus, pero falta una implementación de `SnapshotStore` y de `PlatformDriver`. La aplicación debe aportar esos puertos y sus sistemas para ejecutar el tick; la recuperación completa y el cambio de stage siguen pendientes.

## Qué está diseñado, pero no implementado

Estas decisiones están acordadas en diseño y **no son código disponible**:

- `SystemTable` en core que impone el ciclo de vida, y `TickOutcome` con la política de `Shutdown`.
- `TickState`: dato plano con el siguiente `n`, la semilla y los estados por nombre único. La tabla expone sus estados; el llamador aporta `n` y semilla. No hay setter libre: la restauración valida nombres y orden de sistemas y aplica el protocolo `init` / `on_restore`, sin poder saltarse transiciones; la compatibilidad del stage y de la configuración es responsabilidad del llamador.
- Unidad de recuperación: grafo, `TickState` y la identificación del stage, la configuración y el orden de sistemas compatibles, **capturados en la misma frontera de tick**. Conservarlos juntos es obligación del llamador: core no impone formato al store ni puede comprobar que correspondan al mismo punto de captura.
- Cambio de stage (`LoadStage`) mediante un gestor que aporta el soul.
- Extensión de la suite genérica a otros puertos y al ciclo de vida; actualmente cubre el grafo.

## Escribe tu propio soul

Un soul aporta una o varias implementaciones de los puertos (grafo, persistencia de snapshots, driver de plataforma y, cuando existan, gestor de stages y registro de sistemas). No tiene que cubrirlos todos: la aplicación compone el resto con otros componentes y adaptadores. Durante `run`, los sistemas reciben `Ctx` (grafo, inbox, outbox, RNG). Los hooks reciben sus puertos de ciclo de vida. Ninguno recibe el bus ni el soul.

Reglas que protegen el intercambio de souls:

1. El estado de aplicación que deba sobrevivir a un snapshot vive en el grafo. Los metadatos de ejecución (siguiente `n`, semilla, estados de sistemas, stage y configuración) viajan aparte, en la unidad de recuperación que conserva el llamador.
2. El tick no depende del tiempo de reloj.
3. La concurrencia vive en los adaptadores; el cerebro es síncrono.

Reproducir una ejecución exige el mismo estado inicial del grafo, las mismas entradas, semilla, siguiente `n`, configuración, orden y estado de sistemas, además de implementaciones con la misma semántica.

Intercambiar souls busca conservar los sistemas y blueprints compatibles con el contrato, no reutilizar snapshots. Cada soul define su formato y rechaza capturas incompatibles sin borrar datos.

`WorldGraph` pasa la suite de grafo disponible. No equivale a demostrar todos los contratos del soul ni el intercambio con otra implementación.

## Qué no es

- No es un framework de UI ni un ECS más.
- No ofrece garantías de tiempo real en v0: ni latencia acotada ni ausencia de asignaciones en el tick. Core fija ejecución síncrona, ordenada y fronteras de control, y deja abierta la implementación dentro de esas reglas; no fija la representación del almacenamiento. Un peor caso acotado exigiría verificar todo el camino de ejecución, no solo cambiar de soul.
- No es una solución cerrada: el soul por defecto es generalista y opinado.

## Contribuir

Juzga el contrato, no el currículum. Si construyes un soul, publícalo: es la mejor validación del diseño.

## Licencia

MIT o Apache-2.0, a tu elección. Ver [LICENSE-MIT](LICENSE-MIT) y [LICENSE-APACHE](LICENSE-APACHE).
