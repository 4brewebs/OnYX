# OnYX

**Un Lego de cerebros, no una solución cerrada.**

OnYX separa un núcleo de contratos, pequeño y sin dependencias, de las almas que lo cumplen. Si tu realidad es distinta, escribe tu propia alma: el contrato es lo que importa.

**Estado: v0, en diseño activo.** El contrato público aún no está cerrado y cambiará de forma rompedora. No lo uses donde un fallo tenga consecuencias. Este README separa lo que existe y está comprobado de lo que es diseño propuesto o queda pendiente de implementar.

El resumen en inglés está en [README.md](README.md).

```
onyx-core           = contratos, tick y RNG determinista         · 0 dependencias
onyx-soul           = implementaciones por defecto de esos puertos · 0 dependencias externas
onyx-soul-agents    = alma orientada a agentes, mismos puertos   · 0 dependencias externas
onyx-example-app    = REPL mínimo sobre onyx-soul                · solo depende de esa alma
onyx-example-agents = demo de host, herramienta, snapshot y replay · solo depende de onyx-soul-agents
```

## Criterio de diseño

El core fija el **significado observable** (conceptos, atributos, relaciones, orden y reglas de ejecución), no cómo se almacena o indexa. Un alma puede usar SQL, ECS u OCS si respeta ese significado. Core también fija restricciones de capacidades y de ejecución (por ejemplo, qué puede hacer cada hook), pero nunca la representación interna del almacenamiento. **Si no es observable ni una restricción de ejecución, no es obligación del core.**

Las estructuras internas de cada soul, su esquema de pertenencia a stages y su formato de snapshot son decisiones del soul, no del core.

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

## Workspace

El workspace declara cinco members: `onyx-core`, `onyx-soul`, `onyx-soul-agents`, `onyx-example-app` y `onyx-example-agents`. La versión mínima de Rust es 1.85, la edición es 2024 y el resolver es 3. Un crate WIP que no compile con la API actual puede pasar a `exclude`, con un comentario, para que `--workspace` siga siendo usable. Hoy no hay crates en `exclude`.

Regla de dependencias: `onyx-core`, `onyx-soul`, `onyx-soul-agents` y ambos ejemplos no declaran dependencias externas. Las almas dependen de `onyx-core`. Cada ejemplo depende solo de su soul, que reexporta los tipos de core que la aplicación necesita, así que la app no declara `onyx-core`. Los souls o las aplicaciones futuras pueden añadir dependencias externas. En este árbol no hay flujos de CI que exijan esa regla.

## Qué existe hoy

**`onyx-core`**

- `OnyxEvent` (`System`, `Ingress`, `Domain`). `Tick { n }` es un contador, no un reloj de pared. Existe la variante `Shutdown`, pero `tick` no le aplica una política propia: se encola como cualquier evento que no sea de control.
- Puerto `EventBus` (FIFO, sin backpressure).
- Puertos `Graph`, `SnapshotStore` y `PlatformDriver`. `Graph` separa lectura (`GraphView`) y registro de atributos (`AttrRegistry`), con adaptadores `ReadOnly` y `AttrOnly` compatibles con Rust 1.85, que no tiene upcasting de traits.
- `get` devuelve `Option<Value>`. El contrato documenta consultas, IDs e importación; `exists`, `props` y `links` permiten inspeccionar el estado. `Value` compara flotantes por sus bits, y `OnyxError` implementa `Display` y `Error`.
- `OnyxSystem`, estados `Active` / `Stopped` / `Blocked`, y tipos de blueprint (solo datos).
- `init` es idempotente y recibe solo `AttrRegistry`. Tras importar, el tick llama a `init` en todos los sistemas: solo los `Active` con éxito reciben `on_restore`, con `GraphView` y nunca `on_start`. Un `Blocked` con éxito pasa a `Stopped`, sin hook. El recorrido sigue el orden de sistemas: `init` y el posible `on_restore` de uno terminan antes del `init` del siguiente.
- `PlatformDriver::present` recibe `GraphView`: expone solo consultas, sin registro de atributos ni métodos mutantes.
- Puertos `BlueprintLoader` y `SystemRegistry`. Están definidos. El tick aún no los usa; los souls y las aplicaciones sí, a través del loader y del registro.
- `tick` síncrono y ordenado, y `DetRng` (RNG no criptográfico con semilla explícita). Las escrituras en el grafo se ven en el acto, también para los sistemas posteriores del mismo tick. Los eventos nuevos, salvo las peticiones de control, se entregan en el tick siguiente.

No hay un tipo `Supervisor` en core. El llamador conserva el slice de `SystemState` y lo pasa a `tick`.

**`onyx-soul`**

- `InMemoryBus`.
- `WorldGraph`: grafo EAV en memoria con conceptos, propiedades, relaciones dirigidas, consultas ordenadas, un índice privado de referencias entrantes y un `destroy` quirúrgico. Exporta e importa de forma atómica en un formato privado. Su [diseño y límites](docs/world-graph.md) no forman parte del contrato de otros souls.
- `FileSnapshotStore`: guarda y carga los bytes opacos del grafo en un directorio, solo con la biblioteca estándar.
- `HeadlessDriver` y `PromptDriver`. `PromptDriver` proyecta superficies a texto y acepta comandos o llamadas ya encoladas; no habla por sí mismo con un modelo. `TuiDriver` está en el árbol pero no se compila.
- `InMemorySystemRegistry`, `RustLoader` con el ayudante `load_app`, y `Supervisor`.

**`onyx-soul-agents`**

Segunda alma, desacoplada de `onyx-soul`, para aplicaciones multiagente deterministas. El detalle está en [su README](onyx-soul-agents/README.md).

- `AgentGraph`: mismo contrato observable de `Graph`, formato de snapshot propio, índice de entrantes y `destroy` quirúrgico, como `WorldGraph`.
- `AgentHost`: monta grafo, bus, almacén, sistemas y el bucle de `tick`. Tras un restore, `set_tick` fija el `n` del próximo `step`. `request_restore` no realinea ese contador.
- `RecordingBridge` y `ReplayBridge` (transcripción TR1, magia `ONYXTR1`): el decisor externo se lee en `present` y responde en un `poll` posterior. `wrapping` encadena otro `PlatformDriver`. `starting_at` alinea el cursor tras un restore.
- `HeadlessDriver` y `HeadlessHandle`, de modo que el monitor sigue visible después de `wrapping`.
- `ToolSystem` y `ToolExecutor`: herramientas deterministas que se ejecutan dentro del cerebro.
- `AgentSnapshotStore`, `AgentBus`, `AgentSpawner`, `AgentRuntime`, loader, registro y `Supervisor` propios.
- `AgentDriver` se conserva para pruebas antiguas de petición y respuesta. Las aplicaciones nuevas usan el puente y, si no quieren cablear `tick`, `AgentHost`.

**Ejemplos**

- `onyx-example-app` depende solo de `onyx-soul`. Carga un stage `boot` con `RustLoader::load_app`, ofrece un REPL mínimo (`agree`, `status`, `exit`) y guarda o restaura el grafo con `FileSnapshotStore`. El demo lee stdin en `main` e inyecta comandos a través de `HeadlessDriver`.
- `onyx-example-agents` depende solo de `onyx-soul-agents`. Recorre host, herramienta, puente, snapshot, `set_tick` y replay. El detalle está en [su README](onyx-example-agents/README.md).

**Suite de grafo (`onyx-core`, feature `conformance`)**

- Casos genéricos que reciben fábricas, sin exigir `Default`. La feature está apagada por defecto y no añade dependencias. `onyx-soul` y `onyx-soul-agents` la activan solo en `[dev-dependencies]`.
- API pública de verificación, fuera de la estabilidad de v0: sus casos pueden crecer o endurecerse entre versiones. Ocho grafos defectuosos de prueba demuestran que detecta reutilización de IDs, consultas desordenadas, creación implícita desde `set` e importación no atómica, incluidas fugas que solo alteran nombres o el contador, además de referencias `Concept` y relaciones entrantes que `destroy` no limpia. Cada caso pasa primero con el defecto desactivado.

## Recuperación implementada y comprobada

El snapshot del grafo está implementado en ambos souls. El formato es privado de cada alma, no incorpora suma de comprobación y no define migraciones. Una captura incompatible se rechaza y no se borra. Una alteración que siga describiendo un grafo válido puede aceptarse.

Esa captura no incluye el bus, los eventos en vuelo, el contador `n`, la semilla ni los estados de los sistemas. Los eventos que el outbox devuelve al bus para el tick siguiente no forman parte del snapshot: los conserva el llamador. `AgentHost` lo documenta en `request_restore`.

Tras una importación correcta, `tick` reaplica el protocolo `init` y `on_restore` descrito más arriba. El llamador tiene que aportar de nuevo `n` y la semilla. En `AgentHost`, `ticks` es el `n` del próximo `step`; un restore no lo realinea. Hay que llamar a `set_tick` para que el siguiente `step` entregue ese `n` a `tick` y a `DetRng`. Los puentes TR1 exponen `starting_at` para alinear la transcripción con ese próximo `poll`. Para que continuar equivalga a no interrumpir tras un corte con outbox no vacío, el llamador tiene que reinyectar esos eventos.

Dentro de una configuración compatible, las pruebas de recuperación de ambos souls cortan cada tick de 0 a 28 de una corrida de 30 ticks y reinyectan el outbox pendiente (`Reinsert` en `onyx-soul`, `AgentHost::push` en `onyx-soul-agents`), fijan el siguiente `n` (`set_tick` en `AgentHost`) y continúan. Comparan el estado observable del grafo (y, en el alma de agentes, la huella `fingerprint`) y los eventos del contrato, no los bytes del snapshot. Sin esa reinyección, la equivalencia solo se da en un corte tranquilo, cuando el outbox está vacío. Ese caso lo comprueba un control dedicado en cada suite de recuperación, y el binario `onyx-example-agents`, que captura después de `say`, sin eventos de herramienta en vuelo. Eso no es todavía la unidad completa de recuperación descrita más abajo como diseño propuesto.

## Correspondencia entre contrato y pruebas

Los casos están en [la suite de grafo](onyx-core/src/conformance/graph.rs). `WorldGraph` los ejecuta en [sus pruebas de conformidad](onyx-soul/tests/graph_contract.rs). `AgentGraph` hace lo mismo en `onyx-soul-agents/tests/graph_contract.rs`.

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

Los casos de errores reciben una sonda privada suministrada por el soul. No presuponen que cualquier byte modificado sea inválido ni comparan números `Attr` entre instancias. Cada grafo usa su exportación privada como sonda sobre la misma instancia. No se impone una serialización canónica a otros souls.

Los [controles defectuosos](onyx-core/tests/conformance_probes.rs) comprueban la detección de esos ocho fallos. Las pruebas privadas del codec en `onyx-soul/src/graph/snapshot.rs` y `onyx-soul-agents/src/graph/snapshot.rs` cubren truncados, corrupciones, longitudes, duplicados, referencias colgantes, fallos tardíos y agotamiento de `u64` sin modificar el estado. Los adaptadores de capacidades se comprueban también en [graph_api](onyx-core/tests/graph_api.rs) y en los tres doctests de compilación fallida.

`WorldGraph` y `AgentGraph` pasan por separado la suite genérica. Eso no demuestra el intercambio de un mismo sistema y blueprint entre dos souls, ni exige igualdad de snapshots entre instancias.

Otras pruebas cubren RNG, tick, bus, índice de entrantes, prioridad por identificador, el puente TR1, el host y la recuperación descrita más arriba. La idempotencia de `init` se comprueba con fábricas de sistemas de prueba, no para cualquier sistema externo.

Comandos para repetir las comprobaciones desde la raíz:

```sh
cargo fmt --all -- --check
cargo check --workspace --all-targets
cargo clippy --workspace --all-targets
cargo test --workspace
cargo test --workspace --release
cargo test -p onyx-core --features conformance --test conformance_probes
cargo test -p onyx-core --no-default-features
cargo doc --workspace --no-deps --all-features
cargo tree --workspace
cargo run -p onyx-example-app
cargo run -p onyx-example-agents
```

`cargo test --workspace` llega a ejecutar los ocho controles defectuosos porque las almas activan `conformance` en sus dependencias de desarrollo y Cargo unifica features. Para ejecutarlos aislados sobre `onyx-core`, usa el comando con `--features conformance`. `cargo tree --workspace` muestra solo crates de este repositorio.

## Diseño propuesto, aún no implementado

Estas decisiones están acordadas en diseño y **no son código disponible**. Se conservan aquí para no perder la especificación.

- `SystemTable` en core, que impone el ciclo de vida, y `TickOutcome` con la política de `Shutdown`.
- `TickState`: dato plano con el siguiente `n`, la semilla y los estados por nombre único. La tabla expone sus estados; el llamador aporta `n` y semilla. No hay un setter que salte el protocolo: la restauración valida nombres y orden de sistemas y aplica `init` y `on_restore`, sin poder saltarse transiciones. La compatibilidad del stage y de la configuración es responsabilidad del llamador.
- Unidad de recuperación: grafo, `TickState` y la identificación del stage, la configuración y el orden de sistemas compatibles, capturados en la misma frontera de tick. Conservarlos juntos es obligación del llamador: core no impone formato al store ni puede comprobar que correspondan al mismo punto de captura.
- Cambio de stage (`LoadStage`) mediante un gestor que aporta el soul.
- Extensión de la suite genérica a otros puertos y al ciclo de vida. Hoy cubre el grafo.

El siguiente diseño propuesto, cuando se retome el contrato de core, es `SystemTable` y `TickOutcome` con la política de `Shutdown`, antes de implementar esa unidad completa de recuperación. Cualquier cambio del contrato requiere acuerdo previo y actualización de este README.

## Pendiente de implementar

Marcado **TOIMPLEMENT** o **TODO** cuando ya hay un comentario en el código. No son capacidades actuales.

- **TOIMPLEMENT.** TR2 y herramientas externas. Hoy un `ToolExecutor` solo puede tocar el grafo y `DetRng`. Una herramienta sin ejecutor se deja para que otro sistema o un driver responda con `ev.tool_result` o un `Reply` de observación; si nadie responde, el agente acaba por timeout. La transcripción TR1 graba las `Reply` del decisor, no un protocolo completo de resultados externos. El diseño propuesto para TR2 graba la petición completa de la herramienta (nombre, argumentos e identificador) y el replay falla de forma clara si esa grabación diverge. Tras un restore, la entrega es al menos una vez. El identificador de la llamada se persiste en el grafo como clave de idempotencia; el receptor o el host evitan duplicar el efecto asociado a esa clave. No se promete una entrega exactamente una vez.
- **TOIMPLEMENT.** Drivers TUI y WASM. El REPL de ejemplo no sustituye a un driver de terminal.
- **TOIMPLEMENT.** Cargadores de blueprints desde TOML u otros formatos externos. `RustLoader` solo registra blueprints en código.
- **TOIMPLEMENT.** Pruebas de interoperabilidad que monten los mismos sistemas y el mismo blueprint sobre dos souls con la misma semántica y comparen resultados observables.
- **TOIMPLEMENT.** Flujos de CI en el repositorio. Hoy no hay directorio `.github` ni toolchain file que los defina.
- **TODO** en `RustLoader` de ambos souls: `SystemSpec::persistent` y `StageBlueprint::entities` se ignoran. Afectan a la semántica del cambio de stage y del snapshot, y conviene acordarlos en el contrato antes de implementarlos.
- No se ha ejecutado el agotamiento real del registro de atributos `u32`. La implementación usa una conversión comprobada antes de añadir un nombre.

## Escribe tu propio soul

Un soul aporta una o varias implementaciones de los puertos (grafo, persistencia de snapshots, driver de plataforma y, cuando existan, gestor de stages y registro de sistemas). No tiene que cubrirlos todos: la aplicación compone el resto con otros componentes y adaptadores. Durante `run`, los sistemas reciben `Ctx` (grafo, inbox, outbox, RNG). Los hooks reciben sus puertos de ciclo de vida. Ninguno recibe el bus ni el soul.

Reglas que protegen el intercambio de souls:

1. El estado de aplicación que deba sobrevivir a un snapshot vive en el grafo. Los metadatos de ejecución (siguiente `n`, semilla, estados de sistemas, stage y configuración) viajan aparte. En el diseño propuesto, viajan en la unidad de recuperación que conserva el llamador. Hoy el llamador ya tiene que conservarlos, aunque esa unidad aún no exista como tipo de core.
2. El tick no depende del tiempo de reloj.
3. La concurrencia vive en los adaptadores; el cerebro es síncrono.

Reproducir una ejecución exige el mismo estado inicial del grafo, las mismas entradas, semilla, siguiente `n`, configuración, orden y estado de sistemas, además de implementaciones con la misma semántica.

Intercambiar souls busca conservar los sistemas y blueprints compatibles con el contrato, no reutilizar snapshots. Cada soul define su formato y rechaza capturas incompatibles sin borrar datos. Al cambiar una estructura incompatible o de soul, se admite empezar con almacenamiento nuevo. Cualquier vaciado queda en una operación explícita, no en el rechazo de la captura.

`WorldGraph` y `AgentGraph` pasan la suite de grafo disponible. No equivale a demostrar todos los contratos del soul ni el intercambio con otra implementación.

## Qué no es

- No es un framework de UI ni un ECS más.
- No ofrece garantías de tiempo real en v0: ni latencia acotada ni ausencia de asignaciones en el tick. Core fija ejecución síncrona, ordenada y fronteras de control, y deja abierta la implementación dentro de esas reglas; no fija la representación del almacenamiento. Un peor caso acotado exigiría verificar todo el camino de ejecución, no solo cambiar de soul.
- No es una solución cerrada: cada soul es generalista u opinado a su manera, y no es el único posible.

## Arranque rápido

```bash
cargo run -p onyx-example-app
```

```text
[CORE] Arranque limpio.
========================================
  OnYX Engine REPL (v0.1.0)
  Type 'agree', 'status', or 'exit'
========================================
onyx> agree
[BOOT] System online. Awaiting 'agree'...
[BOOT] NDA accepted. Clearance = 1
onyx> status
[BOOT] ConceptId(1), Clearance: Some(Int(1))
onyx> exit
Apagado seguro...
[CORE] Guardado en ./snapshots/repl_savegame.snap
Engine stopped after 2 ticks.
```

En el primer tick se crea el jugador y se atiende `agree`, por eso aparecen juntas las dos líneas `[BOOT]`.

```bash
cargo run -p onyx-example-agents
```

Si restore, `set_tick` y el replay coinciden con la grabación, el proceso termina con `fingerprints_match=true` y `ok`.

## Contribuir

Juzga el contrato, no el currículum. Si construyes un soul, publícalo: es la mejor validación del diseño.

## Licencia

MIT o Apache-2.0, a tu elección. Ver [LICENSE-MIT](LICENSE-MIT) y [LICENSE-APACHE](LICENSE-APACHE).
