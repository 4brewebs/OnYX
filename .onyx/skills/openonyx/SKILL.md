---
name: openonyx
description: Guía operativa completa para usar, configurar, depurar y automatizar openOnyx (CLI, TUI, proveedores LLM, esfuerzo de razonamiento, replay, snapshots, rewind, fork, doctor, modos, sandbox, Sprout y herramientas).
---

# openOnyx (`openonyx`) — Guía Operativa

`openonyx` es el harness CLI y TUI de OnYX (`onyx-soul-agents` + `onyx-workspace`). Conecta un proveedor LLM por HTTP/SSE (`gemini`, cualquier endpoint compatible con OpenAI `/v1/chat/completions` con tolerancia a cortes SSE y reintentos transitorios, o `anthropic` nativo con prompt caching `ephemeral` y *extended thinking* `--thinking-budget`) con herramientas dinámicas MCP (`[mcp.servers.<name>]` stdio/http → `mcp__<server>__<tool>`) y con un runtime determinista de grafo EAV (`AgentHost`), registro de transcripciones (`ONYXTR2`), snapshots comprimidos por hitos (`snapshots.onyxpack`), checkpoints Git reversibles (`/undo`), time-travel seguro con bifurcación (`/rewind`, `/fork`, `chat --resume --tick`, `rewind --interactive`), sandbox OS, subagentes asíncronos en worktrees Git aislados con fusión 3-way (*Scent-Heated Merge*) y harnesses CLI externos como drivers (`/spawn`, `[[external_drivers]]`), vistas TUI modulares `F1..F5` con paleta `Ctrl+K`, objetivo LLM conmutado en caliente (`[[llm_profiles]]`, `/profile`/`/provider`/`/model`/`/effort`, cabecera `provider/model` viva y selector Web sin credenciales), y orquestación local de servicios (`sprout.toml`).

> **Manual de usuario completo:** consulta [`docs/manual/index.md`](../../../docs/manual/index.md) para tutoriales paso a paso, arquitectura de la TUI, configuración de permisos/sandbox, time-travel, catálogo completo de las 24 herramientas (incluida `scent_references` de Scent 2.0) y resolución de problemas.

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
| `ONYX_PROVIDER` | Proveedor por defecto (`gemini` \| `openai` \| `anthropic`) | `gemini` (sobrescrito por `--provider`) |
| `GEMINI_API_KEY` / `ONYX_API_KEY` | API key para `--provider gemini` | Requerida en `chat`, `run` y `rewind` vivo |
| `ONYX_MODEL` / `ONYX_GEMINI_MODEL` | Modelo para Gemini | `gemini-4-hydrogen` (sobrescrito por `--model`) |
| `GEMINI_API_BASE` | URL base personalizada de Gemini | Sobrescrito por `--api-base` |
| `OPENAI_API_KEY` / `ONYX_API_KEY` | API key para `--provider openai` | Opcional si usas `--api-base` / `OPENAI_API_BASE` local |
| `ONYX_MODEL` / `OPENAI_MODEL` | Modelo para backend OpenAI | `gpt-4o` (sobrescrito por `--model`) |
| `OPENAI_API_BASE` / `OPENAI_BASE_URL` | Endpoint compatible OpenAI | `https://api.openai.com/v1` |
| `ANTHROPIC_API_KEY` / `ONYX_API_KEY` | API key para `--provider anthropic` | Requerida en `chat`, `run` y `rewind` vivo |
| `ONYX_MODEL` / `ANTHROPIC_MODEL` | Modelo para Anthropic | `claude-sonnet-4-5` (sobrescrito por `--model`) |
| `ANTHROPIC_API_BASE` | URL base de la API de Anthropic | `https://api.anthropic.com` (sobrescrito por `--api-base`) |
| `ANTHROPIC_THINKING_BUDGET` | Presupuesto de *extended thinking* en tokens (`0` = off, `1024..=60000`) | Prioridad sobre el presupuesto derivado de `--effort` (2048/8192/16384) |
| `ONYX_EFFORT` | Esfuerzo de razonamiento (`low` \| `medium` \| `high`) | Sobrescrito por `--effort`; fallbacks: `GEMINI_THINKING_LEVEL` (Gemini), `OPENAI_REASONING_EFFORT` (OpenAI); en Anthropic lo gobierna `ANTHROPIC_THINKING_BUDGET` |
| `ONYX_AUTO_SAVE` | Activa `--auto-save` por defecto (`1` \| `true` \| `yes` \| `on`) | Guarda automáticamente snapshots + `.tr` en `.onyx/vault/<id>/` |

> **Nota:** `replay`, `inspect`, `rewind --dry-run`, `rewind --transcript` y `doctor` funcionan sin API key ni llamadas de red.

### Ejemplos de arranque rápido

```sh
# TUI interactiva con guardado automático en .onyx/vault/ (snapshots + .tr)
openonyx chat --effort high --auto-save

# TUI con nombre de sesión corto en .onyx/vault/mi-feat/ (snapshots + chat.tr juntos)
openonyx chat --snapshot-dir mi-feat --record chat.tr

# TUI con OpenAI / OpenRouter
openonyx chat --provider openai --model gpt-4o --effort medium --auto-save

# TUI con Anthropic nativo (thinking acotado)
openonyx chat --provider anthropic --model claude-sonnet-4-5 --thinking-budget 8192 --auto-save

# TUI con modelo local en Ollama / vLLM / LM Studio (sin API key requerida)
openonyx chat --provider openai --api-base http://localhost:11434/v1 --model qwen2.5-coder:14b --effort low --auto-save

# Turno único headless con guardado automático
openonyx run "resume la arquitectura de crates" --auto-save
```

### Perfiles LLM y cambio en caliente (`[[llm_profiles]]`)

`.onyx/config.toml` admite (global `~/.onyx/config.toml` + workspace, fusión *replace-by-id* con precedencia `global < workspace (trusted)`) objetivos LLM nombrados:

```toml
[[llm_profiles]]
id = "local-coder"            # [a-z0-9_-]; "custom" está reservado
provider = "openai"           # "gemini" | "openai" | "anthropic"
model = "qwen2.5-coder:14b"   # no vacío
effort = "medium"             # "low" | "medium" | "high" | ausente (auto)
api_base = "http://localhost:11434/v1"
api_key_env = "OLLAMA_API_KEY"   # nombre de la variable de entorno — nunca el valor literal
```

- Los perfiles del workspace solo se aplican en workspaces **confiados** (mismo gate de Workspace Trust que `[[external_drivers]]`; en `[UNTRUSTED]` se ignoran por completo). Las API keys solo se referencian por nombre de variable de entorno (`api_key_env`: la estructura no tiene campo de clave literal, de modo que ningún snapshot público puede exponerlas), y `api_base` se valida en el registro (`validate_api_base`: `http://` solo en loopback, remotos exigen `https://` + autorización explícita — bloquea exfiltración de credenciales).
- **Hot-swap**: `/profile [<id>]`, `/provider <gemini|openai|anthropic>`, `/model <name>` y `/effort <auto|low|medium|high>` (TUI y Web, case-insensitive) cambian el objetivo activo **en la frontera de tick** (`DynamicLlmDriver`): no interrumpen el turno en vuelo, los subagentes pinnan el objetivo del tick de spawn, cada conmutación queda sellada en `ONYXTR2` (el `replay` reproduce el hot-swap con 0 I/O vía `LlmProfileHandle::new_for_replay`) y la cabecera TUI/Web muestra el `provider/model · effort` **vivo** (fuente: `LlmProfileHandle::public_snapshot()`, con estado modal `ws.llm.*` restaurado en `/rewind` / `chat --resume --tick`).
- **Web UI**: la PWA expone un selector LLM sin credenciales (`#llm-config-btn` → `<dialog id="llm-modal">` nativo, 0 `innerHTML`) con los perfiles registrados (los no disponibles aparecen deshabilitados con motivo accionable), provider, modelo y esfuerzo; aplica **solo los campos modificados** como slash estándar por el endpoint autenticado existente `/api/v1/command` (0 endpoints nuevos) y los push SSE con el modal abierto nunca reescriben los campos en edición.

---

## 2. Subcomandos y Opciones CLI

### Subcomandos

1. **`chat`**: Lanza la TUI (`ratatui` + `crossterm`) con streaming SSE en vivo, emblema Braille de estado, cabecera compacta con objetivo LLM `provider/model` vivo/`--effort`/modo/sandbox, vista previa de diffs antes de aprobar, vistas modulares `F1..F5` (Chat, Agents, Sprout, Plan & Diff, Timeline) con paleta `Ctrl+K` y `/view`, selector de agentes (`/agent`, `Ctrl+Tab`), cambio en caliente de objetivo LLM (`/profile`, `/provider`, `/model`, `/effort`), alternancia `[PLAN]` / `[BUILD]`, time-travel en sesión (`/rewind`, `/fork`, `/undo`) y paneles laterales `Live Plan` + `Services (Sprout)`.
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
6. **`doctor`**: Verifica salud del worktree Git, artefactos residuales, integridad de `snapshots.onyxpack` (checksums FNV-1a, payloads `ONYXAG`, colas truncadas tras crash), snapshots legacy, cabeceras de transcripción, objetos `tree_sha` del ledger, driver de sandbox OS disponible, estado de **Workspace Trust**, configuración/puertos de `sprout.toml`, **drivers externos** `[[external_drivers]]` (binario en `PATH`/`PATHEXT` con probe `--version` aislada de 3 s, y severidad por origen: `[WARN]` preset integrado ausente, `[ERR]` driver declarado ausente, `[CHECK]` binario que responde y allowlist enforzada solo con `strict-container` activo) y **servidores MCP** `[mcp.servers.*]` (handshake acotado a 5 s: `Connected`/`Disabled` → `[CHECK]`, fallo → `[ERR]` scrubbeado, workspace no confiado → `[WARN]` sin descubrimiento).
7. **`trust [add|list|check|revoke]`**: Aprueba, lista, verifica o revoca los hashes `blake3` de `.onyx/config.toml` y `sprout.toml` en `~/.onyx/trusted_workspaces.toml`. En repositorios no confiados o modificados (`[UNTRUSTED]`), las reglas `allow` y relajaciones de `[sandbox]`/`[impact]` del proyecto se ignoran y `sprout.toml` bloquea la ejecución hasta confirmación explícita (también desde `/trust` en TUI).
8. **`serve [--bind <ip:port>] [--api-key <key>] [--cors-origin <origin>]`**: Sirve la PWA mobile-first embebida (Vanilla HTML/CSS/JS, 0 Node, 0 `tokio`) — con selector LLM sin credenciales para `[[llm_profiles]]` — y la API `/api/v1/...` (JSON + SSE) lista para clientes externos (p. ej. Svelte) y despliegue en VPS detrás de HTTPS/Tailscale. Escucha en `127.0.0.1:8787` por defecto (generando clave aleatoria de 256 bits) y bloquea `--bind 0.0.0.0:...` sin `--api-key` / `ONYX_WEB_API_KEY` explícita (`>= 16` chars).
9. **`bench --snapshot-dir <dir> --tick <n> --models <list> [--goal <text>] [--verify <cmd>] [--out <file.json>] [--max-steps <n>]`**: benchmark multi-modelo desde un snapshot idéntico. Cada candidato de `--models` (LLM vivo `<provider>:<model>` con `provider ∈ gemini|openai|anthropic`, o replay offline `ONYXTR2` determinista `replay:<label>=<file.tr>` / `replay:<file.tr>` — el parse divide primero por `:` y luego por `=`, de modo que las rutas con letra de unidad Windows se conservan intactas) se ejecuta en un **worktree desechable** `bench-<pid>-<idx>-<label>` con rewind al `--tick`, host de sesión fresco y, opcionalmente, `--goal` para spawnear el agente raíz si el tick no tiene agente activo; `--verify <cmd>` se ejecuta sandboxed dentro del worktree del candidato (misma tubería que `shell_exec`, respeta `[permissions].deny`; sin `--verify`, `passed_verify` = agente asentado `Done`). La raíz y `HEAD` quedan **100 % intactas** (guard RAII del worktree, también en error). Salida: tabla Markdown de 9 columnas en `stdout` (`model`, `passed_verify`, `ticks`, `tool_calls`, `input_tokens`, `cached_tokens`, `output_tokens`, `estimated_cost_usd`, `wall_time_ms`) e informe JSON completo con `--out <file.json>`. `--auto-approve` está pensado para ejecución **desatendida**: cada candidato corre solo en su worktree y la raíz nunca se toca.

### Flags transversales principales

- `-w, --workspace <dir>`: directorio del proyecto objetivo (default: CWD).
- `--provider <gemini|openai|anthropic>` / `--model <name>` / `--effort <low|medium|high>` / `--thinking-budget <n>` / `--api-base <url>`: selección del backend LLM y nivel de razonamiento (`--thinking-budget` = presupuesto de *extended thinking* de Anthropic en tokens, `0` = off, `1024..=60000`) (`--api-base` solo permite `http://` en loopback `localhost`/`127.0.0.1`/`[::1]`, exige `https://` en hosts remotos, y pide `--allow-custom-auth-host` / `ONYX_ALLOW_CUSTOM_AUTH_HOST=1` para enviar API keys a hosts remotos no oficiales).
- `--trust-workspace`: confía en memoria `.onyx/config.toml` y `sprout.toml` del workspace para esta ejecución.
- `--auto-approve`: omite confirmación interactiva para herramientas mutantes (`fs_write`, `fs_edit`, `shell_exec`, `service_*`; las reglas `deny` de `.onyx/config.toml` siguen bloqueando y el sandbox se eleva automáticamente a `workspace-write` + `offline` salvo `--sandbox off`).
- `--ignore-deny`: desactiva explícitamente las reglas `[permissions].deny` de `.onyx/config.toml`.
- `--sandbox <off|workspace-write|strict-container>` y `--sandbox-net <offline|localhost|unrestricted>`: perfil de sandbox OS (`env_clear()`, protección RO del Control Plane `.onyx/config.toml`/`.git/hooks/**`/`.git/config`/`.env*`, `ProcessJob` RAII, y fallback automático de `bwrap` en Linux con AppArmor restrictivo) y política de red para `shell_exec`. El estado se muestra en `[SANDBOX:...]` y se audita en `openonyx doctor`.
- `--max-steps <n>`: límite de pasos del agente raíz por turno (default: `128` en `chat`, `64` en `run`; los subagentes lanzados con `spawn_agent` heredan un techo por defecto de `32` pasos salvo override explícito `max_steps` en los argumentos).
- `--max-subagents <n>`: tope de subagentes **asíncronos** concurrentes en worktree (`spawn_subagent` / `/spawn`; default `3`, `ONYX_MAX_SUBAGENTS`); aplica a los backends nativo y externo por igual.
- `--shell-timeout <secs>`: timeout en segundos por comando `shell_exec` (default de `WorkspacePolicy`: `30s`).
- `--auto-save`: genera automáticamente una carpeta descriptiva en `<workspace>/.onyx/vault/<YYYYMMDD-HHMMSS>_<modelo>[_<rama|goal>]/` guardando juntos `snapshots.onyxpack` y `<id>.tr` (activable por defecto con `ONYX_AUTO_SAVE=1`). Crea `.onyx/vault/.gitignore` (`*`) automáticamente para no ensuciar Git ni los checkpoints de `/undo`.
- `--record <file>` / `--save-transcript <file>`: guarda transcripción determinista `ONYXTR2`. Los nombres cortos sin separador de ruta se guardan dentro de `--snapshot-dir` si existe, o en `.onyx/vault/unassigned/<file>` si no hay directorio de snapshots (rutas explícitas con `/` o `\` se respetan tal cual).
- `--snapshot-dir <dir>`: guarda hitos comprimidos en `<dir>/snapshots.onyxpack` (nombres cortos sin separador se resuelven dentro de `.onyx/vault/<dir>/`; rutas explícitas se respetan tal cual). Al leer (`--resume`, `inspect`, `rewind`, `replay`, `doctor`), los nombres cortos también se resuelven automáticamente en `.onyx/vault/`.
- `--snapshot-every <n>`: fuerza snapshots periódicos adicionales cada `<n>` ticks.
- `--max-snapshots <n>`: retención opt-in por ventana deslizante (conserva `init` + últimos `n - 1` hitos).
- `-r, --resume <dir>` *(solo `chat`)*: reanuda desde el último hito guardado en `<dir>`, o desde un tick específico combinándolo con `--tick <n>` (crea fork hermano seguro por defecto; personalizable con `--fork-dir <dir>` o destructivo opt-in con `--truncate-future`).

---

## 3. Controles TUI, Modos y Comandos Slash (`chat`)

### Atajos de teclado

- **`Enter`**: envía objetivo, prompt de seguimiento (reactiva al mismo agente raíz conservando toda su `rel_memory` vía `continue_agent`) o comando slash. Mientras el agente raíz está ocupado (`thinking`, `acting`, `waiting_children` o `waiting_approval`), los prompts enviados se encolan automáticamente (`queued:N`, limpiable con `Ctrl+X`) y se despachan al terminar sin crear un segundo agente raíz.
- **`/` y `@`**: abre el popup de autocompletado interactivo de comandos slash (`/`) o archivos del workspace (`@`, ignorando `.git`/`target`/`.onyx/vault`, navegable con `↑`/`↓`, `Tab`/`Enter` para aceptar y `Esc` para cerrar).
- **`Left` / `Right` / `Home` / `End` / `Delete`**: edición UTF-8 con cursor visible; **`Alt+Up` / `Alt+Down`** (`Ctrl+P` / `Ctrl+N`) navega el historial de prompts preservando el borrador.
- **`Tab`**: alterna en caliente entre `[BUILD]` (edición permitida) y `[PLAN]` (solo lectura).
- **`F1..F5`**: cambia entre las cinco vistas efímeras — `F1 Chat` (traza completa, paneles, input y aprobaciones), `F2 Agents` (agentes del grafo + jobs asíncronos `[bg]`; `↑`/`↓` selecciona y `Enter` enfoca de vuelta en `F1 Chat`), `F3 Sprout` (lista/DAG de servicios de `sprout.toml` + logs en vivo del servicio seleccionado; las teclas mutantes `s`/`x`/`r`/`a` están gateadas por modo/confianza con aviso), `F4 Plan & Diff` (plan activo + ledger de efectos Git) y `F5 Timeline` (hitos de `snapshots.onyxpack` para `/rewind` / `/fork`). La tira de pestañas muestra el badge `[F1 Chat !APPROVAL]` cuando hay una aprobación pendiente fuera de Chat.
- **`Ctrl+K`**: abre la paleta de comandos (vistas + objetivos LLM + agentes): cualquier carácter imprimible filtra (subsecuencia fuzzy), `↑`/`↓`/`Ctrl+P`/`Ctrl+N` navegan, `Enter` aplica y `Esc`/`Ctrl+K` cierra. Mientras está abierta traga todas las teclas (salvo `Ctrl+C`).
- **`Ctrl+Tab`**: rota el agente enfocado (ciclo rápido); con varios agentes, `/agent` (alias `/agents`) abre el popup interactivo **Agent Selector** para inspeccionar y enfocar al agente raíz o cualquiera de sus subagentes (`↑`/`↓`, `Enter`, `Esc`).
- **`y` / `n`**: aprueba o rechaza una herramienta mutante pendiente (`WaitingApproval`), indicando el origen `[agent #id name]` y mostrando diff unificado en color para `fs_write` y `fs_edit` multi-hunk.
- **`Up` / `Down` / `PgUp` / `PgDn` / `Home` / `End` / `Ctrl+U` / `Ctrl+D`**: desplaza el historial `Memory & Trace` incluso durante un prompt de aprobación.
- **`Esc`**: cierra el popup/paleta abierta; en `F1 Chat` sale limpiamente guardando snapshot final (`exit`) y transcripción si están configurados; en `F2..F5` **vuelve a `F1 Chat`** en vez de salir (nunca sale a ciegas desde otra vista). **`Ctrl+C`** siempre sale limpiamente.
- **Cabecera compacta, emblema Braille y paneles laterales**: la cabecera de 3 filas muestra el emblema animado (`⠋..⠏`, throttled a 90 ms), `[PLAN]`/`[BUILD]`, `[SANDBOX:...]`, `[UNTRUSTED]`, el objetivo LLM activo `provider/model` **vivo** (sigue los hot-swaps de `/profile`/`/provider`/`/model`/`/effort`) y `--effort`, además de telemetría en vivo (`ctx: <1%`/`XX% · in / cache / out / think · ~$cost` o `local`). Cuando `plan_update` y/o `sprout.toml` están activos y el terminal tiene `>= 100` columnas, se muestran a la derecha `Live Plan` y `Services (Sprout)`.

### Comandos Slash

- **`/agent [id|name]`** (alias `/agents`): abre el selector de agentes o enfoca directamente a un agente por ID numérico o nombre (etiqueta de hito `cmd:/agent`).
- **`/view <chat|agents|sprout|plan|timeline>`**: cambia la vista efímera (case-insensitive). Se intercepta a nivel TUI: no llega al bus de eventos, al historial de prompts ni a `ONYXTR2` (0 ticks, 0 mutaciones de grafo).
- **`/profile [<id>]`**: sin argumentos lista los perfiles LLM registrados (`[[llm_profiles]]`); con `<id>` conmuta en caliente al perfil (frontera de tick, sellado en `ONYXTR2`, reproducido por `replay` con 0 I/O).
- **`/provider <gemini|openai|anthropic>`**, **`/model <name>`** y **`/effort <auto|low|medium|high>`**: cambian el proveedor, el modelo y el esfuerzo de razonamiento del objetivo LLM activo (misma semántica de hot-swap: no interrumpen el turno en vuelo, subagentes pinnan el tick de spawn, cabecera `provider/model` viva en TUI y Web). Todos los slash LLM son case-insensitive y también disponibles en la Web UI.
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
- **`/subagents`**: lista los jobs de subagentes asíncronos en worktree aislado (`[bg #job_id]`, estado, rama y badge `→ <driver>` cuando el motor no es `native`).
- **`/merge <job_id>`**: fusiona la propuesta sellada del job con aprobación humana única (preview 3-way con banner `# impact:` de calor Scent; `Deny` para rutas del Control Plane y `Ask` obligatorio por gobernanza) y limpia el worktree.
- **`/discard <job_id>`**: descarta el job sin fusionar (limpia el worktree y el estado `ws.subagent.*`).
- **`/spawn [--driver <id>] <tarea>`**: lanza un subagente en segundo plano en un worktree aislado **sin gastar un turno de `root`** (válido incluso antes de tener un agente raíz activo). Comando y flag `--driver` case-insensitive y tolerantes a espacios múltiples; sin driver → motor nativo.
- **Servidores MCP (`[mcp.servers.<name>]` en `config.toml`)**: `transport = "stdio"` (`command`/`args`/`env`) o `"http"` (`url`/`headers`), `timeout_secs` (default `20`), `enabled`; fusión `global < workspace (trusted)` (un workspace `[UNTRUSTED]` ignora los suyos con advertencia). Las tools descubiertas se exponen como `mcp__<server>__<tool>` con `requires_approval = true` (aprobación en `BUILD` salvo `allow` más específica, p. ej. `allow = ["mcp__github__*"]`; **`Deny` en `PLAN`/`EXPLORE`**), salidas por `truncate_utf8` + `scrub_secrets`, aislamiento de fallos por servidor y auditoría en `openonyx doctor` (handshake 5 s). `replay` reconstruye las tools `mcp__*` de la propia transcripción (0 I/O). Los subagentes no heredan el registro MCP.
- **Harnesses CLI externos (`[[external_drivers]]` en `.onyx/config.toml`)**: declara drivers adicionales (`id`, `bin`, `args`, `prompt_transport = "stdin" | "arg" | "temp_file"`, `prompt_flag`, `timeout_secs`, `allowed_env`, `inherit_home`, `network_allowlist`); el preset `opencode` existe de serie y se sobreescribe por `id` (precedencia `preset < global < workspace`). Solo se aplican en workspaces **confiados** (en un workspace `[UNTRUSTED]` se ignoran por completo, también en el selector de la Web UI), `root` necesita aprobación explícita o regla `allow spawn_subagent(<driver>)` para delegar a un driver no-nativo, y el binario se audita en `openonyx doctor` (severidad por origen + probe `--version` aislada).
- **Permisos granulares, impacto AST y Verify-on-Edit (`.onyx/config.toml`)**: `~/.onyx/config.toml` y `<workspace>/.onyx/config.toml` admiten `[permissions]` (`deny`, `allow`, `ask`, precedencia `deny > ask > allow`; `PLAN`/`EXPLORE` bloquean siempre mutaciones y `deny` aplica incluso con `--auto-approve` salvo `--ignore-deny`), `[sandbox]`, `[impact]` (*Scent-Heated Approval* monótono: escala `Allow → Ask` cuando `fs_edit`/`fs_write` toca símbolos con callers y muestra `# impact: ...` sobre el diff, guardando además `ws.effect.symbols` en el ledger) y `[verify]` / `[[verify.rules]]` (*Verify-on-Edit Loop*: `on_edit` opt-in + `max_secs` default `20` clamp `1..=120`; cada regla `glob`/`command`/`max_secs` se ejecuta sandboxed al editar un archivo que empareja la `glob` —o un test `scope:test` impactado por Scent — inyectando `[verify: PASS|FAIL|BLOCKED]` con diagnóstico acotado a 4 KiB; un workspace `[UNTRUSTED]` ignora sus reglas `[verify]`).

---

## 4. Protocolo de Trabajo Recomendado para Agentes

Cuando operes dentro de `openonyx`, sigue esta disciplina para minimizar tokens, errores y aprobaciones innecesarias:

1. **Mapeo AST antes de leer archivos completos**:
   - Usa `glob_files` para localizar rutas por patrón (`crates/**/*.rs`, `**/*SKILL.md`) sin pedir aprobación.
   - Usa `code_search` v2 con `regex`, `case_insensitive`, `include_glob`, `exclude_glob` y `context_lines` (`0..=5`).
   - Usa `scent_outline` para obtener firmas `@TOON v1` y rangos de líneas (`seen_outlines` cachea llamadas repetidas en sesión y se invalida automáticamente tras `fs_edit`/`fs_write`/`shell_exec`/`/undo`).
   - Usa `scent_impact` sobre símbolos compartidos antes de refactorizar.
   - Usa `scent_references` para ver la definición exacta y las referencias directas de un símbolo (callers, callees, implementors, type users) antes de cambiar su API. Scent 2.0 soporta **7 lenguajes** (Rust, Python, Java, TypeScript, JavaScript, Go, C/C++) con `EntityKind::{TypeAlias, Component}`, caché binaria incremental persistente `.onyx/scent.idx` (`ONYXSCI1`, recarga en decenas de ms, reconstrucción silenciosa ante anomalías) y CLI `scent outline|impact|references` (feature `cli`).
   - Usa `fs_read` acotado con `start_line` / `end_line` (límite de lectura: `64 KiB`; `fs_read` deduplica lecturas repetidas y rangos solapados ya cubiertos con `covered_by:start-end`, y rechaza con aviso accionable artefactos binarios de OnYX `ONYXTR1`/`ONYXTR2`/`ONYXPK1`/`ONYXAG`).
2. **Planificación visible**:
   - En tareas multi-paso, mantén actualizado `plan_update` (`pending`, `in_progress`, `completed`) sin llamarlo varias veces seguidas sin herramientas de workspace de por medio.
3. **Ediciones limpias y seguras**:
   - En modo `BUILD`, **siempre** lee el archivo con `fs_read` antes de `fs_edit` / `fs_write`.
   - Prefiere `fs_edit` con `edits: [{ old_text, new_text }]` para cambios atómicos multi-hunk en un mismo archivo.
   - Usa `git_status` y `git_diff` (sin aprobación) en lugar de `shell_exec` para inspeccionar estado Git.
   - Reserva `shell_exec` para compilar, testear o ejecutar comandos mutantes (salida limitada a `32 KiB`, filtrada con `scrub_secrets`; evita crear scripts temporales ad-hoc cuando exista herramienta nativa).
4. **Subagentes acotados**:
   - Usa `spawn_agent` para delegar subtareas independientes **síncronas** con un `goal` claro y acotado (techo por defecto: `32` pasos por subagente; inspeccionables en vivo con `F2` / `/agent`). Con `isolated: true`, el hijo corre en un worktree Git aislado `onyx/sub/agent-<parent>-<child>-<seq>` (creado con `commit-tree` **sin mover `HEAD` ni el índice** de la raíz; exige Git `>= 2.38.0`) y su reconciliación 3-way `git merge-tree` se aplica a la raíz solo al finalizar limpio; un worktree con conflictos se transfiere a la gobernanza estándar (`subagent_status` / `subagent_merge` / `subagent_discard`) y la limpieza ocurre en apply/discard, `Drop` del bridge y poda de worktrees huérfanos en `openonyx doctor`.
   - Usa `spawn_subagent` para trabajo **asíncrono en segundo plano** en un worktree Git aislado (tope de concurrencia `--max-subagents`, default `3`; parámetro opcional `driver` para un harness CLI de `[[external_drivers]]`, que exige aprobación). La propuesta sellada vuelve a `root` para una fusión 3-way con aprobación humana única (`subagent_merge`); consulta el estado en vivo con `subagent_status`.

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
