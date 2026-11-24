# 05. Herramientas del Agente, Scent, Sprout, Skills y Automatización

> 🇬🇧 **[Read this chapter in English](05-tools-scent-sprout-and-skills.md)**

Este capítulo recoge el catálogo completo de las **23 herramientas** disponibles para el agente dentro de `openonyx` (19 base + `spawn_agent` + 4 asíncronas de subagentes en worktrees Git aislados), cómo aprovechar la navegación AST (`onyx-scent`), cómo orquestar procesos y contenedores locales con `sprout.toml`, cómo personalizar el comportamiento mediante `ONYX.md` / skills y cómo ejecutar tareas en modo headless (`openonyx run`).

---

## 1. Catálogo Completo de las 23 Herramientas

### Herramientas de solo lectura y coordinación (sin aprobación)

| Herramienta | Propósito | Notas clave |
| --- | --- | --- |
| `fs_read` | Leer archivos UTF-8 (enteros o por rango 1-based `start_line`/`end_line`). | Límite `64 KiB`. Deduplica lecturas repetidas (`unchanged_since_tick`) y rangos solapados ya cubiertos (`covered_by:start-end`). Detecta y da aviso accionable ante artefactos binarios de OnYX (`ONYXTR1`/`2`, `ONYXPK1`, `ONYXAG`). |
| `fs_list` | Listar árbol de directorio hasta `max_depth` (default `3`). | Omite symlinks fuera de raíz, `.git`, `target`, `node_modules`, `*.tr`, `*.onyxpack`. |
| `glob_files` | Localizar rutas por patrón glob (`*`, `**`, `?`, `[...]`, `{a,b}`). | Orden lexicográfico determinista (o `sort_by_mtime`). Rechaza *path traversal*. |
| `code_search` | Búsqueda de texto literal o `regex` en el workspace. | Soporta `case_insensitive`, `include_glob`, `exclude_glob` y `context_lines` (`0..=5`) con `scrub_secrets`. |
| `scent_outline` | Mapa AST paginado (`@TOON v1`) de símbolos, visibilidad y rangos de líneas. | Soporta `Rust`, `Python` y `Java`; caché incremental `blake3` + caché de sesión `seen_outlines` invalidada automáticamente tras ediciones/shell/`/undo`; filtros `public_only`, `exclude_tests`, `cursor`, `max_rows` (`1..=500`). |
| `scent_impact` | Grafo de impacto semántico (difusión térmica entera `0..=10_000` bp, `1..=6` hops). | Indica qué símbolos y archivos se ven afectados antes de refactorizar un símbolo compartido. |
| `git_status` | Ejecuta `git status --short --branch` de solo lectura. | Úsala en lugar de `shell_exec` para comprobar cambios locales. |
| `git_diff` | Ejecuta `git diff --no-color` con `path`, `staged` y `context_lines` opcionales. | Pasa `path` estrictamente tras `--`. |
| `load_skill` | Carga bajo demanda una guía Markdown/skill por nombre. | Busca en `.onyx/skills/`, `skills/`, `~/.onyx/skills/` y las integradas `scent` y `rust`. |
| `plan_update` | Actualiza el checklist estructurado de progreso (`pending`, `in_progress`, `completed`). | Se persiste en `ws.plan.text` y se renderiza en el panel derecho `Live Plan`. |
| `service_status` | Consulta el estado `@TOON v1` de todos los servicios de `sprout.toml`. | Reflejado también en el panel derecho `Services (Sprout)`. |
| `service_logs` | Lee las últimas líneas del ring-buffer (hasta 2.000 líneas) de un servicio Sprout. | Filtrado con `scrub_secrets` y decodificación UTF-8 lossy. |
| `spawn_agent` | Delega una subtarea independiente a un subagente hijo (`WaitingChildren`). | Techo por defecto de `32` pasos (ajustable mediante `max_steps` en los argumentos JSON). |

### Herramientas mutantes (requieren aprobación en `BUILD`; bloqueadas en `PLAN`/`EXPLORE`)

| Herramienta | Propósito | Notas clave |
| --- | --- | --- |
| `fs_write` | Crear o sobrescribir un archivo UTF-8. | Captura checkpoint Git previo, muestra diff en color antes de aprobar, bloqueado en Control Plane RO. |
| `fs_edit` | Reemplazo exacto único (`old_text`/`new_text`) o multi-hunk atómico (`edits: [...]`). | Exige lectura previa con `fs_read`. Aplica *fallback* por líneas y re-indentado relativo en memoria antes de escribir. Captura checkpoint Git previo. |
| `shell_exec` | Ejecutar comando de shell dentro del workspace. | Timeout configurable (`--shell-timeout`, default `30s`), `env_clear()`, sandbox OS, `ProcessJob` RAII, `scrub_secrets`, límite `32 KiB` e invalidación de cachés `fs_read`/`scent_outline`. |
| `service_start` | Arranca un servicio (`id`) o todo el DAG (`all: true`) de `sprout.toml`. | Respeta `depends_on` en orden topológico y espera sondas reales (`ready_port`, `ready_log`, `ready_cmd`). |
| `service_stop` | Detiene un servicio (`id`) o todos (`all: true`). | Cierre limpio con `stop_cmd` / `ProcessJob`. |
| `service_exec` | Ejecuta un comando dentro de un contenedor activo (`docker exec` / `nerdctl exec` / `podman exec`). | Aplica `env_clear()`, timeout y `scrub_secrets`. |

### Herramientas asíncronas de subagentes (worktrees Git aislados)

Además de `spawn_agent` (síncrono, misma workspace), el agente puede lanzar jobs **asíncronos** que corren en worktrees Git aislados (rama `onyx/sub/<job_id>`) sin bloquear al agente raíz:

| Herramienta | Propósito | Notas clave |
| --- | --- | --- |
| `spawn_subagent` | Lanza un job asíncrono en un worktree Git aislado. | `goal` requerido; `max_steps` default `32` (máx `128`); `driver` opcional (`native` por defecto o id de `[[external_drivers]]` — un driver externo exige aprobación o `allow spawn_subagent(<driver>)`, capítulo 03). Paralelismo limitado por `--max-subagents <n>` (default `3`, o `ONYX_MAX_SUBAGENTS`). |
| `subagent_status` | Consulta el estado de los jobs asíncronos. | `job_id` opcional; muestra estado, rama, archivos tocados y resumen. |
| `subagent_merge` | Fusiona el worktree del job con la workspace raíz (3-way, *Scent-Heated Merge*). | `git merge-tree` **sin mover `HEAD`** ni el índice. Es la única de las 4 con `requires_approval: true`. Gobernanza del delta: `Deny` si toca Control Plane (bloquea el merge entero) y `Ask` si toca `sprout.toml` / `.github/workflows/**` (capítulo 03); driver externo fuerza `Ask`. |
| `subagent_discard` | Cancela el job y elimina su worktree y rama temporal. | Si el job está en marcha, cancela el árbol de procesos del `ProcessJob`. |

**Equivalencias slash en la TUI:** `/spawn [--driver <id>] <objetivo>` (lanza), `/subagents` (lista), `/merge <job_id>` (fusiona) y `/discard <job_id>` (descarta). Los logs completos de cada job (stdout/stderr del trabajo, sellados con `scrub_secrets`) se guardan en `.onyx/vault/subagent-logs/<job_id>.log`, y la vista `F2 Agents` (capítulo 02) muestra los jobs `[bg]` con su estado y badge `→ <driver>`.

> **Drivers externos:** con `driver = "opencode"` (o cualquier `[[external_drivers]]` declarado), el job se delega a un harness CLI externo que trabaja en el mismo worktree aislado; el aislamiento de proceso, el gate de Trust y la política de red se documentan en el capítulo 03 (sección 2.7).

---

## 2. Navegación AST con `onyx-scent`

En lugar de leer archivos de miles de líneas completos, `openonyx` anima al agente a trabajar en dos fases:
1. **`scent_outline`**: obtiene en pocos tokens todas las funciones, structs, traits, clases y métodos con sus números de línea exactos en formato compacto `@TOON v1` (paginado con `cursor` y `next_cursor`). Si el agente intenta repetir exactamente el mismo outline sobre un archivo que no ha cambiado, la caché `seen_outlines` devuelve una respuesta compacta ahorrando tokens y evitando bucles; en cuanto `fs_edit`, `fs_write`, `shell_exec` o `/undo` modifican el workspace, la caché se invalida automáticamente.
2. **`scent_impact`** + **`fs_read(start_line, end_line)`**: calcula el radio de impacto antes de tocar una firma pública y lee únicamente el bloque exacto necesario.

> **Lenguajes soportados actualmente por `onyx-scent`:** Rust (`.rs`), Python (`.py`) y Java (`.java`). En otros lenguajes el agente utiliza `glob_files` + `code_search` v2 + `fs_read`.

---

## 3. Orquestación Local de Procesos y Contenedores (`sprout.toml`)

Si tu proyecto necesita arrancar servicios locales (bases de datos en Docker/Rancher, APIs, workers, frontends) durante una sesión de desarrollo o testing, crea un archivo **`sprout.toml`** en la raíz del workspace.

`onyx-workspace` incluye un supervisor puro en `std` (**0 `tokio`**) que valida el grafo de dependencias (DAG sin ciclos), arranca servicios en orden topológico, comprueba sondas de disponibilidad reales y cierra todos los procesos limpiamente al salir de `openonyx` (`ProcessJob` RAII).

### Formato moderno (`[runtime]` + `[services.<id>]`)

```toml
[runtime]
# Opcional: "docker" | "nerdctl" | "podman" (si se omite, se autodetecta)
container_cli = "docker"

[services.db]
name = "Postgres local"
kind = "container"
cmd = "docker compose up -d postgres"
stop_cmd = "docker compose stop postgres"
logs_cmd = "docker compose logs --tail=100 postgres"
container = "myapp-postgres"
# Opcional: fuerza usuario en `service_exec` (ej. "1000:1000" o "app")
# exec_user = "1000:1000"
ready_port = 5432

[services.api]
name = "API Backend"
kind = "process"
cwd = "backend"
cmd = "cargo run --bin api"
depends_on = ["db"]
ready_port = 8080
ready_log = "Listening on 127.0.0.1:8080"
```

También se admite el formato clásico de Sprout (`[global]` + `[[service]]` con `env_wrapper`, `dir`, `debug_port`, `depends_on`, `optional`, `enabled`).

### Control interactivo desde la TUI
- `/services`: imprime en `Memory & Trace` la tabla `@TOON v1` con estado de cada servicio.
- `/service start <id|all>`: arranca un servicio (y sus dependencias) o todo el stack.
- `/service stop <id|all>`: detiene un servicio o todos.
- `/service logs <id> [tail]`: muestra las últimas `[tail]` líneas del buffer de logs.
- Además, con terminal `>= 100` columnas verás el estado en tiempo real en el panel derecho **`Services (Sprout)`**, y `openonyx doctor` validará tu `sprout.toml` (sintaxis, ciclos en DAG, runtime de contenedores y puertos ya ocupados).

> **Nota sobre red, confianza y contenedores:** si el agente necesita consultar con `shell_exec` una API local levantada por Sprout (p. ej. `http://127.0.0.1:8080/health`), ejecuta `openonyx` con `--sandbox-net localhost` (o configúralo en `.onyx/config.toml`). Además, si el repositorio no está en tu almacén de **Workspace Trust** (`openonyx trust add` o `/trust`), Sprout muestra `[UNTRUSTED — run /trust]` y bloquea `start`, `stop` y `exec` hasta que confíes en el `sprout.toml` actual (cualquier cambio de 1 byte revoca la confianza automáticamente). `container_cli` solo admite `docker`, `nerdctl` o `podman`, y el campo opcional `exec_user` permite ejecutar `service_exec` con un usuario concreto.

---

## 4. Instrucciones de Proyecto (`ONYX.md` / `AGENTS.md`)

En cada inicio (`chat`, `run` y `rewind` vivo), `openonyx` carga automáticamente instrucciones Markdown en este orden (hasta `16 KiB` totales, siempre pasadas por `scrub_secrets`):

1. `~/.onyx/AGENTS.md` y `~/.onyx/ONYX.md` *(reglas globales personales)*
2. `<workspace>/AGENTS.md` y `<workspace>/ONYX.md` *(reglas del repositorio)*

Además, inyecta automáticamente una pista del sistema operativo anfitrión (Windows PowerShell vs Unix/POSIX) para evitar comandos incompatibles en `shell_exec`.

Ejemplo útil de `<workspace>/ONYX.md`:
```markdown
# Convenciones del proyecto
- Ejecuta siempre `cargo test --workspace` tras editar código Rust.
- No modifiques archivos generados bajo `dist/`.
- Escribe los mensajes de error orientados al usuario en español.
```

---

## 5. Crear tus Propias Skills (`SKILL.md`)

En lugar de inflar `ONYX.md` con manuales largos que se envían en cada turno, crea **skills bajo demanda**. Al arrancar, `openonyx` solo inyecta un catálogo compacto (`discover_skills_summary`), y el agente carga el texto completo con `load_skill("<nombre>")` únicamente cuando lo necesita.

### Orden de búsqueda de skills
1. `<workspace>/.onyx/skills/<nombre>/SKILL.md` *(recomendado por proyecto)* o `<nombre>.md`
2. `<workspace>/skills/<nombre>/SKILL.md` o `<nombre>.md`
3. `~/.onyx/skills/<nombre>/SKILL.md` o `<nombre>.md` *(globales de usuario)*
4. Skills integradas: `scent` y `rust`

### Plantilla de una skill personalizada
Crea `.onyx/skills/deploy-checks/SKILL.md`:

```markdown
---
name: deploy-checks
description: Checklist de verificación previa a despliegue y migraciones SQL seguras.
---

# Verificación Pre-Despliegue

1. Comprueba con `git_diff` que no haya migraciones destructivas sin plan de rollback.
2. Verifica que `cargo clippy --workspace --all-targets -- -D warnings` pase sin avisos.
3. Revisa que no queden claves ni URLs de staging en archivos de configuración.
```

Buenas prácticas:
- Incluye `name:` y `description:` en las primeras 20 líneas (el catálogo usa `description:`).
- Nunca escribas secretos reales (todo pasa por `scrub_secrets`).
- Mantén cada skill muy por debajo de `64 KiB`.

---

## 6. Automatización Headless y CI (`openonyx run`)

Para ejecutar una tarea única sin interfaz TUI:

```sh
# Auditoría de solo lectura sin coste humano
openonyx run "revisa git_diff y enumera posibles regresiones de seguridad" \
  --effort medium \
  --max-steps 32 \
  --record ci_review.tr \
  --snapshot-dir .snaps_ci

# Ejecución automatizada acotada con sandbox estricto
openonyx run "corrige los avisos de clippy y valida con cargo test" \
  --auto-approve \
  --sandbox workspace-write \
  --sandbox-net offline \
  --shell-timeout 120 \
  --record ci_fix.tr \
  --snapshot-dir .snaps_fix
```

Recuerda que incluso con `--auto-approve`, cualquier regla `[permissions].deny` de `.onyx/config.toml` y la protección RO del Control Plane siguen bloqueando acciones prohibidas.

---

[← 04. Time-Travel, Snapshots y Forking](04-time-travel-snapshots-y-forking.md) · [Índice](index.md) · [Siguiente: 06. Referencia CLI y Troubleshooting →](06-referencia-cli-y-troubleshooting.md)
