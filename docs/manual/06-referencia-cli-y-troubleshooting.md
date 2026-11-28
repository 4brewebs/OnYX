# 06. Referencia CLI Completa y Solución de Problemas

> 🇬🇧 **[Read this chapter in English](06-cli-reference-and-troubleshooting.md)**

Referencia rápida de todos los subcomandos, flags, variables de entorno, resiliencia HTTP/SSE y soluciones a incidencias reales en Windows, Linux y macOS.

---

## 1. Referencia de Subcomandos

```text
openonyx chat [OPCIONES] [--resume <dir> [--tick <n>] [--fork-dir <dir>] [--truncate-future]]
openonyx run <goal> [OPCIONES]
openonyx replay <transcript> [--workspace <dir>] [--snapshot-dir <dir>] [--snapshot-every <n>] [--max-snapshots <n>]
openonyx inspect <snapshot_dir> [--tick <n>] [--list] [--json]
openonyx rewind <snapshot_dir> <tick> [--transcript <file>] [--interactive | --tui] [--fork-dir <dir>] [--truncate-future] [--dry-run] [OPCIONES]
openonyx bench --snapshot-dir <dir> --tick <n> --models <list> [--goal <text>] [--verify <cmd>] [--out <file.json>] [--max-steps <n>] [OPCIONES]
openonyx doctor [--workspace <dir>] [--snapshot-dir <dir>] [--transcript <file>]
openonyx trust [--workspace <dir>] [add | list | check | revoke] [--list | --check | --revoke]
openonyx serve [--workspace <dir>] [--bind <ip:port>] [--api-key <key>] [--cors-origin <origin>] [OPCIONES]
openonyx [--help | -h | --version | -V]
```

---

## 2. Tabla Completa de Opciones (Flags)

| Flag | Aplica a | Descripción / Default |
| --- | --- | --- |
| `-w, --workspace <dir>` | Todos | Directorio del proyecto objetivo (default: directorio actual). |
| `--provider <gemini\|openai\|anthropic>` | `chat`, `run`, `rewind` | Backend LLM (`gemini` por defecto; `openai` para OpenAI/OpenRouter/Ollama/vLLM/LM Studio; `anthropic` para la Messages API nativa). |
| `--model <name>` | `chat`, `run`, `rewind` | Nombre del modelo (defaults: `gemini-4-hydrogen` en Gemini, `gpt-4o` en OpenAI, `claude-sonnet-4-5` en Anthropic). |
| `--effort <low\|medium\|high>` | `chat`, `run`, `rewind` | Esfuerzo de razonamiento/pensamiento (`thinkingLevel` / `reasoning_effort`; en `anthropic` mapea al presupuesto de thinking 2048/8192/16384 salvo presupuesto explícito). |
| `--thinking-budget <n>` | `chat`, `run`, `rewind` | Presupuesto de *extended thinking* de Anthropic en tokens (`0` = off, o `1024..=60000`; o `ANTHROPIC_THINKING_BUDGET`). Eleva `max_tokens` por encima del presupuesto. |
| `--api-base <url>` | `chat`, `run`, `rewind` | URL base personalizada (`http://` permitido solo en loopback `localhost`/`127.0.0.1`/`[::1]`; hosts remotos exigen `https://`). |
| `--allow-custom-auth-host` | `chat`, `run`, `rewind` | Opt-in explícito para enviar API keys a hosts HTTPS remotos no oficiales (o `ONYX_ALLOW_CUSTOM_AUTH_HOST=1`). |
| `--trust-workspace` | `chat`, `run`, `rewind`, `serve` | Confía el workspace actual en memoria durante la sesión (sin modificar `~/.onyx/trusted_workspaces.toml`). |
| `--bind <ip:port>` | `serve` | Dirección HTTP/SSE (default: `127.0.0.1:8787`; direcciones no loopback exigen `--api-key` explícita `>= 16` chars). |
| `--api-key <key>` | `serve` | API key para autenticación (cookie `HttpOnly` o `Authorization: Bearer`; en loopback se genera aleatoria si se omite). |
| `--cors-origin <url>` | `serve` | Origen explícito permitido para frontales externos (p. ej. SPA Svelte en `http://localhost:5173`; nunca `*`). |
| `--sandbox <off\|workspace-write\|strict-container>` | `chat`, `run`, `rewind` | Perfil de sandbox OS para `shell_exec` (default: `workspace-write`). |
| `--sandbox-net <offline\|localhost\|unrestricted>` | `chat`, `run`, `rewind` | Política de red del sandbox (default: `offline`). |
| `--auto-approve` | `chat`, `run`, `rewind` | Omite confirmaciones interactivas para herramientas mutantes (`deny` sigue aplicando y eleva sandbox `off` → `workspace-write` + `offline` salvo `--sandbox off`). |
| `--ignore-deny` | `chat`, `run`, `rewind` | Desactiva explícitamente las reglas `[permissions].deny` de `.onyx/config.toml`. |
| `--max-steps <n>` | `chat`, `run` | Límite de pasos por turno del agente raíz (default: `128` en `chat`, `64` en `run`; subagentes `32` por defecto). |
| `--max-subagents <n>` | `chat`, `run`, `serve`, `rewind --interactive` | Máximo de worktrees Git en paralelo para subagentes asíncronos (default: `3`, o `ONYX_MAX_SUBAGENTS`). |
| `--shell-timeout <secs>` | `chat`, `run` | Timeout en segundos por comando `shell_exec` (default: `30s`). |
| `--llm-timeout-connect <secs>` | `chat`, `run`, `serve` | Timeout en segundos del connect HTTP (TCP/TLS) al LLM (default: `30s`; o `ONYX_LLM_TIMEOUT_CONNECT_SECS`). |
| `--llm-timeout-read <secs>` | `chat`, `run`, `serve` | Timeout en segundos del read HTTP entre bytes/chunks SSE del LLM (default: `120s`; o `ONYX_LLM_TIMEOUT_READ_SECS`). Un proveedor colgado falla el turno en tiempo acotado en vez de bloquear la sesión para siempre. |
| `--auto-save` | `chat`, `run` | Crea automáticamente `.onyx/vault/<YYYYMMDD-HHMMSS>_<modelo>[_<rama\|goal>]/` con `snapshots.onyxpack` + `.tr` juntos (o `ONYX_AUTO_SAVE=1`). |
| `--record <f>`, `--save-transcript <f>` | `chat`, `run` | Guarda transcripción determinista `ONYXTR2` (nombres cortos van dentro de `--snapshot-dir`, o en `.onyx/vault/unassigned/` si no hay snapshot dir). |
| `--snapshot-dir <dir>` | `chat`, `run`, `replay`, `doctor` | Directorio para `snapshots.onyxpack` (nombres cortos se resuelven en `.onyx/vault/<dir>/`). |
| `--snapshot-every <n>` | `chat`, `run`, `replay` | Captura además un hito `periodic` cada `<n>` ticks. |
| `--max-snapshots <n>` | `chat`, `run`, `replay` | Retención opt-in por ventana deslizante (`init` + últimos `n - 1` hitos). |
| `-r, --resume <dir>` | `chat` | Reanuda desde el último hito (o `--tick <n>`) guardado en `<dir>`. |
| `--tick <n>` | `inspect`, `chat --resume` | Tick específico a inspeccionar o desde el que reanudar/bifurcar. |
| `--fork-dir <dir>` | `chat --resume --tick`, `rewind --interactive` | Directorio destino del fork de snapshots. |
| `--truncate-future` | `chat --resume --tick`, `rewind` | Opt-in destructivo: trunca el pack origen in-place en lugar de crear un fork hermano. |
| `--interactive`, `--tui` | `rewind` | Abre la TUI interactiva en el tick restaurado en lugar de continuar en headless. |
| `-l, --list` | `inspect` | Lista todos los hitos con etiquetas, tamaños, FNV-1a y checkpoints Git. |
| `--json` | `inspect` | Emite salida estructurada en JSON. |
| `--transcript <file>` | `rewind`, `doctor` | Reproduce transcripción desde el punto de rewind o valida su cabecera en `doctor`. |
| `--dry-run` | `rewind` | Previsualiza efectos huérfanos y plan de rollback sin modificar disco. |
| `--models <list>` | `bench` | Lista de candidatos separada por comas: `<provider>:<model>` (LLM vivo, p. ej. `gemini:gemini-2.5-pro,openai:gpt-4o`) y/o `replay:<label>=<file.tr>` / `replay:<file.tr>` (transcripción `ONYXTR2` offline, determinista). |
| `--verify <cmd>` | `bench` | Comando de verificación ejecutado **sandboxed dentro del worktree** del candidato tras asentarse (misma tubería de `shell_exec`); su `exit 0` decide `passed_verify`. Sin `--verify`, `passed_verify` = el agente terminó `Done` y la sesión se asentó. |
| `--out <file.json>` | `bench` | Escribe el informe JSON completo (metadata + resultados) en pretty-print. |
| `--goal <text>` | `bench` | Objetivo del agente raíz si en el `--tick` no hay agente activo (obligatorio en ese caso). |
| `-h, --help` / `-V, --version` | Global | Muestra ayuda o versión y sale con código `0`. |

### `openonyx bench` — referencia completa

Benchmark multi-modelo desde un **snapshot idéntico**: cada candidato se ejecuta en un **worktree desechable** (`bench-<pid>-<idx>-<label>`) con rewind a `--tick` (`reconcile_rewind_effects_report`), un host de sesión fresco (`request_restore` + 1 step) y, al terminar, la raíz y `HEAD` quedan **100 % intactos** y el worktree se elimina (guard RAII; también en error o pánico).

```sh
# Comparativa en vivo + replay offline, con verificación y informe JSON
openonyx bench \
  --snapshot-dir .onyx/vault/20261125-100000_gpt-4o \
  --tick 12 \
  --models "gemini:gemini-2.5-pro,openai:gpt-4o,replay:baseline=run_baseline.tr" \
  --goal "fix the failing test in src/app.rs" \
  --verify "cargo test --workspace" \
  --out bench_report.json \
  --auto-approve \
  --sandbox workspace-write \
  --sandbox-net offline
```

- **Candidatos (`--models`):** `<provider>:<model>` con `provider ∈ gemini|openai|anthropic` (credenciales vía las variables habituales `GEMINI_API_KEY`/`OPENAI_API_KEY`/`ANTHROPIC_API_KEY` o `--api-base`); `replay:<label>=<path.tr>` fija el label; `replay:<path.tr>` usa el *file stem* como label. El parse divide primero por `:` y luego por `=`, de modo que las rutas con letra de unidad Windows (`replay:C:\runs\a.tr`) se conservan intactas.
- **Ejecución por candidato (secuencial):** rewind del worktree al tick → host fresco restaurado → si no hay agente activo en el tick, se spawnea un agente raíz con `--goal` → se ejecuta hasta asentarse (tope interno de 256 ticks) con telemetría real (tokens/coste) en candidatos vivos (replay: tokens/coste `0`).
- **`--auto-approve`:** para ejecución **desatendida** en worktrees desechables: cada candidato corre solo en su worktree, la raíz nunca se toca; `[permissions].deny` sigue aplicando.
- **Salida:** tabla Markdown de 9 columnas en `stdout` — `| model | passed_verify | ticks | tool_calls | input_tokens | cached_tokens | output_tokens | estimated_cost_usd | wall_time_ms |` — y, con `--out`, el informe JSON (`metadata` con snapshot/tick/models/verify + `results` por candidato con todas las columnas).
- **Flags reutilizados:** `--workspace <dir>`, `--sandbox <off|workspace-write|strict-container>`, `--sandbox-net <offline|localhost|unrestricted>`, `--shell-timeout <secs>`, `--llm-timeout-connect <secs>`, `--llm-timeout-read <secs>`, `--trust-workspace`, `--api-base <url>`, `--allow-custom-auth-host`, `--effort <low|medium|high>`, `--thinking-budget <n>` (el candidato vivo determina su propio `provider:model`).
- **Validación accionable:** sin `--snapshot-dir`/`--tick`/`--models` el error indica la flag que falta; proveedor desconocido → `unsupported provider ... (expected `gemini`, `openai` or `anthropic`)`; snapshot ausente → aviso de suelo (floor), no error.

---

## 3. Variables de Entorno

| Variable | Proveedor | Descripción |
| --- | --- | --- |
| `ONYX_PROVIDER` | Todos | Proveedor por defecto (`gemini` \| `openai` \| `anthropic`). |
| `GEMINI_API_KEY` | Gemini | API key de Google AI Studio (fallback: `ONYX_API_KEY`). |
| `OPENAI_API_KEY` | OpenAI | API key para `--provider openai` (fallback: `ONYX_API_KEY`; opcional con `--api-base` local). |
| `ANTHROPIC_API_KEY` | Anthropic | API key para `--provider anthropic` (fallback: `ONYX_API_KEY`). |
| `ONYX_API_KEY` | Todos | Clave compartida de fallback si no se define la específica del proveedor. |
| `ONYX_MODEL` | Todos | Modelo por defecto (fallbacks: `ONYX_GEMINI_MODEL` para Gemini, `OPENAI_MODEL` para OpenAI, `ANTHROPIC_MODEL` para Anthropic). |
| `ONYX_EFFORT` | Todos | Esfuerzo por defecto (`low` \| `medium` \| `high`; fallbacks: `GEMINI_THINKING_LEVEL`, `OPENAI_REASONING_EFFORT`; en Anthropic lo gobierna `ANTHROPIC_THINKING_BUDGET`). |
| `ONYX_AUTO_SAVE` | Ambos | Activa `--auto-save` por defecto (`1` \| `true` \| `yes` \| `on`) guardando en `.onyx/vault/`. |
| `ONYX_MAX_SUBAGENTS` | Ambos | Máximo de worktrees en paralelo para subagentes asíncronos (usado cuando no se pasa `--max-subagents <n>`; default: `3`). |
| `ONYX_LLM_TIMEOUT_CONNECT_SECS` | Todos | Timeout en segundos del connect HTTP al LLM (default: `30`; fallback de `--llm-timeout-connect`). |
| `ONYX_LLM_TIMEOUT_READ_SECS` | Todos | Timeout en segundos del read HTTP entre bytes/chunks SSE del LLM (default: `120`; fallback de `--llm-timeout-read`). |
| `GEMINI_API_BASE` | Gemini | Endpoint personalizado para Gemini. |
| `OPENAI_API_BASE` / `OPENAI_BASE_URL` | OpenAI | Endpoint compatible con OpenAI (default: `https://api.openai.com/v1`). |
| `ANTHROPIC_API_BASE` | Anthropic | Endpoint personalizado de la API de Anthropic (default: `https://api.anthropic.com`). |
| `ANTHROPIC_THINKING_BUDGET` | Anthropic | Presupuesto de *extended thinking* en tokens (`0` = off, o `1024..=60000`); tiene prioridad sobre el presupuesto derivado de `--effort`. |

> **Configuración LLM, drivers y MCP en `config.toml`:** además de flags y variables, `.onyx/config.toml` acepta bloques `[[llm_profiles]]` (capítulo 03, sección 2.6), `[[external_drivers]]` (capítulo 03, sección 2.7) y `[mcp.servers.<name>]` (capítulo 03, sección 2.8). Las credenciales de un perfil se resuelven en el orden `api_key_env` → variable estándar del proveedor → `ONYX_API_KEY`.

---

## 4. Resiliencia HTTP/SSE en Endpoints OpenAI, vLLM y Proxies

En servidores locales o proxies corporativos (vLLM, LiteLLM, nginx, Cloudflare) es frecuente que la conexión *chunked* SSE se cierre bruscamente tras emitir `finish_reason` o que devuelva un error transitorio (`408`, `429`, `500`, `502`, `503`, `504` o corte temporal de red).

El adaptador `open-onyx/src/openai.rs` incorpora tres protecciones automáticas:
1. **Tolerancia a EOF tras respuesta completada**: si el stream SSE falla al decodificar el último chunk pero ya se recibió `finish_reason` / `[DONE]` con respuesta o `tool_calls` completos, la respuesta se acepta sin abortar el turno.
2. **Reintentos automáticos con backoff**: ante fallos HTTP o de transporte transitorios, reintenta automáticamente hasta **2 veces** (`400ms`, `1200ms`) antes de reportar error al agente.
3. **Timeouts HTTP explícitos (`FIX-02`)**: cada petición a los tres proveedores fija `timeout_connect` (default `30s`) y `timeout_read` (default `120s`, por chunk SSE) configurables vía `--llm-timeout-connect` / `--llm-timeout-read` (o `ONYX_LLM_TIMEOUT_CONNECT_SECS` / `ONYX_LLM_TIMEOUT_READ_SECS`). Un proveedor que acepta la conexión y luego se queda en silencio produce un error de transporte clasificado en tiempo acotado en vez de bloquear la sesión (p. ej. `serve`) para siempre. El timeout de read **no** se reintenta según la política transitoria (apartado 2): un proveedor mudo falla una vez, dentro del presupuesto de read, porque reintentar ciegamente tras enviar el prompt completo podría disparar inferencias duplicadas; los fallos de connect, resets y `408`/`429`/`5xx` siguen reintentándose con backoff.

El adaptador nativo `open-onyx/src/anthropic.rs` aplica la misma política de reintentos (`408`, `429`, `500`, `502`, `503`, `504` y el `overloaded_error` `529` propio de Anthropic), respetando el header `retry-after` cuando el servidor lo envía.

---

## 5. Solución de Problemas Frecuentes (Troubleshooting)

### 1. `GEMINI_API_KEY environment variable is not set`
- Si quieres usar **Gemini**: exporta `GEMINI_API_KEY` (PowerShell: `$env:GEMINI_API_KEY="..."`; bash/zsh: `export GEMINI_API_KEY="..."`).
- Si quieres usar **Ollama / vLLM / LM Studio**: añade `--provider openai --api-base http://localhost:11434/v1 --model <tu-modelo>` (así no se exige API key).
- Si solo quieres usar `inspect`, `replay`, `rewind --dry-run` o `doctor`: ninguno de ellos requiere API key.

### 2. El agente intenta leer un `.tr`, `.onyxpack` o `tick-*.bin` con `fs_read`
`fs_read` detecta automáticamente las cabeceras binarias de OnYX (`ONYXTR1`, `ONYXTR2`, `ONYXPK1`, `ONYXAG`) y devuelve un error explicativo recomendando usar `openonyx inspect` o `openonyx replay` en lugar de leer el binario como texto.

### 3. `shell_exec` falla con error de red (`offline`) al ejecutar `cargo build` o `curl`
Por defecto el sandbox usa `--sandbox-net offline` (inyectando además `CARGO_NET_OFFLINE=true`).
- Si necesitas consultar un servicio local de Sprout (`127.0.0.1`): usa `--sandbox-net localhost`.
- Si necesitas descargar crates o paquetes de internet: usa `--sandbox-net unrestricted` (o añádelo temporalmente en `.onyx/config.toml`).

### 4. Linux Ubuntu 24.04+: `bwrap` no puede crear *user namespaces* (AppArmor)
Ubuntu 24.04 restringe *unprivileged user namespaces* vía AppArmor. `openonyx` ejecuta una sonda real (`bwrap_is_usable()`) y aplica automáticamente un *fallback* seguro para que `shell_exec` no falle. Comprueba el estado exacto en `openonyx doctor`.

### 5. `Ctrl+Tab` no cambia de agente en mi terminal
Algunos emuladores de terminal (especialmente en Windows o a través de tmux/SSH) interceptan o no envían `Ctrl+Tab`. Usa siempre la tecla **`F2`** (ahora abre la vista **Agents**, capítulo 02) o el comando **`/agent [id|name]`**, que funcionan en el 100% de terminales.

### 6. `/rewind` dice que no puede ejecutarse mientras hay actividad
Por seguridad, `/rewind <tick>` en vivo requiere que ningún agente esté `thinking`, `acting`, `waiting_children` ni con una llamada LLM en vuelo. Espera a que el turno termine (emblema detenido / `Settled`), o sal con `Esc` y ejecuta `openonyx chat --resume <dir> --tick <n>`.

### 7. Windows PowerShell: comillas o redirecciones en `shell_exec`
`openonyx` informa al modelo del sistema operativo anfitrión (Windows PowerShell vs Unix/POSIX), pero si un modelo local insiste en usar sintaxis bash (`&&`, `grep`, `cat`), recuérdale en tu `ONYX.md` del proyecto que use herramientas nativas (`code_search`, `glob_files`, `fs_read`, `git_diff`) y comandos compatibles con PowerShell.

### 8. Corte inesperado durante una sesión larga
1. Ejecuta `openonyx doctor --snapshot-dir <dir>` para comprobar si quedó una cola truncada en `snapshots.onyxpack`.
2. Revisa los hitos intactos con `openonyx inspect <dir> --list`.
3. Reanuda con `openonyx chat --resume <dir>` (la cola truncada se ignora de forma segura al leer y se repara automáticamente al guardar el próximo hito).

### 9. `openonyx doctor` avisa o falla en un driver externo (`[[external_drivers]]`)
`doctor` audita las declaraciones de `[[external_drivers]]` (capítulo 03, sección 2.7):
- Preset integrado `opencode`: `[WARN]` si el binario `opencode` no está en `PATH` (el preset se omite).
- Driver declarado cuyo `bin` no existe: `[ERR]` — `/spawn --driver <id> will fail at runtime`.
- `bin` encontrado pero la sonda `--version` (deadline 3 s, entorno saneado) se queda a tiempo: `[ERR]`; si responde con error (p. ej., flags mal puestas): `[WARN]`.
- `prompt_transport = "arg"` en Windows: `[WARN]` si el binario resuelve a un wrapper `.cmd`/`.bat` (rechazado en el spawn).
- `network_allowlist` sin `--sandbox strict-container`: `[WARN]` — la allowlist es *advisory* fuera de `strict-container`; un driver declarado sin `network_allowlist` queda `LocalhostOnly` salvo que el sandbox lo relaje.

### 10. Un servidor MCP no se conecta o `[mcp.servers.*]` queda ignorado
`openonyx doctor` audita los servidores declarados en `[mcp.servers.<name>]` (capítulo 03, sección 2.8) con un handshake acotado a 5 s (un servidor caído nunca colga `doctor`):
- `Connected` o `Disabled`: `[CHECK]` informativo.
- `Failed(...)` (binario ausente, timeout del handshake, error del servidor): `[ERR]` accionable con el mensaje ya scrubbeado; la sesión sigue funcionando sin ese servidor (aislamiento de fallos).
- Workspace **no confiado** con `[mcp.servers.*]` de proyecto: `[WARN]` `MCP config in ... ignored: workspace is not trusted (use --trust-workspace or openonyx trust)` — aprueba con `openonyx trust` o usa `--trust-workspace`.
- Tools `mcp__*` siempre piden aprobación en `BUILD` y quedan `Deny` en `PLAN`/`EXPLORE`; si quieres auto-aprobarlas, declara `allow = ["mcp__<server>__*"]` en `.onyx/config.toml` (workspace confiado).

### 11. La Web (o la TUI) no acepta mi prompt: `409 Conflict` / "Paused in TIME FROZEN"
Es el guard de `TimeFreeze` (capítulo 04, sección 9): la sesión está **congelada** (breakpoint temporal activo) y un prompt plano desordenado rompería la bit-a-bit del replay. No es un fallo — son las vías de escape:
- `/go` (o `/unfreeze`): bifurca si el cursor mira el pasado y reanuda en el presente.
- `/inject <texto>` / `/graph …` / `/read …` / `/step [n]` / `/step-llm` / `! <cmd>`: siguen funcionando bajo freeze (primitivas 0-tokens selladas).
- En la Web, la barra ámbar (pulsar `#tick-pill`) tiene `Resume`; en la TUI, `Esc` vuelve al presente sin salir y `Ctrl+T` alterna el freeze.
- En `POST /api/v1/command` el 409 incluye el mensaje accionable completo; un `400` en `POST /api/v1/time/scrub` significa tick futuro/purgado o acción desconocida (el body lo explica).

---

[← 05. Herramientas, Scent, Sprout y Skills](05-herramientas-scent-sprout-y-skills.md) · [Volver al Índice del Manual](index.md)
