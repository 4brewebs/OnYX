# OnYX

[![Manual de Usuario](https://img.shields.io/badge/docs-Manual%20de%20Usuario-1f6feb?style=for-the-badge)](https://4brewebs.github.io/OnYX/)
[![Primeros Pasos ES](https://img.shields.io/badge/gu%C3%ADa-Espa%C3%B1ol-e36209?style=for-the-badge)](docs/manual/01-primeros-pasos.md)
[![Quickstart EN](https://img.shields.io/badge/quickstart-English-2da44e?style=for-the-badge)](docs/manual/01-first-steps.md)

**Un Lego de cerebros, no una solución cerrada.**

OnYX separa un núcleo de contratos, pequeño y sin dependencias, de las almas que lo cumplen. Si tu realidad es distinta, escribe tu propia alma: el contrato es lo que importa.

**Estado: v0, en diseño activo.** El contrato público aún no está cerrado y cambiará de forma rompedora. No lo uses donde un fallo tenga consecuencias. Este README separa lo que existe y está comprobado de lo que es diseño propuesto o queda pendiente de implementar. Si buscas la guía práctica completa del harness **`openonyx`** (primeros pasos, TUI, modos/sandbox, time-travel/forking, herramientas, Sprout y solución de problemas), consulta el **[Manual de Usuario de openOnyx](docs/manual/index.md)**. Las reglas para contribuir, también para agentes autónomos, están en [CONTRIBUTING.md](CONTRIBUTING.md).

```
onyx-core           = contratos + tick + RNG determinista             · 0 dependencias
onyx-soul           = implementaciones por defecto de esos contratos  · 0 dependencias externas
onyx-soul-agents    = soul para harnesses multi-agente deterministas  · 0 dependencias externas
onyx-scent          = destilador AST + caché blake3 + @TOON v1        · CLI opcional bajo feature `cli`
onyx-workspace      = herramientas de código y sandbox de workspace   · usa onyx-soul-agents (+ onyx-scent opcional)
open-onyx           = harness CLI/TUI/Web PWA (Gemini/OpenAI + ratatui + SSE) · usa onyx-soul-agents + onyx-workspace
onyx-example-app    = REPL mínimo sobre onyx-soul                     · usa onyx-soul
onyx-example-agents = recorrido completo sobre onyx-soul-agents       · usa onyx-soul-agents
```

## Estado del workspace

Comprobado con Rust `1.85` y edición `2024`. El workspace declara ocho members (`onyx-core`, `onyx-soul`, `onyx-soul-agents`, `onyx-scent`, `onyx-workspace`, `open-onyx`, `onyx-example-app` y `onyx-example-agents`) bajo resolver `3`. Un crate WIP que no compile con la API actual puede pasar a `exclude`, con un comentario, para que `--workspace` siga siendo usable; hoy no hay crates en `exclude`.

Regla de dependencias: `onyx-core`, `onyx-soul`, `onyx-soul-agents` y ambos ejemplos no declaran dependencias externas. Las almas dependen de `onyx-core`. Cada ejemplo depende solo de su soul, que reexporta los tipos de core que la aplicación necesita, así que la app no declara `onyx-core`. Los adaptadores (`onyx-scent`, `onyx-workspace`, `open-onyx`) añaden sus dependencias externas manteniendo el cerebro síncrono y determinista. [`.github/workflows/ci.yml`](.github/workflows/ci.yml) verifica formato, `onyx-workspace --no-default-features`, clippy (`-D warnings`), tests debug/release, `rustdoc` y auditoría de seguridad (`cargo audit` + `cargo deny` vía [`deny.toml`](deny.toml)); usa `cargo tree -p onyx-core -p onyx-soul -p onyx-soul-agents` para comprobar la ausencia de dependencias externas en el núcleo y las almas.

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

El workspace declara ocho members: `onyx-core`, `onyx-soul`, `onyx-soul-agents`, `onyx-scent`, `onyx-workspace`, `open-onyx`, `onyx-example-app` y `onyx-example-agents`. La versión mínima de Rust es `1.85`, la edición es `2024` y el resolver es `3`. Un crate WIP que no compile con la API actual puede pasar a `exclude`, con un comentario, para que `--workspace` siga siendo usable. Hoy no hay crates en `exclude`.

Regla de dependencias: `onyx-core`, `onyx-soul`, `onyx-soul-agents` y ambos ejemplos no declaran dependencias externas. Las almas dependen de `onyx-core`. Cada ejemplo depende solo de su soul, que reexporta los tipos de core que la aplicación necesita, así que la app no declara `onyx-core`. Los crates adaptadores (`onyx-scent`, `onyx-workspace`, `open-onyx`) pueden declarar dependencias externas manteniendo aislado el núcleo determinista. [`.github/workflows/ci.yml`](.github/workflows/ci.yml) verifica formato, clippy, tests y documentación; ejecuta `cargo tree -p onyx-core -p onyx-soul -p onyx-soul-agents` para comprobar la regla de cero dependencias externas.

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

**Auditoría de `onyx-core` (18 de noviembre de 2026)**

`tick` y `DetRng` cumplen el contrato escrito. Esta pasada no cambia traits ni el orden de fases. La suite del crate queda en 31 tests de tick, 6 de `DetRng`, 7 del puerto de grafo, 3 doctests y 8 controles de `conformance`. Los bordes añadidos están en [`onyx-core/tests/tick_contract.rs`](onyx-core/tests/tick_contract.rs) y [`onyx-core/tests/detrng_contract.rs`](onyx-core/tests/detrng_contract.rs).

Cuatro comportamientos son peligros de protocolo, no fallos de implementación:

- Un `RestoreRequested` no aparta el outbox del mismo tick. Los eventos de dominio emitidos alrededor del restore se entregan en el tick siguiente, con los `ConceptId` y los payloads calculados antes de importar, y `SnapshotRestored` queda entre ellos. El consumidor tiene que descartar esas copias.
- El outbox es un `Vec` compartido. Un sistema posterior del mismo tick puede vaciarlo y anular, por ejemplo, un `SnapshotRequested` anterior.
- Si ningún sistema está `Active`, el ingress de ese tick entra en un inbox que nadie lee y no se reencola. Un `Start` posterior no lo recupera. Escribir `SystemState::Active` a mano tampoco dispara `on_start`: ese hook solo corre en la fase de control de `tick`.
- Un pánico dentro de `run` conserva las escrituras de los sistemas anteriores, pierde el inbox ya drenado y el outbox todavía no encolado, y no ejecuta la fase de control ni `present`.

`Start` y `Stop` de un nombre inexistente no emiten `Fault`. `Shutdown` sigue sin política propia, como dice la lista de arriba. Dos sistemas con el mismo nombre reciben ambos el `Start`. `poll` recibe el `EventBus` completo, y `run`, `on_start` y `on_stop` reciben el `Graph` completo: un driver puede drenar el bus y un sistema puede llamar a `import` fuera del protocolo. Un `init` fallido después de un import correcto sigue emitiendo `SnapshotRestored`: el grafo nuevo está cargado y ese sistema queda `Blocked`.

`DetRng` reproduce los vectores fijos, evita el estado cero y mantiene `gen_range` dentro del intervalo. 4096 ticks consecutivos con semillas 0, 1 y 91 no repiten la primera salida. El módulo no promete ausencia de colisiones en todo el espacio de 64 bits.

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

**`onyx-soul-agents`**, **`onyx-workspace`** y **`open-onyx`**

- [`onyx-soul-agents`](onyx-soul-agents/README.md): soul sin dependencias externas especializado en harnesses multi-agente deterministas (`AgentGraph`, `append_agent_memory` con volcado post-`Done` de `result` a `"say"`, `continue_agent` multi-turno con `steps` monótonos, `AgentSnapshotStore`, `RecordingBridge`/`ReplayBridge` con `ONYXTR1`/`ONYXTR2`, `SubagentSystem` con `spawn_agent` acotado por defecto a `32` pasos / `WaitingChildren`, `AgentState::WaitingApproval`, `ApprovalPolicy` y llamadas estructuradas con `call_id`).
- [`onyx-scent`](onyx-scent/README.md): indexador AST multi-archivo (`Rust`, `Python`, `Java`) con `tree-sitter`, caché incremental `blake3`, difusión térmica entera (`0..=10_000` bp), etiquetas `scope:src|test` y `vis:pub|priv`, filtrado `exclude_tests`, herencia de visibilidad en `trait` públicos de Rust, resumen de campos en `struct`, anotaciones Java, resolución preferente intra-archivo y codificador/decodificador `@TOON v1` con paginación determinista.
- [`onyx-workspace`](onyx-workspace/README.md): herramientas de código (`fs_read` con `recall_seen`, deduplicación de rangos solapados `covered_by` y avisos accionables ante artefactos binarios de OnYX; `fs_list`; `fs_write`; `fs_edit` multi-hunk atómico en memoria con *fallback* por líneas y re-indentado relativo; `code_search` v2 con `regex`/`include_glob`/`exclude_glob`/`context_lines`; `glob_files`; `git_status` y `git_diff` de solo lectura sin aprobación; `load_skill` para guías bajo demanda `SKILL.md`/`scent`/`rust`; `plan_update`; `shell_exec` con `env_clear()`, sandbox OS `bwrap` (con fallback automático bajo AppArmor) / `sandbox-exec` / `ProcessJob` RAII, badge honesto `env+job` en Windows, validación OCI estricta (`ContainerCli` + `validate_oci_image_ref`), protección RO del Control Plane, redacción de credenciales e invalidación de cachés; Workspace Trust con `blake3` en `trust.rs`; *Scent-Heated Approvals* en `impact.rs`; perfiles LLM (`llm_profile.rs`: `[[llm_profiles]]` con `api_key_env` — nunca claves literales —, gate de Trust y fusión global < workspace por id, `ws.llm.*` en grafo), subagentes asíncronos en worktrees Git aislados (`worktree.rs`: ramas `onyx/sub/<job_id>`, tools `spawn_subagent`/`subagent_status`/`subagent_merge`/`subagent_discard`, merge 3-way `git merge-tree` sin mover `HEAD` con gobernanza Deny/Ask/Safe del delta, `ws.subagent.*` en grafo) y drivers CLI externos (`external_driver.rs`: `[[external_drivers]]` con `prompt_transport`/`allowed_env`/`network_allowlist`, gate de Trust y aprobación `Ask` por defecto); orquestador local `sprout.toml` con `exec_user` opt-in y `service_status`/`service_start`/`service_stop`/`service_logs`/`service_exec`; más `scent_outline` con caché `seen_outlines` y `scent_impact` bajo `feature = "scent"`), `preview_tool_diff`, `WorkspacePolicy` y `.onyx/config.toml` (`[permissions]` + `[sandbox]` + `[impact]`), modos `WorkspaceMode` (`Plan`/`Build`/`Explore`) con `ws.mode`/`ws.plan.text`/`ws.services.text` en grafo, ledger EAV (`WorkspaceLedgerSystem`), checkpoints Git ligeros (`capture_git_tree_checkpoint` y `restore_git_tree_checkpoint_for_targets` limpiando también archivos untracked recién creados por `fs_write`) y `WorkspaceToolBridge`.
- [`open-onyx`](open-onyx/README.md) ([Manual de Usuario](docs/manual/index.md)): binario modular `openonyx` con parsing CLI en [`main.rs`](open-onyx/src/main.rs) (`--provider gemini|openai`, `--model`, `--effort low|medium|high`, `--api-base` validado con `--allow-custom-auth-host`, `--trust-workspace`, `--sandbox`, `--sandbox-net`, `--auto-approve`, `--ignore-deny`, `--auto-save` / `ONYX_AUTO_SAVE`, `--record`/`--save-transcript`, `--snapshot-every`, `--max-snapshots`, `chat --resume <dir> [--tick <n>] [--fork-dir <dir>] [--truncate-future]`, `serve [--bind] [--api-key] [--cors-origin]`, `trust [add|list|check|revoke]`, `inspect [--list] [--json]`, `rewind [--dry-run] [--interactive|--tui] [--fork-dir <dir>]`, `doctor`), bóveda de sesiones `.onyx/vault/` con `.gitignore` automático y resolución de nombres cortos en [`vault.rs`](open-onyx/src/vault.rs), motor de contexto escalonado Tier 0/1/2 + telemetría en [`context.rs`](open-onyx/src/context.rs), `CommandSpawnerSystem` puro en [`command.rs`](open-onyx/src/command.rs) (`/new`, `/mode`, `/agent`, `/trust`, `/undo`, `/rewind [tick]`, `/fork [name]`, `/compact [focus]`, `/services`, `/service`, continuación multi-turno con guardia anti-duplicado mientras el raíz está activo), almacén append-only comprimido `snapshots.onyxpack` + bifurcación no destructiva (`SharedPackStore`, `fork_pack_until_tick`) en [`pack.rs`](open-onyx/src/pack.rs), superficies lógicas declaradas y bucle `run_session` en [`session.rs`](open-onyx/src/session.rs), utilidades offline en [`ops.rs`](open-onyx/src/ops.rs), adaptadores streaming SSE en [`gemini.rs`](open-onyx/src/gemini.rs) y [`openai.rs`](open-onyx/src/openai.rs) (este último con tolerancia a EOF tras respuesta completa y reintentos con backoff ante errores HTTP/stream transitorios), TUI `ratatui`/`crossterm` en [`tui.rs`](open-onyx/src/tui.rs) con cabecera de 5 filas (tira de pestañas `F1..F5` + título en vivo `provider/model (effort:...)` + telemetría), emblema Braille, badges `[SANDBOX:env+job]`/`[UNTRUSTED]`, vistas `F1..F5` (`F1 Chat` · `F2 Agents` · `F3 Sprout` · `F4 PlanDiff` · `F5 Timeline`) con paleta `Ctrl+K` y `/view`, subagentes asíncronos en worktrees Git listados en la vista `F2 Agents` (`[bg]` + badge `→ <driver>`; `/spawn [--driver <id>]`, `/subagents`, `/merge <job_id>`, `/discard <job_id>`), hot-swap LLM en caliente (`/profile <id>`, `/provider <p>`, `/model <m>`, `/effort <e>`), selector interactivo de agentes (`/agent [id|name]` con popup cuando hay varios; `Ctrl+Tab` al siguiente), streaming `[stream] ...▍`, scroll completo, paneles laterales `Live Plan` + `Services (Sprout)` y previsualización de diff coloreado con banner `# impact:`, y servidor web embebido [`web.rs`](open-onyx/src/web.rs) + [`open-onyx/web/`](open-onyx/web/) (`WebDriver` HTTP/1.1 + SSE sin `tokio`, autenticación `blake3`, CSP estricta y PWA mobile-first Obsidiana/Oro Champán con logotipo real giratorio, modal LLM (`#llm-config-btn` → `dialog#llm-modal`: perfiles/proveedor/modelo/esfuerzo) y selector de driver para subagentes).
- `onyx-example-app` y `onyx-example-agents`: aplicaciones de ejemplo sobre `onyx-soul` y `onyx-soul-agents`.

Verificación reproducible: `cargo test --workspace` y `cargo test --workspace --release`. `WorldGraph` pasa los casos genéricos de consultas, referencias, destrucción, remapeo de atributos y continuidad tras restaurar, además de pruebas privadas de rechazo atómico de snapshots inválidos. Las mutaciones de bytes no se presuponen inválidas: solo si `import` devuelve `Err` se exige conservación del observable y de una sonda de estado privado suministrada por el soul, que cubre registro completo y contador. La suite no exige igualdad de snapshots entre instancias. Esto no demuestra todavía intercambio con una segunda implementación de grafo ni recuperación completa del runtime. También se prueban RNG, tick, bus y capacidades de los puertos. La idempotencia de `init` se comprueba con una fábrica de sistemas consumidores de prueba, no para cualquier sistema externo. `cargo tree --workspace` permite comprobar que no hay dependencias externas. Para ejecutar solo los controles defectuosos: `cargo test -p onyx-core --features conformance --test conformance_probes`.

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

- **TOIMPLEMENT.** Validación estricta de divergencia en TR2 y entrega con idempotencia completa tras restore. Hoy ya existen `ONYXTR2` (comandos + `Reply` v2 con `call_id`), `AgentState::WaitingApproval` y herramientas externas en `onyx-workspace` (`WorkspaceToolBridge`), pero el replay aún no falla explícitamente si divergen los argumentos de una petición respecto a la transcripción ni empaqueta la unidad completa de recuperación junto con el snapshot.
- **TOIMPLEMENT.** Drivers WASM (el driver TUI interactivo ya existe en `open-onyx` mediante `TuiDriver` sobre `ratatui` y `crossterm` junto con `AsyncGeminiDriver`, `--shell-timeout`, `doctor`, `recall_seen`, `supersede`/`ContextBudget`, `scrub_secrets` y delegación a subagentes `spawn_agent`/`WaitingChildren` en `onyx-soul-agents`, manteniendo `onyx-core` y los souls con 0 dependencias).
- **TOIMPLEMENT.** Cargadores de blueprints desde TOML u otros formatos externos. `RustLoader` solo registra blueprints en código.
- **TOIMPLEMENT.** Pruebas de interoperabilidad que monten los mismos sistemas y el mismo blueprint sobre dos souls con la misma semántica y comparen resultados observables.
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
