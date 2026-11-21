# 02. Interfaz TUI y Flujo de Trabajo Diario

Este capítulo detalla cada componente visual de `openonyx chat`, cómo controlar el esfuerzo de razonamiento (`--effort`), cómo funciona la memoria multi-turno y el contexto escalonado (`/compact`), y cómo supervisar subagentes con `F2` / `/agent`.

---

## 1. Anatomía de la TUI

Al ejecutar `openonyx chat`, la pantalla se organiza en cuatro zonas bien definidas:

```text
┌─ openOnyx · gemini-2.5-pro · effort:high ───────────────────────────────────────────────┐
│ ⠋ [BUILD] [SANDBOX:workspace] · Agent #1 root (thinking) · tick 42 · Tab: Plan/Build … │
└─ ctx: 24% · in: 18.4k (cache 12.1k) · out: 1.9k · think: 0.6k · ~$0.024 ────────────────┘
┌─ Memory & Trace (Agent #1 root) ───────────────────────┐┌─ Live Plan ───────────────────┐
│ [user] Refactoriza el parser...                        ││ [x] Inspeccionar AST          │
│ [call] scent_outline {"path":"src/parser.rs"}          ││ [>] Editar parser.rs          │
│ [obs]  @TOON v1 ...                                    ││ [ ] Ejecutar cargo test       │
│ [stream] Voy a ajustar la validación de rangos...▍     │├───────────────────────────────┤
│                                                        ││ Services (Sprout)             │
│                                                        ││ [READY] postgres (5432)       │
│                                                        ││ [READY] api      (8080)       │
└────────────────────────────────────────────────────────┘└───────────────────────────────┘
┌─ [BUILD] Goal / Command (Enter send · Tab Plan/Build · F2 Agents · /rewind · /fork …) ──┐
│ > _                                                                                     │
└─────────────────────────────────────────────────────────────────────────────────────────┘
```

### Cabecera compacta de 3 filas
- **Borde superior**: muestra el proveedor/modelo activo y, si está configurado, el esfuerzo de razonamiento (`effort:low|medium|high`).
- **Fila central**:
  - **Emblema Braille animado** (`⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏`, actualizado con *throttle* de 90 ms) cuando hay actividad en curso (`thinking`, `acting`, `waiting_children`, streaming SSE).
  - **Badge de modo**: `[BUILD]`, `[PLAN]` o `[EXPLORE]`.
  - **Badge de sandbox**: `[SANDBOX:workspace]`, `[SANDBOX:strict]` o `[SANDBOX:off]`.
  - **Estado del agente enfocado**: ID, nombre, estado y tick determinista actual.
  - **Atajos rápidos**: recordatorio compacto (`Tab: Plan/Build · F2: Agents · /rewind · /fork · /services · Esc: quit`).
- **Borde inferior (Telemetría en vivo)**:
  - Ocupación aproximada de ventana de contexto (`ctx: <1%` o `XX%`).
  - Contadores acumulados de sesión del proveedor: `in`, `cache` (tokens servidos desde *prompt cache*), `out` y `think` (tokens de razonamiento cuando el proveedor los reporta).
  - Coste estimado (`~$0.024`) para modelos cloud conocidos, o `local` cuando usas un `--api-base` personalizado.
  - Esta telemetría vive fuera del grafo EAV para **no alterar nunca el determinismo** de ticks, snapshots ni replay.

### Panel `Memory & Trace` y navegación por historial
- Muestra la conversación completa del agente enfocado: prompts de usuario, llamadas a herramientas (`[call]`), observaciones (`[obs]`), respuestas del modelo, notas del sistema (`[system]`) y la vista previa SSE en tiempo real (`[stream] ...▍`, renderizada en `pump_idle` sin consumir ticks).
- **Desplazamiento suave**: usa `Up`, `Down`, `PgUp`, `PgDn`, `Home`, `End`, `Ctrl+U` y `Ctrl+D` incluso mientras hay una aprobación pendiente. Cuando subes en el historial, aparece el indicador `[SCROLLED -N lines | End to follow]`; pulsar `End` o enviar un nuevo mensaje devuelve el seguimiento automático al final.

### Paneles laterales automáticos (`>= 100` columnas)
Cuando tu terminal tiene al menos **100 columnas de ancho**, la zona central se divide en `70%` (`Memory & Trace`) y `30%` (panel derecho):
- **`Live Plan`**: aparece automáticamente cuando el agente registra pasos con `plan_update` (`[x]` completado, `[>]` en progreso, `[ ]` pendiente).
- **`Services (Sprout)`**: aparece cuando existe un `sprout.toml` válido en el workspace, mostrando el estado coloreado de cada servicio (`[READY]`, `[STARTING]`, `[PENDING]`, `[FAILED]`, `[OFF]`).
- Si ambos están activos a la vez, la columna derecha se divide `50%` / `50%` verticalmente.

### Panel de aprobación con diff unificado (`y` / `n`)
Cuando cualquier agente solicita una herramienta mutante (`fs_write`, `fs_edit`, `shell_exec`, `service_start`, `service_stop`, `service_exec`) que requiere confirmación:
- El panel de aprobación identifica explícitamente al solicitante: **`[agent #id name]`** (muy útil cuando delegas trabajo a subagentes).
- Para `fs_write` y `fs_edit` (incluyendo ediciones atómicas multi-hunk), se calcula una **vista previa de diff unificado en color** antes de escribir nada en disco.
- Pulsa **`y`** para aprobar o **`n`** para rechazar.

---

## 2. Control del Esfuerzo de Razonamiento (`--effort`)

Puedes modular cuánto tiempo y tokens dedica el modelo a razonar internamente:

| Valor | Gemini (`thinkingLevel`) | OpenAI-compatible (`reasoning_effort`) | Uso recomendado |
| --- | --- | --- | --- |
| `low` | `LOW` | `low` | Tareas rápidas, consultas de lectura, modelos locales ligeros |
| `medium` | `MEDIUM` | `medium` | Balance diario de edición, debugging y refactorizaciones |
| `high` | `HIGH` | `high` | Diseño arquitectónico, bugs sutiles, auditorías complejas |

Se puede indicar por flag o por variable de entorno (precedencia: `--effort` > `ONYX_EFFORT` > `GEMINI_THINKING_LEVEL` / `OPENAI_REASONING_EFFORT`):

```sh
openonyx chat --effort high
# o mediante variable de entorno:
export ONYX_EFFORT=medium
```

---

## 3. Conversaciones Multi-Turno, Guardia de Raíz Activa y `/new`

### Continuación automática (`continue_agent`)
Al terminar un turno (`Settled`), escribir un nuevo mensaje y pulsar `Enter` **no crea un agente amnésico**: `openonyx` reactiva al mismo agente raíz mediante `continue_agent`, conservando toda su `rel_memory` con contador de pasos monótono.

### Protección contra duplicado de agente raíz
Si el agente raíz todavía está trabajando (`thinking`, `acting`, `waiting_children` o `waiting_approval`), la TUI **bloquea la creación accidental de un segundo agente raíz** al pulsar `Enter` con texto normal, evitando que dos raíces compitan por el mismo workspace. Espera a que termine, aprueba/rechaza la acción pendiente o usa los comandos de inspección (`F2`, `/agent`, `/services`).

### Empezar una conversación limpia (`/new`)
- `/new`: marca que el siguiente prompt creará un agente raíz limpio desde cero (sin arrastrar historial de la tarea anterior).
- `/new <objetivo>`: crea y lanza inmediatamente un nuevo agente raíz con `<objetivo>` y enfoca la vista automáticamente en él.

---

## 4. Contexto Escalonado y `/compact [focus]`

Para evitar que sesiones largas agoten la ventana de contexto o disparen el coste de tokens, `openonyx` aplica un motor de **contexto escalonado (Tier 0 / Tier 1 / Tier 2)** compartido por Gemini y OpenAI:

- **Tier 0 (Completo)**: las últimas 10 observaciones de herramientas se envían íntegras al modelo.
- **Tier 1 (Resumen con coordenadas)**: observaciones antiguas de `fs_read`, `scent_outline`, `scent_impact`, `code_search`, `glob_files` y `shell_exec` se comprimen determinísticamente preservando rutas, rangos de líneas y códigos de salida, manteniendo prefijos estables para maximizar el *prompt caching*.
- **Tier 2 (Compactación `/compact` y auto-compactación)**: al llegar al umbral automático (`80%` de presupuesto de contexto) o al invocar manualmente `/compact [focus]`, `CommandSpawnerSystem` inserta un límite `[session_summary]` verificado en la memoria del agente.

### Cómo sacar partido a `/compact [focus]`

```text
/compact
/compact autenticacion jwt y middleware
```

Cuando proporcionas un `[focus]`:
1. `openonyx` rescata automáticamente `files_found` y `focus_matches` relevantes a las palabras clave desde turnos anteriores.
2. El porcentaje `ctx:` baja de inmediato en el siguiente turno.
3. Se guarda el hito `cmd:/compact` en `snapshots.onyxpack`.
4. **No se pierde nada**: los ticks anteriores al `/compact` siguen intactos en el grafo y en `snapshots.onyxpack` para `inspect`, `replay` y `rewind`.

---

## 5. Flujo Multi-Agente: Subagentes, `F2` y `/agent`

El agente raíz puede delegar subtareas independientes mediante la herramienta `spawn_agent`. Mientras existen hijos activos, el padre pasa a `WaitingChildren`.

- **Presupuesto seguro por defecto**: los subagentes heredan un techo por defecto de **`32` pasos** (`DEFAULT_SUBAGENT_MAX_STEPS`, modificable si el padre pasa `max_steps` explícitamente en los argumentos JSON de `spawn_agent`), evitando bucles infinitos en modelos locales.
- **Inspección en vivo con `F2`**: pulsa **`F2`** en cualquier momento para abrir el popup **Agent Selector**:
  - Muestra la lista de agentes (raíz y subagentes) con su ID, nombre y estado actual.
  - Navega con **`Up`** / **`Down`**, pulsa **`Enter`** para enfocar la traza `Memory & Trace` de ese agente, o **`Esc`** para cerrar el popup.
- **Atajos alternativos**:
  - **`/agent`** (o `/agents`): abre el mismo selector.
  - **`/agent <id|nombre>`**: salta directamente a un agente concreto (por ejemplo `/agent 1` o `/agent researcher`).
  - **`Ctrl+Tab`**: rota rápidamente al siguiente agente enfocado (disponibilidad según soporte de tu terminal para `Ctrl+Tab`; `F2` y `/agent` funcionan en todos los terminales).

---

## 6. Resumen de Atajos y Comandos en `chat`

| Tecla / Comando | Acción |
| --- | --- |
| `Enter` | Envía objetivo, seguimiento al agente raíz o comando slash |
| `Tab` | Alterna en caliente entre `[BUILD]` y `[PLAN]` |
| `F2` | Abre/cierra el popup interactivo **Agent Selector** |
| `Ctrl+Tab` | Rota rápidamente el agente enfocado (alias opcional de `F2`) |
| `y` / `n` | Aprueba o rechaza la herramienta mutante pendiente |
| `Up`/`Down`/`PgUp`/`PgDn`/`Home`/`End`/`Ctrl+U`/`Ctrl+D` | Desplaza el historial `Memory & Trace` |
| `Esc` / `Ctrl+C` | Cierra popup abierto o sale limpiamente guardando snapshot y transcripción |
| `/agent [id\|name]` | Abre el selector de agentes o enfoca el agente indicado |
| `/mode [plan\|build\|explore]` | Cambia de modo o alterna `PLAN` ↔ `BUILD` |
| `/undo` | Revierte el último checkpoint Git reversible (y limpia archivos nuevos de `fs_write`) |
| `/rewind [tick]` | Lista hitos o retrocede en caliente a `<tick>` creando un fork seguro |
| `/fork [name]` | Crea una rama de snapshots hasta el tick actual en un directorio hermano |
| `/compact [focus]` | Compacta turnos anteriores rescatando archivos y coincidencias del foco |
| `/services` | Muestra estado `@TOON v1` de servicios definidos en `sprout.toml` |
| `/service <start\|stop\|logs> <id\|all> [tail]` | Controla servicios Sprout directamente desde la TUI |
| `/new [goal]` | Prepara o lanza un agente raíz limpio |

---

[← 01. Primeros Pasos](01-primeros-pasos.md) · [Índice](index.md) · [Siguiente: 03. Modos, Permisos y Sandbox OS →](03-modos-permisos-y-sandbox.md)
