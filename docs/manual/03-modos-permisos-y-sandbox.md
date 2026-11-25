# 03. Modos de Trabajo, Permisos y Sandbox OS

> 🇬🇧 **[Read this chapter in English](03-modes-permissions-and-sandbox.md)**

`openonyx` aplica **defensa en profundidad** sobre todo lo que hace el agente: modos de workspace, reglas declarativas `.onyx/config.toml`, protección del *Control Plane*, sanitización de entorno (`env_clear()`), aislamiento a nivel de sistema operativo y supervisión RAII de procesos.

---

## 1. Modos de Workspace: `PLAN`, `BUILD` y `EXPLORE`

El modo activo (`ws.mode`) se almacena dentro del grafo EAV y se graba en las transcripciones `ONYXTR2`, de modo que `rewind` y `replay` lo restauran exactamente.

| Modo | Lectura / Búsqueda / AST | Mutaciones (`fs_write`, `fs_edit`, `shell_exec`, `service_start/stop/exec`) | Uso recomendado |
| --- | --- | --- | --- |
| **`BUILD`** *(default)* | Permitido | Permitido con aprobación (o según reglas `.onyx/config.toml`). `fs_edit` exige lectura previa del archivo con `fs_read`. | Implementación, refactorización, ejecución de tests. |
| **`PLAN`** | Permitido | **Bloqueado incondicionalmente** (incluso con `allow` en `config.toml` o `--auto-approve`). | Análisis de arquitectura, diseño de planes, revisión de PRs. |
| **`EXPLORE`** | Permitido | **Bloqueado incondicionalmente**. | Exploración rápida de solo lectura en repositorios desconocidos. |

> **Tools MCP dinámicas:** aunque un tool `mcp__<server>__<tool>` no mutee el filesystem local, un servidor MCP remoto puede ejecutar procesos o red fuera del control de `openonyx`; por eso se trata igual que una mutación y queda **`Deny` en `PLAN`/`EXPLORE`** (capítulo 03, sección 2.8).

### Cómo cambiar de modo en `chat`
- Pulsa **`Tab`** para alternar en caliente entre `[BUILD]` y `[PLAN]`.
- O usa el comando slash explícito: `/mode plan`, `/mode build`, `/mode explore` (o `/mode` sin argumentos para alternar `PLAN` ↔ `BUILD`).

---

## 2. Permisos Declarativos (`.onyx/config.toml`)

Puedes definir reglas granulares en dos ubicaciones que se cargan y fusionan automáticamente:

1. **Global de usuario**: `~/.onyx/config.toml`
2. **Por proyecto**: `<workspace>/.onyx/config.toml`

> **Regla de seguridad:** un proyecto **nunca puede eliminar** una regla `deny` definida en tu `~/.onyx/config.toml` global.

### Precedencia: `deny > ask > allow`
- **`deny`**: bloquea la herramienta de forma incondicional, incluso si ejecutas `openonyx` con `--auto-approve`. Solo puede omitirse pasando explícitamente `--ignore-deny`.
- **`ask`**: fuerza confirmación interactiva (`y`/`n`), prevaleciendo sobre `allow` a igual o mayor especificidad.
- **`allow`**: auto-aprueba una herramienta/patrón específico sin interrumpirte (siempre que estés en modo `BUILD`; en `PLAN`/`EXPLORE` las mutaciones siguen bloqueadas). Un `allow` más específico (p. ej. `shell_exec(cargo test*)`) vence a un comodín genérico `ask` (`shell_exec(*)`).

### Ejemplo completo de `.onyx/config.toml`

```toml
[permissions]
# Bloqueos absolutos (ni siquiera --auto-approve los salta)
deny = [
  "shell_exec(rm -rf*)",
  "shell_exec(git push*)",
  "shell_exec(curl*)",
  "fs_write(.env*)",
]

# Auto-aprobar comandos de verificación seguros en modo BUILD
allow = [
  "shell_exec(cargo check*)",
  "shell_exec(cargo test*)",
  "shell_exec(cargo clippy*)",
  "shell_exec(cargo fmt --all -- --check)",
]

# Requerir confirmación explícita para cualquier otro shell_exec o escritura
ask = [
  "shell_exec(*)",
  "fs_write(*)",
  "fs_edit(*)",
]

[sandbox]
# "workspace-write" (default) | "strict-container" | "off"
mode = "workspace-write"
# "offline" (default) | "localhost" | "unrestricted"
network = "offline"
# Variables extra permitidas tras env_clear() (nunca pases API keys)
env_passthrough = ["RUST_LOG", "RUST_BACKTRACE"]
# Rutas adicionales de solo lectura fuera del workspace (opcional)
allow_read_paths = []
# Límites de recursos para subprocesos
max_memory_mb = 4096
max_pids = 128
# Imagen OCI cuando mode = "strict-container" (opcional)
# container_image = "rust:1.85-slim"

[impact]
# Scent-Heated Approval: escala Allow -> Ask si un diff toca símbolos públicos con callers
enabled = true
max_hops = 2          # acotado 1..=4
ask_heat_bp = 2500    # umbral para símbolos pub scope:src (basis points 0..=10000)
hard_ask_bp = 8000    # umbral duro para cualquier símbolo con alto fan-out
```

> **Monotonía de *Scent-Heated Approval*:** `[impact]` solo puede **subir** una decisión `Allow → Ask` y mostrar la línea `# impact: N callers, H hops, scope:src(pub) [heat Xbp]` sobre el diff en TUI/CLI. Nunca anula un `deny`, nunca baja un `ask` a `allow`, y degrada limpiamente a política estática si el archivo no está indexado o `enabled = false`.

---

## 2.5. Workspace Trust (`~/.onyx/trusted_workspaces.toml`)

Para proteger frente a repositorios clonados o PRs externos con configuraciones maliciosas (*Indirect Prompt Injection* + `allow = ["shell_exec(*)"]` o `sprout.toml` arbitrario), `openOnyx` fija los hashes `blake3` de `.onyx/config.toml` y `sprout.toml` por workspace en `~/.onyx/trusted_workspaces.toml`:

- **Repositorio limpio (sin `.onyx/config.toml` ni `sprout.toml`):** sin fricción (`no-project-config`).
- **Repositorio no confiado o modificado (`untrusted` / `modified`, badge `[UNTRUSTED]` en TUI):**
  - `.onyx/config.toml` del proyecto **solo puede restringir** (`deny`/`ask`, sandbox/impact más estrictos); se ignoran sus reglas `allow`, `env_passthrough`, `allow_read_paths`, `container_image` y cualquier relajación de `[sandbox]`/`[impact]`.
  - `sprout.toml` bloquea la ejecución de servicios (`service_start`, `service_exec`, `stop_cmd`) hasta aprobación explícita.
- **Comandos de confianza:** `openonyx trust` (aprueba el workspace actual), `openonyx trust check`, `openonyx trust list`, `openonyx trust revoke`, flag de sesión `--trust-workspace` o `/trust [status|revoke]` en la TUI.

---

## 2.6. Perfiles LLM (`[[llm_profiles]]`)

El objetivo LLM (proveedor, modelo, `api_base`, esfuerzo) puede declararse en `.onyx/config.toml` como bloques `[[llm_profiles]]` y conmutarse en caliente con `/profile` / `/provider` / `/model` / `/effort` (capítulo 02, sección 2.1):

```toml
[[llm_profiles]]
id = "local-ollama"
provider = "openai"
model = "llama3.1:70b"
effort = "medium"
api_base = "http://localhost:11434/v1"
# api_key_env = "OLLAMA_API_KEY"
```

- **Campos:** `id` (único), `provider` (`gemini` | `openai` | `anthropic`), `model`, `effort` (`auto` | `low` | `medium` | `high`), `api_base` opcional y `api_key_env` opcional. Claves desconocidas o duplicadas son error de parse accionable.
- **Nunca claves literales:** un `api_key` literal en el bloque es **rechazado** por el parser con un error que indica usar `api_key_env` (y el mensaje no repite el valor de la clave). Las credenciales siempre se resuelven de variables de entorno (`api_key_env` → variable estándar del proveedor → `ONYX_API_KEY`).
- **`api_base` validado al registro:** con la misma política que `--api-base` (`http://` solo en loopback; hosts remotos exigen `https://`; hosts no oficiales requieren `--allow-custom-auth-host`).
- **Gate de Trust:** un workspace **no confiado** ignora sus bloques `[[llm_profiles]]` y muestra la advertencia `ignored N [[llm_profiles]] block(s); run openonyx trust or /trust to approve`. Con workspace confiado, la precedencia es global (`~/.onyx/config.toml`) < proyecto (fusion *replace-by-id*).
- **Disponibilidad:** un perfil sin clave resuelta o con `api_base` rechazada se lista como `unavailable` con motivo accionable (`/profile`, paleta `Ctrl+K` y selector Web). La TUI y la Web solo exponen metadatos (`public_snapshot`): proveedor, modelo, esfuerzo y motivo — **nunca** claves.

---

## 2.7. Harnesses CLI externos (`[[external_drivers]]`)

Los jobs asíncronos `spawn_subagent` / `/spawn` (capítulo 05) pueden ejecutarse fuera del motor OnYX, delegando el trabajo a un harness CLI externo declarado en `.onyx/config.toml`:

```toml
[[external_drivers]]
id = "opencode"
bin = "opencode"
args = ["run", "--model", "qwen38-27b"]
# prompt_transport = "stdin"        # "stdin" (default) | "arg" | "temp_file"
# prompt_flag = "--prompt-file"     # para "temp_file" (o placeholder {prompt_file} en args)
timeout_secs = 600
allowed_env = []
# inherit_home = true               # si el CLI necesita config/OAuth del HOME real
# network_allowlist = ["api.ejemplo.com"]
```

- **Campos:** `id` (charset `[a-z0-9_-]`), `bin` (resuelto en `PATH`/`PATHEXT`), `args` (argumentos fijos), `prompt_transport` (`stdin` recomendado y default; `arg` rechazado en Windows si el binario resuelve a un wrapper `.cmd`/`.bat`; `temp_file` para prompts grandes, fuera del índice Git), `prompt_flag`, `timeout_secs`, `allowed_env` (whitelist de variables para el hijo) e `inherit_home` (default `false`).
- **Gate de Trust:** un workspace **no confiado** no puede lanzar drivers externos (`Deny` absoluto: `requires a trusted workspace`).
- **Aprobación por defecto `Ask`:** `spawn_subagent` con driver externo **nunca** auto-aproba: sin reglas `ask`/`allow` explícitas la decisión es `Ask`; puedes fijarla con `allow = ["spawn_subagent(opencode)"]` o `ask`/`deny` con el mismo patrón (un `deny spawn_subagent(opencode)` bloquea ese driver aunque haya `allow`).
- **Aislamiento del proceso:** el prompt se entrega sin inyección de shell (stdin/argv único/archivo temporal); el entorno del hijo pasa por `env_clear()` + `allowed_env`; el proceso queda asignado a un `ProcessJob` desde el `spawn` (si `timeout_secs` vence o se cancela el job, muere el árbol entero; en Windows hay fallback `taskkill /F /T /PID`); y el log completo se sella en `.onyx/vault/subagent-logs/<job_id>.log` pasando por `scrub_secrets`.
- **Red declarativa (`network_allowlist`):** es una gobernanza **declarativa**: con `--sandbox strict-container` se enforce en el contenedor; en los demás perfiles es *advisory* (el `doctor` lo avisa). Un allowlist vacío equivale a `LocalhostOnly`; con allowlist, el sandbox **no** inyecta variables `*_OFFLINE` (el harness necesita llegar a su API).
- **Merge forzadamente `Ask`:** aunque tus permisos auto-aprobieran, `subagent_merge` de un job con driver externo exige aprobación explícita (`ExternalDriverApprovalPolicy`).

---

## 2.8. Servidores MCP (`[mcp.servers.<name>]`)

`openonyx` puede conectar **servidores Model Context Protocol (MCP)** declarados en `config.toml` (global `~/.onyx/config.toml` y, con workspace confiado, `<workspace>/.onyx/config.toml`):

```toml
[mcp.servers.github]
transport = "stdio"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-github"]
# env = { GITHUB_TOKEN = "${GITHUB_TOKEN}" }   # referencias ${VAR} al lanzar el proceso
timeout_secs = 20            # default 20
# enabled = true             # default true

[mcp.servers.docs]
transport = "http"
url = "https://mcp.ejemplo.com/sse"
# headers = { Authorization = "Bearer ${MCP_DOCS_TOKEN}" }
```

- **Campos:** `transport` (`"stdio"` con `command`/`args`/`env`, o `"http"` con `url`/`headers`), `timeout_secs` (default `20` s por llamada JSON-RPC) y `enabled` (default `true`). Los nombres de servidor no pueden contener `__` (reservado para el prefijo `mcp__<server>__<tool>`). Las referencias `${ENV_VAR}` se expanden **solo** al lanzar la request/proceso (nunca se loguean ni persisten) y `Authorization`/`X-Api-Key` en `headers` **exigen** `${VAR}`.
- **Fusión y gate de Trust:** fusión *replace-by-name* con precedencia `global < workspace`; un workspace **no confiado** ignora sus `[mcp.servers.*]` con la advertencia `MCP config in ... ignored: workspace is not trusted (use --trust-workspace or openonyx trust)`, mientras que la configuración global siempre aplica.
- **Naming y aprobación:** cada tool descubierta (`tools/list`) se expone al LLM como **`mcp__<server>__<tool>`** con `requires_approval = true`: **siempre** pide aprobación interactiva en `BUILD`, salvo que una regla `allow` más específica diga lo contrario (p. ej. `allow = ["mcp__github__search_repos"]` o `allow = ["mcp__github__*"]`); `ask`/`deny` con glob también funcionan (`deny = ["mcp__*"]` bloquea todo MCP).
- **`PLAN`/`EXPLORE` = `Deny`:** en esos modos cualquier `mcp__*` queda bloqueado **incondicionalmente** (un servidor remoto ejecuta procesos/red fuera del sandbox).
- **Aislamiento de fallos:** un servidor caído no aborta la sesión: se reporta `[openOnyx mcp] server \`<name>\` unavailable: ...` y el resto de la sesión sigue; cada servidor expone su estado para `openonyx doctor`.
- **Seguridad de la sesión stdio:** JSON-RPC 2.0 con hilo lector dedicado, timeout real por llamada (`mpsc::recv_timeout`), línea de `stdout` acotada a 4 MiB y limpieza del árbol de procesos con `ProcessJob` al fallar (Windows y Unix).
- **Salidas saneadas:** los resultados de `tools/call` y los errores de descubrimiento pasan por `truncate_utf8` + `scrub_secrets` antes de llegar al LLM o a la transcripción.
- **`openonyx doctor`:** audita cada servidor declarado con un handshake **acotado a 5 s** (`min(timeout_secs, 5)`): `Connected`/`Disabled` → `[CHECK]`, fallo → `[ERR]` accionable (mensaje ya scrubbeado), workspace no confiado → `[WARN]` sin descubrimiento de workspace.
- **`replay` 0-I/O:** al reproducir una transcripción `ONYXTR2` no se contacta con ningún servidor: los tools `mcp__*` se reconstruyen desde la propia transcripción (`extract_mcp_tools_from_transcript`), sin `load_mcp_registry`.

---

## 3. Protección Read-Only del Control Plane

Incluso en modo `BUILD` y con `--auto-approve`, `onyx-workspace` protege como **solo lectura** los archivos críticos del plano de control del agente y del repositorio:

- `.onyx/config.toml` (evita que un agente se auto-conceda permisos o desactive el sandbox).
- `.onyx/vault/**` (evita que un agente borre o corrompa los snapshots y transcripciones de la bóveda de sesiones).
- `.git/hooks/**` y `.git/config` (evita inyección de código al ejecutar comandos Git).
- `.env` y `.env.*` (evita exfiltración o corrupción de secretos locales).

Esta protección se aplica tanto en `fs_write` / `fs_edit` como en inspecciones de escritura sobre `shell_exec`.

### Gobernanza del delta de `subagent_merge` (3-way merge)

Cuando un job asíncrono (`spawn_subagent` / `/spawn`) se fusiona con `subagent_merge` / `/merge`, el delta `<root>..<merged_tree>` se clasifica **antes de tocar disco**:

- **`Deny` — Control Plane**: si el delta toca `.onyx/config.toml`, `.onyx/vault/**`, `.git/hooks/**`, `.git/config` o `.env*`, el merge se **bloquea entero** (un solo archivo bloquea todo el delta; nunca se aplica parcialmente).
- **`Ask` — superficies de gobernanza**: si el delta toca `sprout.toml` o `.github/workflows/**`, la aprobación del merge debe ser **explícita** (nunca auto-aprobada), aunque tus reglas `allow` cubran otras herramientas.
- **`Safe`**: el resto del delta sigue la aprobación estándar por impacto (*Scent-Heated Approval*).

La fusión es 3-way con `git merge-tree` **sin mover `HEAD` ni el índice del workspace**; el veredicto se aplica sobre el delta completo.

---

## 4. Sandbox OS, `env_clear()` y `ProcessJob` RAII

Toda ejecución de `shell_exec` (y los procesos locales de Sprout) pasa por la infraestructura compartida de `onyx-workspace/src/sandbox.rs`.

### Perfiles de sandbox (`--sandbox`)
| Modo | Badge TUI | Comportamiento |
| --- | --- | --- |
| `workspace-write` *(default)* | `[SANDBOX:workspace]` (Linux `bwrap` / macOS `sandbox-exec`), `[SANDBOX:env+job]` (Windows) o `[SANDBOX:env+pgid]` | `env_clear()`, aislamiento nativo de escritura (en Linux/macOS), Win32 Job Objects / PGID, límites de memoria/PIDs y red `offline` por defecto. En Windows, usa `strict-container` para aislamiento fuerte de FS/red. |
| `strict-container` | `[SANDBOX:strict]` | Ejecuta el comando dentro de un contenedor OCI (`docker`, `nerdctl` o `podman`) montando el workspace. Ideal para automatización desatendida. |
| `off` | `[SANDBOX:off]` | Desactiva el wrapper de sandbox OS (siguen activos `WorkspacePolicy`, timeout, limpieza de procesos y Control Plane RO). |

### Política de red (`--sandbox-net`)
| Política | Comportamiento |
| --- | --- |
| `offline` *(default)* | Bloquea tráfico de red saliente y comandos de red antes de lanzar el proceso, e inyecta variables offline (`CARGO_NET_OFFLINE`, `PIP_NO_INDEX`, `NPM_CONFIG_OFFLINE`). |
| `localhost` | Permite tráfico local (`127.0.0.1` / `localhost`), útil para probar APIs o bases de datos levantadas con `sprout.toml`. |
| `unrestricted` | Sin restricción de red saliente (úsalo solo cuando necesites descargar dependencias). |

### Elevación automática con `--auto-approve`
Si pasas `--auto-approve` y tu configuración tenía `mode = "off"`, `openonyx` **eleva automáticamente** el sandbox a `workspace-write` + `offline` para reducir el radio de impacto, salvo que pases explícitamente `--sandbox off` en la línea de comandos.

### Sanitización de entorno (`env_clear()`)
Antes de arrancar un subproceso:
1. Se limpia el entorno con `env_clear()` y se reconstruye solo una lista segura de variables esenciales del sistema (`PATH` limpio, `HOME`/`USERPROFILE`, toolchains estándar).
2. Se eliminan **todas** las variables sensibles (`*_API_KEY`, `*_TOKEN`, `AWS_*`, etc.), evitando que un comando en `shell_exec` pueda leer tu `GEMINI_API_KEY` o `OPENAI_API_KEY`.
3. `TMP`/`TEMP`/`TMPDIR` se redirigen a un directorio temporal propio de la sesión.
4. Además, toda la salida capturada pasa por `scrub_secrets` antes de volver al modelo o grabarse en transcripciones.

### Comportamiento por sistema operativo y `ProcessJob` RAII
- **Windows (`win32-job`)**: cada comando se envuelve en un **Win32 Job Object** con `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` y límites de memoria/PIDs. Si vence `--shell-timeout` o cierras `openonyx`, el árbol entero de procesos hijos muere al instante sin dejar huérfanos.
- **Linux (`bwrap` / `landlock` + `PGID`)**: usa **Bubblewrap (`bwrap`)** cuando está disponible y pasa una sonda real en vivo (`bwrap_is_usable()`). En distribuciones como **Ubuntu 24.04** donde AppArmor bloquea *unprivileged user namespaces*, `openonyx` detecta el bloqueo automáticamente y aplica *fallback* seguro sin romper `shell_exec`, manteniendo `env_clear()`, control offline, Control Plane RO y limpieza de grupos de procesos POSIX (`PGID`).
- **macOS (`sandbox-exec` + `PGID`)**: utiliza perfiles `sandbox-exec` junto con `env_clear()` y limpieza por grupo de procesos POSIX.

Para comprobar qué driver nativo y qué runtime de contenedores (`docker`/`nerdctl`/`podman`) detecta tu máquina, ejecuta:

```sh
openonyx doctor
```

---

[← 02. Interfaz TUI y Flujo Diario](02-interfaz-tui-y-flujo-diario.md) · [Índice](index.md) · [Siguiente: 04. Time-Travel, Snapshots y Forking →](04-time-travel-snapshots-y-forking.md)
