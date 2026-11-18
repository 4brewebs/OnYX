# Contribuir a OnYX

Este archivo es la instrucción de trabajo para personas y para agentes autónomos. Léelo antes de editar. Si una frase de aquí contradice el comportamiento del código o de [README_es.md](README_es.md), gana el código en lo que ya está probado y gana `README_es.md` en lo que es contrato escrito. No inventes una tercera versión.

**Estado: v0.** El contrato público puede romperse. No implementes por tu cuenta el diseño propuesto ni los ítems `TOIMPLEMENT` / `TODO` del README. Si la tarea pide uno de ellos, dilo en el mensaje de commit y actualiza el README en el mismo cambio.

Licencia del código nuevo: `MIT OR Apache-2.0`, igual que los crates. No añadas cabeceras de copyright por archivo.

## Orden de lectura

Lee solo lo que el cambio toca, en este orden:

1. Este archivo.
2. [README_es.md](README_es.md): qué existe, qué está solo diseñado y las reglas del tick. [README.md](README.md) es el resumen en inglés y tiene que decir lo mismo, más corto.
3. El `README.md` del crate que vas a modificar.
4. El módulo y los tests de ese comportamiento. El comentario de `tick` en `onyx-core/src/tick.rs` es normativo.

No recorras el árbol entero para una tarea local. No abras `target/`, `snaps*` ni `*.tr` como si fueran fuente.

## Clasifica el cambio antes de escribir

| Si la tarea… | Haz esto | No hagas esto |
| --- | --- | --- |
| Añade comportamiento dentro de un contrato ya escrito | Código + test del comportamiento observable | Cambiar firmas de `onyx-core` |
| Corrige un fallo que contradice el comentario de `tick`, `Graph` o `DetRng` | Un test que falle con el código viejo y pase con el arreglo | “Mejorar” a la vez el orden de fases |
| Cambia lo que un llamador puede observar en core | Para y describe el cambio de contrato. Sigue solo si la tarea lo pide | Editar el trait y el README en silencio |
| Está marcado como diseño propuesto o `TOIMPLEMENT` | No lo implementes salvo pedido explícito | Tratar el README como un backlog obligatorio |
| Es una herramienta, un driver o la TUI | Efecto externo en el driver; intención durable en el grafo | Llamar a disco, red o `git` desde `OnyxSystem::run` |

Si no puedes clasificarlo, no edites. Deja la pregunta concreta: qué archivo, qué comportamiento observable y qué test lo fijaría.

## Mapa de crates

Rust `1.85.1`, edición `2024`, resolver `3`. Lo fija [rust-toolchain.toml](rust-toolchain.toml).

| Crate | Qué es | Dependencias externas |
| --- | --- | --- |
| `onyx-core` | Contratos, `tick`, `DetRng` | Ninguna |
| `onyx-soul` | Alma por defecto (`WorldGraph`, store, loader) | Ninguna |
| `onyx-soul-agents` | Alma de harness multiagente | Ninguna |
| `onyx-example-app` | REPL sobre `onyx-soul` | Ninguna. Depende solo de su alma |
| `onyx-example-agents` | Recorrido sobre `onyx-soul-agents` | Ninguna. Depende solo de su alma |
| `onyx-scent` | Índice AST y `@TOON v1` | Sí. `clap` solo con `feature = "cli"` |
| `onyx-workspace` | Herramientas, ledger, bridge, modos | Sí |
| `open-onyx` | Binario `openonyx` | Sí |

Los ejemplos no declaran `onyx-core`: lo reexporta su alma.

Un crate que no compile puede pasar a `exclude` en el `Cargo.toml` del workspace, con un comentario. No dejes `cargo test --workspace` rojo.

## Reglas que no se negocian

- El cerebro es síncrono. `Tick { n }` es un contador, no un reloj. No uses `SystemTime`, `Instant` ni números aleatorios de `std` para decidir un estado que luego se replaya.
- Un `OnyxSystem` no hace I/O, no llama a la red y no ejecuta `git`. Eso vive en un `PlatformDriver` o en un adaptador. El sistema escribe la intención en el grafo; el driver vivo la materializa. El replay no debe repetir el efecto.
- El estado que tiene que sobrevivir a un snapshot vive en el grafo. Un `bool` o un contador solo en el sistema se pierde al restaurar.
- Las escrituras al grafo se ven en el mismo tick para los sistemas posteriores. Los eventos nuevos, salvo `SnapshotRequested`, `RestoreRequested`, `Start` y `Stop`, se entregan en el tick siguiente.
- No reordenes las fases de `tick` (entrada, `run`, control, `present`). No vacíes el bus al restaurar ni reencoles el ingress de un tick sin sistemas activos. Esas son reglas escritas, no descuidos.
- No des política de runtime a `Shutdown`, ni introduzcas `SystemTable` o `TickOutcome`, salvo tarea que cambie el contrato y actualice `README_es.md`.
- `WorldGraph` y `AgentGraph` cumplen el mismo contrato observable. No exijas snapshots intercambiables entre almas. Cada alma tiene su formato y rechaza uno incompatible sin borrar datos.
- Dos sistemas no pueden compartir nombre. `tick` no lo comprueba: no añadas otro con un nombre ya usado.
- No cites símbolos, variantes ni campos que no existan en el código. Si documentas una herramienta, el nombre es el de `WorkspaceTools`.

## Peligros ya medidos

Están descritos en la auditoría de `onyx-core` dentro de `README_es.md` y fijados con tests. No los “arregles” con un cambio de comportamiento no pedido. Si la tarea pide cerrar uno, el test nuevo tiene que decir el comportamiento nuevo y el README tiene que dejar de describir el viejo.

- Un `RestoreRequested` no aparta el outbox del mismo tick. Los `ConceptId` de esos eventos son de antes del import.
- El outbox es un `Vec` compartido. Un sistema posterior puede vaciarlo.
- Con todos los sistemas en `Stopped`, el ingress de ese tick se pierde.
- Un pánico dentro de `run` parte el tick: se conservan las escrituras anteriores y se pierde el inbox ya drenado.
- `Start` y `Stop` de un nombre desconocido no emiten `Fault`.
- Escribir `SystemState::Active` a mano no llama a `on_start`.
- `/undo` marca el grafo en `run` y restaura el árbol en `present`. Un fallo de `git` no se reintenta. El texto de memoria no demuestra que el disco haya cambiado.
- `git read-tree` repone el árbol capturado, no un solo archivo.

## Dónde poner el código

- Contrato y ciclo de tick: `onyx-core`. Casi nunca.
- Grafo, snapshot, bus, host y runtime de agentes: el alma que corresponda. No cruces tipos entre `onyx-soul` y `onyx-soul-agents`.
- Herramienta de código, política de workspace, ledger o checkpoint: `onyx-workspace`.
- Comando de sesión (`/new`, `/mode`, `/undo`), bucle de `openonyx`, TUI o Gemini: `open-onyx`. `main.rs` parsea, registra sistemas y elige driver. La lógica de comando va a `command.rs`; el bucle, a `session.rs`; `inspect`, `rewind` y `doctor`, a `ops.rs`.
- Índice y TOON: `onyx-scent`. La feature por defecto no incluye el binario `scent`.

`present` proyecta. `poll` solo debería emitir `Ingress`. El tick no lo impone: no aproveches el hueco para inyectar `Start` o `Restore` desde un driver.

## Tests

Añade o ajusta un test en el mismo cambio que el comportamiento. El test afirma lo observable (estado del grafo, eventos del tick siguiente, bytes de salida, archivo en disco), no un detalle interno que el contrato no nombra.

- Core: `onyx-core/tests/`. Un cambio de `Graph` también pasa la feature `conformance` y, si toca un alma, el `graph_contract.rs` de esa alma.
- Alma de agentes: `onyx-soul-agents/tests/`.
- Workspace: `onyx-workspace/tests/workspace.rs`. Si el cambio toca disco o `git`, el test usa un directorio temporal y lo borra al salir.
- `open-onyx`: tests en `open-onyx/src/main.rs` o el módulo del comportamiento. Un replay que afirme determinismo no puede depender de Gemini ni de la red. `GEMINI_API_KEY` no hace falta para `cargo test`.

No debilites un test para poner el suite en verde. Si el test describe el contrato y tu cambio lo rompe, el cambio está mal o el contrato tiene que actualizarse a propósito.

No dejes `unwrap` ni `expect` nuevos en código de biblioteca salvo en tests. En tests, `expect` con el motivo del fallo sí.

## Comandos

Desde la raíz del repositorio, con la toolchain del `rust-toolchain.toml`. El CI (`.github/workflows/ci.yml`) ejecuta exactamente esto, con `--locked`:

```sh
cargo fmt --all -- --check
cargo check --workspace --all-targets --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo test --workspace --release --locked
cargo doc --workspace --no-deps --locked
```

`RUSTDOCFLAGS=-D warnings` está puesto en el CI. Un aviso de `rustdoc` rompe la build.

Si tocas `onyx-core` o un alma, añade:

```sh
cargo test -p onyx-core --features conformance --test conformance_probes --locked
cargo test -p onyx-core --no-default-features --locked
cargo tree -p onyx-core -p onyx-soul -p onyx-soul-agents
```

Ese `cargo tree` solo puede mostrar crates de este repositorio. Si aparece `crates.io`, la dependencia está en el sitio equivocado.

Antes de terminar, corre al menos `cargo fmt`, `cargo test -p <crate> --locked` del crate tocado y `cargo clippy -p <crate> --all-targets --locked -- -D warnings`. Si el cambio cruza crates, corre el `cargo test --workspace --locked` que el CI va a correr. No declares el trabajo hecho si no lo has ejecutado.

`cargo fmt` reescribe archivos. No formatees crates que no hayas leído en el mismo cambio.

## Dependencias y lockfile

- Añadir una crate a `onyx-core`, `onyx-soul`, `onyx-soul-agents`, `onyx-example-app` o `onyx-example-agents` está prohibido.
- En `onyx-scent`, `onyx-workspace` y `open-onyx`, una dependencia nueva se justifica en el mensaje de commit. Actualiza `Cargo.lock`. El CI usa `--locked` y falla si no está al día.
- No subas versiones “por si acaso”. No copies dependencias del alma hacia la aplicación si el alma ya las reexporta.

## Documentación

- Cambio de comportamiento visible: actualiza el `README.md` del crate y la sección correspondiente de `README_es.md` y `README.md`.
- Edita el párrafo afectado. No reescribas un README entero ni ordenes de nuevo secciones que no tocaste.
- El diseño propuesto se queda en su sección hasta que el código y los tests existan. Entonces pasa a «qué existe» y desaparece de «pendiente».
- Comentarios de contrato, en el estilo del archivo: `onyx-core` y los README largos están en español; varios README de crate están en inglés. No mezcles los dos idiomas en el mismo párrafo. No narres el historial del cambio en un comentario.

## Qué no se commitea

`.gitignore` ya excluye `/target`, `/snapshots/`, `/snaps*/` y `*.tr`. No fuerces su entrada. No commitees claves, `.env`, ni salidas de `openonyx` de una sesión local.

No modifiques transcripciones de evaluación ni directorios de snapshots para “actualizar” un test. Un test que necesite un repo git lo crea en un temporal.

## Commits y ramas

El CI corre en `main`, en ramas `feat/**` y `test/**`, y en todo pull request.

Asunto, en imperativo, con el crate principal:

```text
feat(onyx-workspace): record git checkpoints on fs_edit
fix(open-onyx): keep plan mode across resume
test(onyx-core): pin ingress dropped while systems are stopped
```

El cuerpo dice qué comportamiento observable cambia y qué test lo cubre. Una frase basta si el diff ya es obvio. No metas en el mismo commit un cambio de contrato y una feature que no lo necesita.

## Definición de hecho

El cambio está listo cuando se cumple todo esto:

1. El comportamiento nuevo o corregido tiene un test que falla si se quita el cambio.
2. Los comandos de la sección «Comandos» que aplican al diff pasan en esta máquina.
3. Los README que describen ese comportamiento dicen lo que el código hace ahora.
4. Ningún crate de dependencias cero ha ganado una dependencia.
5. El replay de una sesión grabada no llama a Gemini, no toca el workspace del usuario y no depende del reloj.
6. No queda un `TODO` nuevo que contradiga el código, ni un símbolo documentado que no exista.

Si un punto no se cumple, el resumen final tiene que decir cuál y por qué. No escribas que la suite pasó si no la ejecutaste.
