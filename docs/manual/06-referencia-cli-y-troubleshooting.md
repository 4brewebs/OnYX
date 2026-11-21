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
openonyx doctor [--workspace <dir>] [--snapshot-dir <dir>] [--transcript <file>]
openonyx [--help | -h | --version | -V]
```

---

## 2. Tabla Completa de Opciones (Flags)

| Flag | Aplica a | Descripción / Default |
| --- | --- | --- |
| `-w, --workspace <dir>` | Todos | Directorio del proyecto objetivo (default: directorio actual). |
| `--provider <gemini\|openai>` | `chat`, `run`, `rewind` | Backend LLM (`gemini` por defecto; `openai` para OpenAI/OpenRouter/Ollama/vLLM/LM Studio). |
| `--model <name>` | `chat`, `run`, `rewind` | Nombre del modelo (defaults: `gemini-4-hydrogen` en Gemini, `gpt-4o` en OpenAI). |
| `--effort <low\|medium\|high>` | `chat`, `run`, `rewind` | Esfuerzo de razonamiento/pensamiento (`thinkingLevel` / `reasoning_effort`). |
| `--api-base <url>` | `chat`, `run`, `rewind` | URL base personalizada (p. ej. `http://localhost:11434/v1`; exime de `OPENAI_API_KEY`). |
| `--sandbox <off\|workspace-write\|strict-container>` | `chat`, `run`, `rewind` | Perfil de sandbox OS para `shell_exec` (default: `workspace-write`). |
| `--sandbox-net <offline\|localhost\|unrestricted>` | `chat`, `run`, `rewind` | Política de red del sandbox (default: `offline`). |
| `--auto-approve` | `chat`, `run`, `rewind` | Omite confirmaciones interactivas para herramientas mutantes (`deny` sigue aplicando y eleva sandbox `off` → `workspace-write` + `offline` salvo `--sandbox off`). |
| `--ignore-deny` | `chat`, `run`, `rewind` | Desactiva explícitamente las reglas `[permissions].deny` de `.onyx/config.toml`. |
| `--max-steps <n>` | `chat`, `run` | Límite de pasos por turno del agente raíz (default: `128` en `chat`, `64` en `run`; subagentes `32` por defecto). |
| `--shell-timeout <secs>` | `chat`, `run` | Timeout en segundos por comando `shell_exec` (default: `30s`). |
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
| `-h, --help` / `-V, --version` | Global | Muestra ayuda o versión y sale con código `0`. |

---

## 3. Variables de Entorno

| Variable | Proveedor | Descripción |
| --- | --- | --- |
| `ONYX_PROVIDER` | Todos | Proveedor por defecto (`gemini` \| `openai`). |
| `GEMINI_API_KEY` | Gemini | API key de Google AI Studio (fallback: `ONYX_API_KEY`). |
| `OPENAI_API_KEY` | OpenAI | API key para `--provider openai` (fallback: `ONYX_API_KEY`; opcional con `--api-base` local). |
| `ONYX_API_KEY` | Ambos | Clave compartida de fallback si no se define la específica del proveedor. |
| `ONYX_MODEL` | Ambos | Modelo por defecto (fallbacks: `ONYX_GEMINI_MODEL` para Gemini, `OPENAI_MODEL` para OpenAI). |
| `ONYX_EFFORT` | Ambos | Esfuerzo por defecto (`low` \| `medium` \| `high`; fallbacks: `GEMINI_THINKING_LEVEL`, `OPENAI_REASONING_EFFORT`). |
| `ONYX_AUTO_SAVE` | Ambos | Activa `--auto-save` por defecto (`1` \| `true` \| `yes` \| `on`) guardando en `.onyx/vault/`. |
| `GEMINI_API_BASE` | Gemini | Endpoint personalizado para Gemini. |
| `OPENAI_API_BASE` / `OPENAI_BASE_URL` | OpenAI | Endpoint compatible con OpenAI (default: `https://api.openai.com/v1`). |

---

## 4. Resiliencia HTTP/SSE en Endpoints OpenAI, vLLM y Proxies

En servidores locales o proxies corporativos (vLLM, LiteLLM, nginx, Cloudflare) es frecuente que la conexión *chunked* SSE se cierre bruscamente tras emitir `finish_reason` o que devuelva un error transitorio (`408`, `429`, `500`, `502`, `503`, `504` o corte temporal de red).

El adaptador `open-onyx/src/openai.rs` incorpora dos protecciones automáticas:
1. **Tolerancia a EOF tras respuesta completada**: si el stream SSE falla al decodificar el último chunk pero ya se recibió `finish_reason` / `[DONE]` con respuesta o `tool_calls` completos, la respuesta se acepta sin abortar el turno.
2. **Reintentos automáticos con backoff**: ante fallos HTTP o de transporte transitorios, reintenta automáticamente hasta **2 veces** (`400ms`, `1200ms`) antes de reportar error al agente.

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
Algunos emuladores de terminal (especialmente en Windows o a través de tmux/SSH) interceptan o no envían `Ctrl+Tab`. Usa siempre **`F2`** (popup interactivo **Agent Selector**) o el comando **`/agent [id|name]`**, que funcionan en el 100% de terminales.

### 6. `/rewind` dice que no puede ejecutarse mientras hay actividad
Por seguridad, `/rewind <tick>` en vivo requiere que ningún agente esté `thinking`, `acting`, `waiting_children` ni con una llamada LLM en vuelo. Espera a que el turno termine (emblema detenido / `Settled`), o sal con `Esc` y ejecuta `openonyx chat --resume <dir> --tick <n>`.

### 7. Windows PowerShell: comillas o redirecciones en `shell_exec`
`openonyx` informa al modelo del sistema operativo anfitrión (Windows PowerShell vs Unix/POSIX), pero si un modelo local insiste en usar sintaxis bash (`&&`, `grep`, `cat`), recuérdale en tu `ONYX.md` del proyecto que use herramientas nativas (`code_search`, `glob_files`, `fs_read`, `git_diff`) y comandos compatibles con PowerShell.

### 8. Corte inesperado durante una sesión larga
1. Ejecuta `openonyx doctor --snapshot-dir <dir>` para comprobar si quedó una cola truncada en `snapshots.onyxpack`.
2. Revisa los hitos intactos con `openonyx inspect <dir> --list`.
3. Reanuda con `openonyx chat --resume <dir>` (la cola truncada se ignora de forma segura al leer y se repara automáticamente al guardar el próximo hito).

---

[← 05. Herramientas, Scent, Sprout y Skills](05-herramientas-scent-sprout-y-skills.md) · [Volver al Índice del Manual](index.md)
