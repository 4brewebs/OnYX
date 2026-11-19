---
name: openonyx
description: Guía operativa completa para usar, configurar, depurar y automatizar openOnyx (CLI, TUI, proveedores LLM, replay, snapshots, rewind, doctor, modos y herramientas).
---

# openOnyx (`openonyx`) — Guía Operativa

`openonyx` es el harness CLI y TUI de OnYX (`onyx-soul-agents` + `onyx-workspace`). Conecta un proveedor LLM por HTTP/SSE (`gemini` o cualquier endpoint compatible con OpenAI `/v1/chat/completions`) con un runtime determinista de grafo EAV (`AgentHost`), registro de transcripciones (`ONYXTR2`), snapshots comprimidos por hitos (`snapshots.onyxpack`), checkpoints Git reversibles (`/undo`) y time-travel (`rewind`).

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

> **Nota:** `replay`, `inspect`, `rewind --dry-run`, `rewind --transcript` y `doctor` funcionan sin API key ni llamadas de red.

### Ejemplos de arranque rápido

```sh
# TUI interactiva con Gemini + snapshots + transcripción
openonyx chat --snapshot-dir snaps_chat --record chat.tr

# TUI con OpenAI / OpenRouter
openonyx chat --provider openai --model gpt-4o --snapshot-dir snaps_openai

# TUI con modelo local en Ollama / vLLM / LM Studio (sin API key requerida)
openonyx chat --provider openai --api-base http://localhost:11434/v1 --model qwen2.5-coder:14b

# Turno único headless
openonyx run "resume la arquitectura de crates" --record run.tr --snapshot-dir snaps_run
```

---

## 2. Subcomandos y Opciones CLI

### Subcomandos

1. **`chat`**: Lanza la TUI (`ratatui` + `crossterm`) con streaming SSE en vivo, vista previa de diffs antes de aprobar, alternancia `[PLAN]` / `[BUILD]` y panel lateral `Live Plan`.
2. **`run <goal>`**: Ejecuta un único objetivo en modo headless y muestra el resultado final.
3. **`replay <transcript>`**: Reproduce offline una transcripción `ONYXTR2` (0 llamadas HTTP, 0 mutaciones en disco). Acepta `--snapshot-dir`, `--snapshot-every`, `--max-snapshots`.
4. **`inspect <snapshot_dir>`**: Inspecciona hitos en `snapshots.onyxpack` (o archivos legacy `tick-*.bin`).
   - `--list` / `-l`: lista todos los ticks con etiqueta de hito, tamaños, checksum FNV-1a y checkpoint Git.
   - `--tick <n>`: inspecciona un tick específico.
   - `--json`: salida estructurada en JSON.
5. **`rewind <snapshot_dir> <tick>`**: Retrocede al tick `<n>` (o al hito previo más cercano `<= N`), audita efectos secundarios huérfanos y restaura el checkpoint de árbol Git en el workspace.
   - `--dry-run`: previsualiza efectos huérfanos y plan de rollback sin tocar archivos.
   - `--transcript <file>`: reanuda determinísticamente desde una transcripción grabada.
6. **`doctor`**: Verifica salud del worktree Git, artefactos residuales, integridad de `snapshots.onyxpack` (checksums FNV-1a, colas truncadas tras crash), snapshots legacy, cabeceras de transcripción y objetos `tree_sha` del ledger.

### Flags transversales principales

- `-w, --workspace <dir>`: directorio del proyecto objetivo (default: CWD).
- `--provider <gemini|openai>` / `--model <name>` / `--api-base <url>`: selección del backend LLM.
- `--auto-approve`: omite confirmación interactiva para herramientas mutantes (`fs_write`, `fs_edit`, `shell_exec`; las reglas `deny` de `.onyx/config.toml` siguen bloqueando).
- `--ignore-deny`: desactiva explícitamente las reglas `[permissions].deny` de `.onyx/config.toml`.
- `--max-steps <n>`: límite de pasos del agente por turno (default: `128` en `chat`, `64` en `run`).
- `--shell-timeout <secs>`: timeout en segundos por comando `shell_exec` (default de `WorkspacePolicy`: `30s`).
- `--record <file>` / `--save-transcript <file>`: guarda transcripción determinista `ONYXTR2`.
- `--snapshot-dir <dir>`: guarda hitos comprimidos en `<dir>/snapshots.onyxpack`.
- `--snapshot-every <n>`: fuerza snapshots periódicos adicionales cada `<n>` ticks.
- `--max-snapshots <n>`: retención opt-in por ventana deslizante (conserva `init` + últimos `n - 1` hitos).
- `-r, --resume <dir>` *(solo `chat`)*: reanuda desde el último hito guardado en `<dir>`.

---

## 3. Controles TUI, Modos y Comandos Slash (`chat`)

### Atajos de teclado

- **`Enter`**: envía objetivo, prompt de seguimiento (reactiva al mismo agente raíz conservando toda su `rel_memory` vía `continue_agent`) o comando slash.
- **`Tab`**: alterna en caliente entre `[BUILD]` (edición permitida) y `[PLAN]` (solo lectura).
- **`y` / `n`**: aprueba o rechaza una herramienta mutante pendiente (`WaitingApproval`), mostrando diff unificado en color para `fs_write` y `fs_edit` multi-hunk.
- **`Up` / `Down` / `PgUp` / `PgDn` / `Home` / `End` / `Ctrl+U` / `Ctrl+D`**: desplaza el historial `Memory & Trace` incluso durante un prompt de aprobación.
- **`Esc` / `Ctrl+C`**: sale limpiamente guardando snapshot final y transcripción si están configurados.
- **Panel `Live Plan`**: aparece automáticamente a la derecha (`70%` / `30%`) cuando `plan_update` registra pasos (`[x]`, `[>]`, `[ ]`) y el ancho de terminal es `>= 100` columnas.

### Comandos Slash

- **`/mode [plan|build|explore]`**: cambia al modo indicado o alterna `PLAN` ↔ `BUILD` si se omite el argumento.
  - `BUILD`: lectura y modificación controlada (`fs_edit` exige lectura previa del archivo).
  - `PLAN`: solo lectura y análisis; bloquea mutaciones.
  - `EXPLORE`: exploración de solo lectura.
- **`/undo`**: revierte el último checkpoint Git reversible (`fs_write` / `fs_edit`) creado en la sesión sin alterar `HEAD` ni el staging index del usuario.
- **`/compact [focus]`**: compacta determinísticamente el historial del agente en `[session_summary]` (rescatando `files_found` y `focus_matches` relevantes a `[focus]` de turnos previos, con hito `cmd:/compact` en `snapshots.onyxpack`) y preserva los ticks previos para `rewind` y `replay`.
- **`/new`** o **`/new <goal>`**: fuerza que el siguiente prompt cree un agente raíz limpio (o lo lanza de inmediato con `<goal>`).
- **Telemetría en vivo**: barra en la cabecera con ocupación de contexto (`ctx: <1%` / `XX%`), tokens `in` / `cache` / `out` / `think` y coste estimado (`~$...` o `local`).
- **Permisos granulares (`.onyx/config.toml`)**: `~/.onyx/config.toml` y `<workspace>/.onyx/config.toml` admiten `[permissions]` (`deny`, `allow`, `ask`, precedencia `deny > ask > allow`; `PLAN`/`EXPLORE` bloquean siempre mutaciones y `deny` aplica incluso con `--auto-approve` salvo `--ignore-deny`).

---

## 4. Protocolo de Trabajo Recomendado para Agentes

Cuando operes dentro de `openonyx`, sigue esta disciplina para minimizar tokens, errores y aprobaciones innecesarias:

1. **Mapeo AST antes de leer archivos completos**:
   - Usa `glob_files` para localizar rutas por patrón (`crates/**/*.rs`, `**/*SKILL.md`) sin pedir aprobación.
   - Usa `code_search` v2 con `regex`, `case_insensitive`, `include_glob`, `exclude_glob` y `context_lines` (`0..=5`).
   - Usa `scent_outline` para obtener firmas `@TOON v1` y rangos de líneas.
   - Usa `scent_impact` sobre símbolos compartidos antes de refactorizar.
   - Usa `fs_read` acotado con `start_line` / `end_line` (límite de lectura: `64 KiB`; `fs_read` deduplica lecturas sin cambios en la sesión).
2. **Planificación visible**:
   - En tareas multi-paso, mantén actualizado `plan_update` (`pending`, `in_progress`, `completed`) sin llamarlo varias veces seguidas sin herramientas de workspace de por medio.
3. **Ediciones limpias y seguras**:
   - En modo `BUILD`, **siempre** lee el archivo con `fs_read` antes de `fs_edit` / `fs_write`.
   - Prefiere `fs_edit` con `edits: [{ old_text, new_text }]` para cambios atómicos multi-hunk en un mismo archivo.
   - Usa `git_status` y `git_diff` (sin aprobación) en lugar de `shell_exec` para inspeccionar estado Git.
   - Reserva `shell_exec` para compilar, testear o ejecutar comandos mutantes (salida limitada a `32 KiB`, filtrada con `scrub_secrets`).
4. **Subagentes acotados**:
   - Usa `spawn_agent` para delegar subtareas independientes con un `goal` claro y acotado.

---

## 5. Instrucciones de Proyecto y Skills Personalizadas

Al iniciar (`chat`, `run` y `rewind` vivo), `openonyx` carga instrucciones Markdown en este orden (hasta `16 KiB` en total, filtradas con `scrub_secrets`):

1. `~/.onyx/AGENTS.md` y `~/.onyx/ONYX.md`
2. `<workspace>/AGENTS.md` y `<workspace>/ONYX.md`

Además, inyecta el catálogo compacto de skills (`discover_skills_summary`), buscando por orden de prioridad:

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

### Protocolo seguro de `rewind`
```sh
# 1. Listar hitos disponibles y sus checkpoints Git
openonyx inspect .snaps_feat --list

# 2. Previsualizar efectos huérfanos y rollback sin modificar disco
openonyx rewind .snaps_feat 12 --dry-run

# 3. Ejecutar rewind determinista con transcripción (o en vivo omitiendo --transcript)
openonyx rewind .snaps_feat 12 --transcript feat.tr
```

### Auditoría tras corte o error inesperado
```sh
openonyx doctor --snapshot-dir .snaps_feat --transcript feat.tr
```
Si un crash truncó el final de `snapshots.onyxpack`, `PackSnapshotStore` ignora de forma segura la cola truncada en lectura y la repara automáticamente en el siguiente append.
