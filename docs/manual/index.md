# Manual de Usuario de openOnyx (`openonyx`)

> 🇬🇧 **[Read the full User Manual in English](index-en.md)**

Bienvenido al manual oficial de **`openonyx`**, el harness de agente de programación determinista (**CLI, TUI y Web PWA embebida**) construido sobre el microkernel **OnYX** (`onyx-core` → `onyx-soul-agents` → `onyx-workspace` → `open-onyx`).

A diferencia de otros asistentes de terminal que mezclan estado en memoria volátil, `openonyx` registra cada decisión, lectura, edición y comando en un **grafo EAV determinista** con:

- **Snapshots comprimidos por hitos** (`snapshots.onyxpack`) con verificación FNV-1a y tolerancia a cortes.
- **Time-travel y bifurcación segura** (`/rewind`, `/fork`, `chat --resume --tick`, `rewind --interactive`).
- **Subagentes asíncronos en worktrees Git aislados** (`spawn_subagent` / `/spawn`, fusión 3-way con *Scent-Heated Merge*, `--max-subagents`, y harnesses CLI externos como drivers opcionales `[[external_drivers]]`), y **`spawn_agent(isolated: true)`**: hijos síncronos en worktrees aislados `onyx/sub/agent-*` con reconciliación 3-way que **nunca toca `HEAD` ni la raíz** hasta el merge.
- **Benchmark multi-modelo** (`openonyx bench`): mismos snapshot/tick, worktrees desechables por candidato (rewind + RAII), candidatos LLM vivos `provider:model` o replays offline `replay:<label>=<file.tr>`, `--verify` sandboxed, tabla Markdown de 9 columnas + informe JSON `--out`; la raíz queda 100 % intacta.
- **Verify-on-Edit** (`[verify]` / `[[verify.rules]]` en `.onyx/config.toml`): tras cada `fs_edit`/`fs_write` exitoso, ejecuta las reglas cuya `glob` empareja el archivo editado (y tests `scope:test` impactados por Scent) en sandbox y inyecta `[verify: PASS|FAIL|BLOCKED]` a la observación del tool, sujeto a Workspace Trust y `[permissions].deny`.
- **Objetivo LLM conmutable en caliente** (`[[llm_profiles]]`, `/profile` / `/provider` / `/model` / `/effort`, cabecera `provider/model` viva y selector Web sin credenciales) con **proveedor nativo Anthropic** (Messages API, prompt caching `ephemeral` y *extended thinking* acotado con `--thinking-budget`).
- **Herramientas dinámicas MCP** (`[mcp.servers.<name>]` con transportes `stdio` y `http`): cada tool se expone como `mcp__<server>__<tool>`, siempre con aprobación explícita, `Deny` en `PLAN`/`EXPLORE`, con gate de Workspace Trust, aislamiento de fallos por servidor y auditoría en `openonyx doctor`.
- **TUI modular `F1..F5`** (Chat, Agents, Sprout, Plan & Diff, Timeline) con paleta de comandos `Ctrl+K` y `/view`.
- **Checkpoints Git reversibles (`/undo`)** sin tocar `HEAD` ni tu área de *staging*.
- **Grabación y reproducción 100% offline** (`ONYXTR2`, 0 llamadas de red, 0 escrituras en disco).
- **Seguridad por capas**: **Workspace Trust con `blake3`** (`openonyx trust` / `/trust`), modos `PLAN` / `BUILD` / `EXPLORE`, reglas `.onyx/config.toml` (`deny > ask > allow`), *Scent-Heated Approvals*, sandbox OS con `env_clear()`, protección RO del Control Plane y `ProcessJob` RAII.
- **Interfaz Web / PWA embebida (`openonyx serve`)**: servidor HTTP/1.1 + SSE puro en `std::net` (0 `tokio`), pensado para navegador local, móvil Android/iOS o despliegue detrás de proxy HTTPS, con autenticación constante por `blake3` (Cookie `HttpOnly` / `Bearer`) y `--cors-origin` para clientes externos.
- **Navegación semántica AST Scent 2.0** (`onyx-scent`: `scent_outline` + `scent_impact` + `scent_references`): **7 lenguajes** (Rust, Python, Java, TypeScript, JavaScript, Go, C/C++), tipos `EntityKind::{TypeAlias, Component}`, caché binaria incremental `.onyx/scent.idx` (`ONYXSCI1`, recarga < 20 ms) y CLI `scent outline|impact|references`; más **orquestación local de servicios/contenedores** (`sprout.toml`).

---

## Índice del Manual

| Capítulo | Español | English |
| --- | --- | --- |
| **01. Primeros Pasos / First Steps** | [01. Primeros Pasos](01-primeros-pasos.md) | [01. First Steps](01-first-steps.md) |
| **02. TUI y Flujo Diario / Daily Workflow** | [02. Interfaz TUI y Flujo Diario](02-interfaz-tui-y-flujo-diario.md) | [02. TUI & Daily Workflow](02-tui-and-daily-workflow.md) |
| **03. Modos, Permisos y Sandbox** | [03. Modos, Permisos y Sandbox OS](03-modos-permisos-y-sandbox.md) | [03. Modes, Permissions & OS Sandbox](03-modes-permissions-and-sandbox.md) |
| **04. Time-Travel, Vault y Forking** | [04. Time-Travel, Snapshots y Forking](04-time-travel-snapshots-y-forking.md) | [04. Time-Travel, Snapshots & Forking](04-time-travel-snapshots-and-forking.md) |
| **05. Herramientas, Scent, Sprout y Skills** | [05. Herramientas, Scent, Sprout y Skills](05-herramientas-scent-sprout-y-skills.md) | [05. Tools, Scent, Sprout & Skills](05-tools-scent-sprout-and-skills.md) |
| **06. Referencia CLI y Troubleshooting** | [06. Referencia CLI y Troubleshooting](06-referencia-cli-y-troubleshooting.md) | [06. CLI Reference & Troubleshooting](06-cli-reference-and-troubleshooting.md) |

---

## Rutas de Lectura Recomendadas

- **Si es tu primer día con `openonyx`**: empieza por [01. Primeros Pasos](01-primeros-pasos.md) (o [01. First Steps](01-first-steps.md)) y después lee [02. Interfaz TUI y Flujo Diario](02-interfaz-tui-y-flujo-diario.md).
- **Si trabajas con modelos locales (Ollama, vLLM, LM Studio)**: consulta la sección de proveedores locales en [01. Primeros Pasos](01-primeros-pasos.md), `--effort` en [02. Interfaz TUI](02-interfaz-tui-y-flujo-diario.md) y los reintentos SSE en [06. Referencia CLI y Troubleshooting](06-referencia-cli-y-troubleshooting.md).
- **Si necesitas control estricto de seguridad o entornos corporativos**: lee [03. Modos, Permisos y Sandbox OS](03-modos-permisos-y-sandbox.md).
- **Si quieres experimentar con ramas de conversación y auditoría reproducible**: lee [04. Time-Travel, Snapshots y Forking](04-time-travel-snapshots-y-forking.md).
- **Si usas microservicios locales o contenedores durante el desarrollo**: consulta la sección de Sprout en [05. Herramientas, Scent, Sprout y Skills](05-herramientas-scent-sprout-y-skills.md).

---

## Requisitos Previos

- **Rust `1.85` o superior** (el repositorio fija `1.85.1` en `rust-toolchain.toml` con `edition = "2024"`).
- **Git** instalado en el `PATH` (necesario para checkpoints reversibles `/undo`, `rewind`, `git_status` y `git_diff`).
- **Acceso a un LLM**:
  - Una clave de **Google AI Studio (`GEMINI_API_KEY`)**, o
  - Una clave de **OpenAI / OpenRouter (`OPENAI_API_KEY`)**, o
  - Una clave de **Anthropic (`ANTHROPIC_API_KEY`)** para el proveedor nativo (`--provider anthropic`), o
  - Un servidor local compatible con OpenAI (`/v1/chat/completions`) como **Ollama**, **vLLM** o **LM Studio** (sin API key requerida).
- **Opcional**: `docker`, `nerdctl` o `podman` si vas a usar `--sandbox strict-container` o servicios `kind = "container"` en `sprout.toml`; `bwrap` (Bubblewrap) en Linux para aislamiento de sistema de archivos.

---

## Otros Documentos del Repositorio

- [README del repositorio (EN)](https://github.com/4brewebs/OnYX/blob/main/README.md) · [README en español](https://github.com/4brewebs/OnYX/blob/main/README_es.md)
- [README técnico de `open-onyx`](https://github.com/4brewebs/OnYX/blob/main/open-onyx/README.md)
- [README técnico de `onyx-workspace`](https://github.com/4brewebs/OnYX/blob/main/onyx-workspace/README.md)
- [Guía rápida operativa para agentes (`SKILL.md`)](https://github.com/4brewebs/OnYX/blob/main/.onyx/skills/openonyx/SKILL.md)
- [Historial de cambios (`CHANGELOG.md`)](https://github.com/4brewebs/OnYX/blob/main/CHANGELOG.md)
