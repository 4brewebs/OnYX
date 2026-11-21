# Manual de Usuario de openOnyx (`openonyx`)

Bienvenido al manual oficial de **`openonyx`**, el harness de agente de programación determinista (CLI y TUI) construido sobre el microkernel **OnYX** (`onyx-core` → `onyx-soul-agents` → `onyx-workspace` → `open-onyx`).

A diferencia de otros asistentes de terminal que mezclan estado en memoria volátil, `openonyx` registra cada decisión, lectura, edición y comando en un **grafo EAV determinista** con:

- **Snapshots comprimidos por hitos** (`snapshots.onyxpack`) con verificación FNV-1a y tolerancia a cortes.
- **Time-travel y bifurcación segura** (`/rewind`, `/fork`, `chat --resume --tick`, `rewind --interactive`).
- **Checkpoints Git reversibles (`/undo`)** sin tocar `HEAD` ni tu área de *staging*.
- **Grabación y reproducción 100% offline** (`ONYXTR2`, 0 llamadas de red, 0 escrituras en disco).
- **Seguridad por capas**: modos `PLAN` / `BUILD` / `EXPLORE`, reglas `.onyx/config.toml` (`deny > ask > allow`), sandbox OS con `env_clear()`, protección RO del Control Plane y `ProcessJob` RAII.
- **Navegación semántica AST** (`onyx-scent`: `scent_outline` + `scent_impact`) y **orquestación local de servicios/contenedores** (`sprout.toml`).

---

## Índice del Manual

| Capítulo | Contenido | Idioma |
| --- | --- | --- |
| **[01. Primeros Pasos](01-primeros-pasos.md)** | Instalación, compilación, configuración con Gemini / OpenAI / Ollama / vLLM, primera sesión TUI paso a paso hasta `Settled`, aprobar diffs y `/undo`. | Español |
| **[01. First Steps (Quickstart)](01-first-steps.md)** | Step-by-step quickstart in English: build, configure Gemini / OpenAI / local LLMs, first interactive session, diff approvals, `Tab` Plan/Build, and `/undo`. | English |
| **[02. Interfaz TUI y Flujo Diario](02-interfaz-tui-y-flujo-diario.md)** | Anatomía de la TUI (cabecera de 3 filas, emblema Braille, modelo/`--effort`, telemetría en vivo, `Memory & Trace`, paneles `Live Plan` y `Services`), contexto escalonado, `/compact [focus]`, `/new`, y trabajo multi-agente (`spawn_agent`, `F2` Agent Selector, `/agent`, `Ctrl+Tab`). | Español |
| **[03. Modos, Permisos y Sandbox OS](03-modos-permisos-y-sandbox.md)** | Modos `PLAN`, `BUILD` y `EXPLORE`; configuración declarativa `.onyx/config.toml` (`[permissions]` y `[sandbox]`), `deny > ask > allow`, `--ignore-deny`, `--auto-approve`, protección RO del Control Plane, `env_clear()`, `ProcessJob` RAII y drivers Linux (`bwrap` + fallback AppArmor) / macOS / Windows. | Español |
| **[04. Time-Travel, Snapshots y Forking](04-time-travel-snapshots-y-forking.md)** | Cómo funciona `snapshots.onyxpack`, etiquetas de hito automáticas, `/undo` (incluyendo limpieza de archivos nuevos `fs_write`), `/rewind [tick]` y `/fork [name]` en vivo, `chat --resume --tick`, `rewind --interactive`, `replay` offline (`ONYXTR2`), `inspect` y `doctor`. | Español |
| **[05. Herramientas, Scent, Sprout y Skills](05-herramientas-scent-sprout-y-skills.md)** | Catálogo completo de las 19 herramientas del agente, navegación AST (`scent_outline`, caché `seen_outlines`, `@TOON v1`, `scent_impact`), orquestación de procesos y contenedores con `sprout.toml`, reglas `ONYX.md`/`AGENTS.md`, creación de skills propias y automatización headless (`openonyx run`). | Español |
| **[06. Referencia CLI y Solución de Problemas](06-referencia-cli-y-troubleshooting.md)** | Referencia completa de subcomandos, flags, variables de entorno, resiliencia SSE / reintentos OpenAI/vLLM, lista de chequeo con `openonyx doctor` y soluciones a los problemas más habituales en Windows, Linux y macOS. | Español |

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
  - Un servidor local compatible con OpenAI (`/v1/chat/completions`) como **Ollama**, **vLLM** o **LM Studio** (sin API key requerida).
- **Opcional**: `docker`, `nerdctl` o `podman` si vas a usar `--sandbox strict-container` o servicios `kind = "container"` en `sprout.toml`; `bwrap` (Bubblewrap) en Linux para aislamiento de sistema de archivos.

---

## Otros Documentos del Repositorio

- [README del repositorio (EN)](https://github.com/4brewebs/OnYX/blob/main/README.md) · [README en español](https://github.com/4brewebs/OnYX/blob/main/README_es.md)
- [README técnico de `open-onyx`](https://github.com/4brewebs/OnYX/blob/main/open-onyx/README.md)
- [README técnico de `onyx-workspace`](https://github.com/4brewebs/OnYX/blob/main/onyx-workspace/README.md)
- [Guía rápida operativa para agentes (`SKILL.md`)](https://github.com/4brewebs/OnYX/blob/main/.onyx/skills/openonyx/SKILL.md)
- [Historial de cambios (`CHANGELOG.md`)](https://github.com/4brewebs/OnYX/blob/main/CHANGELOG.md)
