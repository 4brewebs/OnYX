# OnYX

**Un Lego de cerebros, no una solución cerrada.**

OnYX separa un núcleo de contratos, pequeño y sin dependencias, de las almas que lo cumplen. Si tu realidad es distinta, escribe tu propia alma: el contrato es lo que importa.

> **Estado: v0, en diseño activo.** El contrato público aún no está cerrado y cambiará de forma rompedora. No lo uses donde un fallo tenga consecuencias. Este README separa lo que existe hoy de lo que solo está diseñado.

```
onyx-core   = contratos + tick + RNG determinista            · 0 dependencias
onyx-soul   = implementaciones por defecto de esos contratos · 0 dependencias externas
adaptadores = TUI, WASM, TOML, Redis, Kafka...               · (aún no existen)
```

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
- Puertos `Graph`, `SnapshotStore` y `PlatformDriver` (solo contratos: no hay implementación). `Graph` separa lectura (`GraphView`) y registro de atributos (`AttrRegistry`), con adaptadores `ReadOnly` y `AttrOnly` compatibles con Rust 1.85.1.
- `get` devuelve `Option<Value>`. El contrato documenta consultas, IDs e importación; `exists`, `props` y `links` permiten inspeccionar el estado. `Value` compara flotantes por sus bits, y `OnyxError` implementa `Display` y `Error`.
- `OnyxSystem`, estados `Active` / `Stopped` / `Blocked`, y tipos de blueprint (solo datos).
- `init` idempotente recibe solo `AttrRegistry`. Tras importar, el tick llama a `init` en todos los sistemas: solo los Active con éxito reciben `on_restore`, con `GraphView` y nunca `on_start`. Un Blocked con éxito pasa a Stopped, sin hook.
- `PlatformDriver::present` recibe `GraphView`: expone solo consultas, sin registro de atributos ni métodos mutantes.
- Puertos `BlueprintLoader` y `SystemRegistry` (definidos, el tick aún no los usa).
- `tick` síncrono, ordenado y sin reloj de pared, y `DetRng` (RNG no criptográfico con semilla explícita).

**`onyx-soul`**

- `InMemoryBus`.

Verificación reproducible: `cargo test --workspace` y `cargo test --workspace --release`. Las pruebas cubren RNG, tick, bus, valores, adaptadores del puerto y selección de hooks tras restaurar. La idempotencia se comprueba con una fábrica de sistemas consumidores de prueba, no para cualquier sistema externo. No demuestran todavía la conformidad de un grafo, incluido el no-op de `set` con propietario o destino `Concept` inexistente. `cargo tree --workspace` permite comprobar que no hay dependencias externas.

## Qué NO existe todavía

Con la versión actual no se puede arrancar un cerebro completo: falta una implementación de `Graph`, de `SnapshotStore` y de `PlatformDriver`. Hoy solo hay un runtime de tick utilizable si la aplicación aporta esos puertos.

## Qué está diseñado, pero no implementado

Estas decisiones están acordadas en diseño y **no son código disponible**:

- `SystemTable` en core que impone el ciclo de vida, y `TickOutcome` con la política de `Shutdown`.
- `TickState`: dato plano con el siguiente `n`, la semilla y los estados por nombre único. La tabla expone sus estados; el llamador aporta `n` y semilla. No hay setter libre: la restauración valida nombres y orden de sistemas y aplica el protocolo `init` / `on_restore`, sin poder saltarse transiciones; la compatibilidad del stage y de la configuración es responsabilidad del llamador.
- Unidad de recuperación: grafo, `TickState` y la identificación del stage, la configuración y el orden de sistemas compatibles, **capturados en la misma frontera de tick**. Conservarlos juntos es obligación del llamador: core no impone formato al store ni puede comprobar que correspondan al mismo punto de captura.
- Cambio de stage (`LoadStage`) mediante un gestor que aporta el soul.
- Suite de conformidad genérica para que cualquier soul demuestre que cumple el contrato.
- `WorldGraph` (soul por defecto del grafo), que nacerá pasando esa suite.

## Escribe tu propio soul

Un soul aporta una o varias implementaciones de los puertos (grafo, persistencia de snapshots, driver de plataforma y, cuando existan, gestor de stages y registro de sistemas). No tiene que cubrirlos todos: la aplicación compone el resto con otros componentes y adaptadores. Durante `run`, los sistemas reciben `Ctx` (grafo, inbox, outbox, RNG). Los hooks reciben sus puertos de ciclo de vida. Ninguno recibe el bus ni el soul.

Reglas que protegen el intercambio de souls:

1. El estado de aplicación que deba sobrevivir a un snapshot vive en el grafo. Los metadatos de ejecución (siguiente `n`, semilla, estados de sistemas, stage y configuración) viajan aparte, en la unidad de recuperación que conserva el llamador.
2. El tick no depende del tiempo de reloj.
3. La concurrencia vive en los adaptadores; el cerebro es síncrono.

Reproducir una ejecución exige las mismas entradas, semilla, siguiente `n`, configuración, orden y estado de sistemas, además de implementaciones compatibles.

Hasta que exista la suite de conformidad, "cumple el contrato" no está demostrado para ningún soul.

## Qué no es

- No es un framework de UI ni un ECS más.
- No ofrece garantías de tiempo real en v0: ni latencia acotada ni ausencia de asignaciones en el tick. Core fija ejecución síncrona, ordenada y fronteras de control, y deja abierta la implementación dentro de esas reglas; no fija la representación del almacenamiento. Un peor caso acotado exigiría verificar todo el camino de ejecución, no solo cambiar de soul.
- No es una solución cerrada: el soul por defecto es generalista y opinado.

## Contribuir

Juzga el contrato, no el currículum. Si construyes un soul, publícalo: es la mejor validación del diseño.

## Licencia

MIT o Apache-2.0, a tu elección. Ver [LICENSE-MIT](LICENSE-MIT) y [LICENSE-APACHE](LICENSE-APACHE).
