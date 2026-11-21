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

## 3. Protección Read-Only del Control Plane

Incluso en modo `BUILD` y con `--auto-approve`, `onyx-workspace` protege como **solo lectura** los archivos críticos del plano de control del agente y del repositorio:

- `.onyx/config.toml` (evita que un agente se auto-conceda permisos o desactive el sandbox).
- `.onyx/vault/**` (evita que un agente borre o corrompa los snapshots y transcripciones de la bóveda de sesiones).
- `.git/hooks/**` y `.git/config` (evita inyección de código al ejecutar comandos Git).
- `.env` y `.env.*` (evita exfiltración o corrupción de secretos locales).

Esta protección se aplica tanto en `fs_write` / `fs_edit` como en inspecciones de escritura sobre `shell_exec`.

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
