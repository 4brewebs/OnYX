---
name: openonyx
description: Guía operativa completa para usar, configurar, depurar y automatizar openOnyx (CLI, TUI, proveedores LLM, esfuerzo de razonamiento, replay, snapshots, rewind, fork, doctor, modos, sandbox, Sprout y herramientas).
---

# openOnyx (`openonyx`) — Guía Operativa

`openonyx` es el harness CLI y TUI de OnYX (`onyx-soul-agents` + `onyx-workspace`). Conecta un proveedor LLM por HTTP/SSE (`gemini` o cualquier endpoint compatible con OpenAI `/v1/chat/completions`, con tolerancia a cortes SSE y reintentos transitorios) con un runtime determinista de grafo EAV (`AgentHost`), registro de transcripciones (`ONYXTR2`), snapshots comprimidos por hitos (`snapshots.onyxpack`), checkpoints Git reversibles (`/undo`), time-travel seguro con bifurcación (`/rewind`, `/fork`, `chat --resume --tick`, `rewind --interactive`), sandbox OS y orquestación local de servicios (`sprout.toml`).

> **Manual de usuario completo:** consulta [`docs/manual/index.md`](../../../docs/manual/index.md) para tutoriales paso a paso, arquitectura de la TUI, configuración de permisos/sandbox, time-travel, catálogo completo de las 19 herramientas y resolución de problemas.

---

## 1. Invocación y Proveedores LLM

Desde el workspace de OnYX o con el binario instalado:

```sh
openonyx <subcomando> [opciones]
# o directamente desde el repositorio:
cargo run -p open-onyx -- <subcomando> [opciones]
```

### Variables de entorno

| Variable | Uso | Default / Notas |
| --- | --- | --- |
| `ONYX_PROVIDER` | Proveedor por defecto (`gemini` \| `openai`) | `gemini` (sobrescrito por `--provider`) |
| `GEMINI_API_KEY` / `ONYX_API_KEY` | API key para `--provider gemini` | Requerida en `chat`, `run` y `rewind` vivo |
| `ONYX_MODEL` / `ONYX_GEMINI_MODEL` | Modelo para Gemini | `gemini-4-hydrogen` (sobrescrito por `--model`) |
| `GEMINI_API_BASE` | URL base personalizada de Gemini | Sobrescrito por `--api-base` |
| `OPENAI_API_KEY` / `ONYX_API_KEY` | API key para `--provider openai` | Opcional si usas `--api-base` / `OPENAI_API_BASE` local |
| `ONYX_MODEL` / `OPENAI_MODEL` | Modelo para backend OpenAI | `gpt-4o` (sobrescrito por `--model`) |
| `OPENAI_API_BASE` / `OPENAI_BASE_URL` | Endpoint compatible OpenAI | `https://api.openai.com/v1` |
| `ONYX_EFFORT` | Esfuerzo de razonamiento (`low` \| `medium` \| `high`) | Sobrescrito por `--effort`; fallbacks: `GEMINI_THINKING_LEVEL` (Gemini) y `OPENAI_REASONING_EFFORT` (OpenAI) |

> **Nota:** `replay`, `inspect`, `rewind --dry-run`, `rewind --transcript` y `doctor` funcionan sin API key ni llamadas de red.

### Ejemplos de arranque rápido

```sh
# TUI interactiva con Gemini + esfuerzo alto + snapshots + transcripción
openonyx chat --effort high --snapshot-dir snaps_chat --record chat.tr

# TUI con OpenAI / OpenRouter
openonyx chat --provider openai --model gpt-4o --effort medium --snapshot-dir snaps_openai

# TUI con modelo local en Ollama / vLLM / LM Studio (sin API key requerida)
openonyx chat --provider openai --api-base http://localhost:11434/v1 --model qwen2.5-coder:14b --effort low

# Turno único headless
openonyx run "resume la arquitectura de crates" --record run.tr --snapshot-dir snaps_run
```

---

## 2. Subcomandos y Opciones CLI

### Subcomandos

1. **`chat`**: Lanza la TUI (`ratatui` + `crossterm`) con streaming SSE en vivo, emblema Braille de estado, cabecera compacta con modelo/`--effort`/modo/sandbox, vista previa de diffs antes de aprobar, selector interactivo de agentes (`F2` / `/agent`), alternancia `[PLAN]` / `[BUILD]`, time-travel en sesión (`/rewind`, `/fork`, `/undo`) y paneles laterales `Live Plan` + `Services (Sprout)`.
2. **`run <goal>`**: Ejecuta un único objetivo en modo headless y muestra el resultado final + resumen `[openOnyx telemetry]`.
3. **`replay <transcript>`**: Reproduce offline una transcripción `ONYXTR2` (0 llamadas HTTP, 0 mutaciones en disco). Acepta `--snapshot-dir`, `--snapshot-every`, `--max-snapshots`.
4. **`inspect <snapshot_dir>`**: Inspecciona hitos en `snapshots.onyxpack` (o archivos legacy `tick-*.bin`).
   - `--list` / `-l`: lista todos los ticks con etiqueta de hito, tamaños, checksum FNV-1a y checkpoint Git.
   - `--tick <n>`: inspecciona un tick específico.
   - `--json`: salida estructurada en JSON.
5. **`rewind <snapshot_dir> <tick>`**: Retrocede al tick `<n>` (o al hito previo más cercano `<= N`), audita efectos secundarios huérfanos y restaura el checkpoint de árbol Git en el workspace (limpiando también archivos untracked recién creados por `fs_write`).
   - `--dry-run`: previsualiza efectos huérfanos y plan de rollback sin tocar archivos.
   - `--transcript <file>`: reanuda determinísticamente desde una transcripción grabada.
   - `--interactive` / `--tui`: abre directamente la TUI en el tick restaurado creando un fork hermano seguro (`--fork-dir <dir>` opcional, o `--truncate-future` si se desea truncar in-place).
6. **`doctor`**: Verifica salud del worktree Git, artefactos residuales, integridad de `snapshots.onyxpack` (checksums FNV-1a, payloads `ONYXAG`, colas truncadas tras crash), snapshots legacy, cabeceras de transcripción, objetos `tree_sha` del ledger, driver de sandbox OS disponible y configuración/puertos de `sprout.toml`.

### Flags transversales principales

- `-w, --workspace <dir>`: directorio del proyecto objetivo (default: CWD).
- `--provider <gemini|openai>` / `--model <name>` / `--effort <low|medium|high>` / `--api-base <url>`: selección del backend LLM y nivel de razonamiento.
- `--auto-approve`: omite confirmación interactiva para herramientas mutantes (`fs_write`, `fs_edit`, `shell_exec`, `service_*`; las reglas `deny` de `.onyx/config.toml` siguen bloqueando y el sandbox se eleva automáticamente a `workspace-write` + `offline` salvo `--sandbox off`).
- `--ignore-deny`: desactiva explícitamente las reglas `[permissions].deny` de `.onyx/config.toml`.
- `--sandbox <off|workspace-write|strict-container>` y `--sandbox-net <offline|localhost|unrestricted>`: perfil de sandbox OS (`env_clear()`, protección RO del Control Plane `.onyx/config.toml`/`.git/hooks/**`/`.git/config`/`.env*`, `ProcessJob` RAII, y fallback automático de `bwrap` en Linux con AppArmor restrictivo) y política de red para `shell_exec`. El estado se muestra en `[SANDBOX:...]` y se audita en `openonyx doctor`.
- `--max-steps <n>`: límite de pasos del agente raíz por turno (default: `128` en `chat`, `64` en `run`; los subagentes lanzados con `spawn_agent` heredan un techo por defecto de `32` pasos salvo override explícito `max_steps` en los argumentos).
- `--shell-timeout <secs>`: timeout en segundos por comando `shell_exec` (default de `WorkspacePolicy`: `30s`).
- `--record <file>` / `--save-transcript <file>`: guarda transcripción determinista `ONYXTR2`.
- `--snapshot-dir <dir>`: guarda hitos comprimidos en `<dir>/snapshots.onyxpack`.
- `--snapshot-every <n>`: fuerza snapshots periódicos adicionales cada `<n>` ticks.
- `--max-snapshots <n>`: retención opt-in por ventana deslizante (conserva `init` + últimos `n - 1` hitos).
- `-r, --resume <dir>` *(solo `chat`)*: reanuda desde el último hito guardado en `<dir>`, o desde un tick específico combinándolo con `--tick <n>` (crea fork hermano seguro por defecto; personalizable con `--fork-dir <dir>` o destructivo opt-in con `--truncate-future`).

---

## 3. Controles TUI, Modos y Comandos Slash (`chat`)

### Atajos de teclado

- **`Enter`**: envía objetivo, prompt de seguimiento (reactiva al mismo agente raíz conservando toda su `rel_memory` vía `continue_agent`) o comando slash. Mientras el agente raíz está ocupado (`thinking`, `acting`, `waiting_children` o `waiting_approval`), la TUI impide crear un segundo agente raíz accidental.
- **`Tab`**: alterna en caliente entre `[BUILD]` (edición permitida) y `[PLAN]` (solo lectura).
- **`F2`**: abre/cierra el popup interactivo **Agent Selector** para inspeccionar y enfocar al agente raíz o cualquiera de sus subagentes (`↑`/`↓`, `Enter`, `Esc`).
- **`Ctrl+Tab`**: atajo rápido para rotar el agente enfocado (alias dependiente del terminal para `F2` / `/agent`).
- **`y` / `n`**: aprueba o rechaza una herramienta mutante pendiente (`WaitingApproval`), indicando el origen `[agent #id name]` y mostrando diff unificado en color para `fs_write` y `fs_edit` multi-hunk.
- **`Up` / `Down` / `PgUp` / `PgDn` / `Home` / `End` / `Ctrl+U` / `Ctrl+D`**: desplaza el historial `Memory & Trace` incluso durante un prompt de aprobación.
- **`Esc` / `Ctrl+C`**: cierra el popup abierto o sale limpiamente guardando snapshot final (`exit`) y transcripción si están configurados.
- **Cabecera compacta, emblema Braille y paneles laterales**: la cabecera de 3 filas muestra el emblema animado (`⠋..⠏`, throttled a 90 ms), `[PLAN]`/`[BUILD]`, `[SANDBOX:...]`, modelo y `--effort`, además de telemetría en vivo (`ctx: <1%`/`XX% · in / cache / out / think · ~$cost` o `local`). Cuando `plan_update` y/o `sprout.toml` están activos y el terminal tiene `>= 100` columnas, se muestran a la derecha `Live Plan` y `Services (Sprout)`.

### Comandos Slash

- **`/agent [id|name]`** (alias `/agents`): abre el selector de agentes o enfoca directamente a un agente por ID numérico o nombre (etiqueta de hito `cmd:/agent`).
- **`/mode [plan|build|explore]`**: cambia al modo indicado o alterna `PLAN` ↔ `BUILD` si se omite el argumento.
  - `BUILD`: lectura y modificación controlada (`fs_edit` exige lectura previa del archivo).
  - `PLAN`: solo lectura y análisis; bloquea mutaciones.
  - `EXPLORE`: exploración de solo lectura.
- **`/undo`**: revierte el último checkpoint Git reversible (`fs_write` / `fs_edit`) creado en la sesión sin alterar `HEAD` ni el staging index del usuario, eliminando además archivos untracked recién creados por `fs_write` sin tocar otros archivos untracked del usuario.
- **`/rewind [tick]`**: sin argumentos lista los hitos disponibles en `snapshots.onyxpack`; con `<tick>` retrocede en caliente al hito `<= tick` (bloqueado de forma segura si hay agentes activos o peticiones LLM en vuelo), bifurcando el pack a un directorio hermano sin mutar el historial original y restaurando el árbol Git + archivos untracked objetivo (`cmd:/rewind`).
- **`/fork [name]`**: crea una rama de snapshots hasta el tick actual en un directorio hermano (`<dir>.fork-<name>` o `<dir>.fork-t<tick>`) y cambia el store activo a la nueva rama sin tocar el pack original (`cmd:/fork`).
- **`/compact [focus]`**: compacta determinísticamente el historial de conversación del agente en un `[session_summary]` (rescatando `files_found` y `focus_matches` relevantes a `[focus]` de turnos previos), crea el hito `cmd:/compact` en `snapshots.onyxpack` y conserva intacta la traza en el grafo para `rewind` y `replay`.
- **`/services`** y **`/service <start|stop|logs> <id|all> [tail]`**: consulta y controla procesos locales y contenedores Docker/Rancher definidos en `sprout.toml` (reflejados en el panel derecho `Services (Sprout)` y auditados en `openonyx doctor`).
- **`/new`** o **`/new <goal>`**: fuerza que el siguiente prompt cree un agente raíz limpio (o lo lanza de inmediato con `<goal>`, enfocando automáticamente la vista en el nuevo agente raíz).
- **Permisos granulares (`.onyx/config.toml`)**: `~/.onyx/config.toml` y `<workspace>/.onyx/config.toml` admiten `[permissions]` (`deny`, `allow`, `ask`, precedencia `deny > ask > allow`; `PLAN`/`EXPLORE` bloquean siempre mutaciones y `deny` aplica incluso con `--auto-approve` salvo `--ignore-deny`) y `[sandbox]`.

---

## 4. Protocolo de Trabajo Recomendado para Agentes

Cuando operes dentro de `openonyx`, sigue esta disciplina para minimizar tokens, errores y aprobaciones innecesarias:

1. **Mapeo AST antes de leer archivos completos**:
   - Usa `glob_files` para localizar rutas por patrón (`crates/**/*.rs`, `**/*SKILL.md`) sin pedir aprobación.
   - Usa `code_search` v2 con `regex`, `case_insensitive`, `include_glob`, `exclude_glob` y `context_lines` (`0..=5`).
   - Usa `scent_outline` para obtener firmas `@TOON v1` y rangos de líneas (`seen_outlines` cachea llamadas repetidas en sesión y se invalida automáticamente tras `fs_edit`/`fs_write`/`shell_exec`/`/undo`).
   - Usa `scent_impact` sobre símbolos compartidos antes de refactorizar.
   - Usa `fs_read` acotado con `start_line` / `end_line` (límite de lectura: `64 KiB`; `fs_read` deduplica lecturas repetidas y rangos solapados ya cubiertos con `covered_by:start-end`, y rechaza con aviso accionable artefactos binarios de OnYX `ONYXTR1`/`ONYXTR2`/`ONYXPK1`/`ONYXAG`).
2. **Planificación visible**:
   - En tareas multi-paso, mantén actualizado `plan_update` (`pending`, `in_progress`, `completed`) sin llamarlo varias veces seguidas sin herramientas de workspace de por medio.
3. **Ediciones limpias y seguras**:
   - En modo `BUILD`, **siempre** lee el archivo con `fs_read` antes de `fs_edit` / `fs_write`.
   - Prefiere `fs_edit` con `edits: [{ old_text, new_text }]` para cambios atómicos multi-hunk en un mismo archivo.
   - Usa `git_status` y `git_diff` (sin aprobación) en lugar de `shell_exec` para inspeccionar estado Git.
   - Reserva `shell_exec` para compilar, testear o ejecutar comandos mutantes (salida limitada a `32 KiB`, filtrada con `scrub_secrets`; evita crear scripts temporales ad-hoc cuando exista herramienta nativa).
4. **Subagentes acotados**:
   - Usa `spawn_agent` para delegar subtareas independientes con un `goal` claro y acotado (techo por defecto: `32` pasos por subagente; inspeccionables en vivo con `F2` / `/agent`).

---

## 5. Instrucciones de Proyecto y Skills Personalizadas

Al iniciar (`chat`, `run` y `rewind` vivo), `openonyx` carga instrucciones Markdown en este orden (hasta `16 KiB` en total, filtradas con `scrub_secrets`):

1. `~/.onyx/AGENTS.md` y `~/.onyx/ONYX.md`
2. `<workspace>/AGENTS.md` y `<workspace>/ONYX.md`

Además, inyecta una pista de sistema operativo anfitrión (Windows PowerShell vs Unix/POSIX) y el catálogo compacto de skills (`discover_skills_summary`), buscando por orden de prioridad:

1. `<workspace>/.onyx/skills/<nombre>/SKILL.md` *(ruta recomendada por proyecto)* o `<nombre>.md`
2. `<workspace>/skills/<nombre>/SKILL.md` o `<nombre>.md`
3. `~/.onyx/skills/<nombre>/SKILL.md` o `<nombre>.md` *(skills globales de usuario)*
4. Skills integradas: `scent` y `rust`

Para crear una nueva skill descubrible por `load_skill(name)`:
- Usa frontmatter en las primeras 20 líneas con `name:` y `description:` (la línea `description:` alimenta el resumen del catálogo en el prompt del sistema).
- No incluyas secretos ni tokens reales (todo contenido pasa por `scrub_secrets`).
- Mantén el archivo muy por debajo de `64 KiB` para no saturar el contexto al cargarlo bajo demanda.

---

## 6. Recetas de Oro y Recuperación Time-Travel

### Sesión importante con trazabilidad completa
```sh
openonyx chat --snapshot-dir .snaps_feat --record feat.tr
# Más tarde, reanudar desde el último hito:
openonyx chat --resume .snaps_feat --record feat_part2.tr
```

### Bifurcar desde un tick anterior sin perder el historial original
```sh
# 1. Listar hitos disponibles y sus checkpoints Git
openonyx inspect .snaps_feat --list

# 2. Abrir TUI directamente en el tick 12 creando un fork hermano automático (.snaps_feat.fork-t12)
openonyx chat --resume .snaps_feat --tick 12

# O especificando nombre de fork, o desde el subcomando rewind:
openonyx rewind .snaps_feat 12 --interactive --fork-dir .snaps_alt
```
También puedes hacerlo **sin salir de la TUI** con `/fork experimento` o `/rewind 12`.

### Protocolo seguro de `rewind` headless / replay
```sh
# 1. Previsualizar efectos huérfanos y rollback sin modificar disco
openonyx rewind .snaps_feat 12 --dry-run

# 2. Ejecutar rewind determinista con transcripción (o en vivo omitiendo --transcript)
openonyx rewind .snaps_feat 12 --transcript feat.tr
```

### Auditoría tras corte o error inesperado
```sh
openonyx doctor --snapshot-dir .snaps_feat --transcript feat.tr
```
Si un crash truncó el final de `snapshots.onyxpack`, `PackSnapshotStore` ignora de forma segura la cola truncada en lectura y la repara automáticamente en el siguiente append. Si un endpoint OpenAI/vLLM corta el stream SSE tras emitir la respuesta o sufre un fallo HTTP transitorio (`408`/`429`/`5xx`/red), el adaptador aplica tolerancia de EOF completado y hasta 2 reintentos automáticos con backoff.
