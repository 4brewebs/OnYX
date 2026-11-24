# 02. Interfaz TUI y Flujo de Trabajo Diario

> 🇬🇧 **[Read this chapter in English](02-tui-and-daily-workflow.md)**

Este capítulo detalla cada componente visual de `openonyx chat`, cómo controlar el esfuerzo de razonamiento (`--effort`) y cambiar el objetivo LLM en caliente, cómo funciona la memoria multi-turno y el contexto escalonado (`/compact`), cómo navegar las cinco vistas efímeras `F1..F5` (con la paleta `Ctrl+K` y `/view`), y cómo supervisar subagentes desde la vista `F2 Agents` (o el selector `/agent`).

---

## 1. Anatomía de la TUI

Al ejecutar `openonyx chat`, la pantalla se organiza en cuatro zonas bien definidas:

```text
┌ F1 Chat · F2 Agents · F3 Sprout · F4 Plan & Diff · F5 Timeline · Esc:Quit ──────────────┐
│ openOnyx [BUILD] [SANDBOX:workspace] · gemini/gemini-2.5-pro (effort:high)              │
│ goal:  Refactoriza el parser… · phase: thinking / streaming… · #1 [root] [subagents:1] │
│ ⠋ Tab: Plan/Build · F2: Agents · /rewind · /fork · /services · Esc: quit               │
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

### Cabecera compacta (pestañas + título + cuerpo + telemetría)
- **Tira de pestañas (fila superior, alineada a la derecha)**: `F1 Chat · F2 Agents · F3 Sprout · F4 Plan & Diff · F5 Timeline`, con la vista activa en blanco negrita. Al final, la etiqueta `Esc:Quit` en `F1 Chat` o `Esc:Back` en las vistas efímeras; si hay una aprobación pendiente **fuera** de `F1 Chat`, se sustituye por el badge rojo **[F1 Chat !APPROVAL]** (aprobar a ciegas está prohibido: en las demás vistas `y`/`n` nunca aprueban ni deniegan).
- **Fila de título**: `openOnyx [BUILD] [SANDBOX:workspace] · gemini/gemini-2.5-pro (effort:high)` — badge de modo (`[BUILD]`, `[PLAN]` o `[EXPLORE]`), badge de sandbox (`[SANDBOX:workspace]`, `[SANDBOX:strict]` o `[SANDBOX:off]`), el tag `[UNTRUSTED]` si el workspace no está confiado y el objetivo LLM **vivo** `provider/model (effort:...)`, que se actualiza en caliente con `/profile` / `/provider` / `/model` / `/effort` (sección 2.1).
- **Filas de cuerpo**:
  - **Emblema Braille animado** (`⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏`, actualizado con *throttle* de 90 ms) cuando hay actividad en curso (`thinking`, `acting`, `waiting_children`, streaming SSE).
  - **Objetivo y fase**: `goal:` del agente enfocado y `phase:` con la fase en curso, el ID/nombre/rol (`#1 openonyx-1 [root]`), los tags `[subagents:N · bg:M · F2]` (o `[N/M agents · F2]`), `[queued:N]` y, si el guardado en sesión está activo (`--auto-save` o `--snapshot-dir`), la etiqueta viva `vault:<sesion>` (que se actualiza automáticamente al crear ramas con `/fork` o `/rewind`).
  - **Atajos rápidos o tira de agentes**: recordatorio compacto (`Tab: Plan/Build · F2: Agents · /rewind · /fork · /services · Esc: quit`) o, con múltiples agentes, la tira compacta de agentes en una línea.
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
- **Solo desde `F1 Chat`**: en las vistas efímeras `F2..F5` las teclas `y`/`n` **nunca** aprueban ni deniegan; la tira de pestañas muestra el badge `[F1 Chat !APPROVAL]` para recordarte volver a `F1` a revisar el diff y la línea `# impact:`.

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

### 2.1 Objetivo LLM en caliente (`/profile`, `/provider`, `/model`, `/effort`)

Más allá del esfuerzo, el objetivo LLM completo (proveedor, modelo, `api_base` y esfuerzo) es **conmutable en caliente** sin reiniciar la sesión:

- **Perfiles**: bloques `[[llm_profiles]]` en `.onyx/config.toml` (workspace confiado) con `id`, `provider` (`gemini` | `openai`), `model`, `effort`, `api_base` opcional y `api_key_env` opcional. **Nunca** se admiten claves literales en el archivo (detalles en el capítulo 03).
- **Comandos slash**: `/profile [id]` lista o aplica un perfil, `/provider <gemini|openai>`, `/model <name>` y `/effort <auto|low|medium|high>`. El cambio se aplica en la siguiente frontera de tick y queda sellado en la transcripción `ONYXTR2` (replay determinista).
- **Cabecera viva**: la fila de título pasa a mostrar `provider/model (effort:...)` del objetivo actual en lugar del de arranque.
- **Paleta `Ctrl+K`**: alternativa sin teclear — filtras perfiles, proveedores y esfuerzos y ejecutas con `Enter` (las acciones LLM pasan por el bus determinista, igual que si las tecleas; un perfil sin credenciales se muestra como `unavailable` con su motivo y no es seleccionable).
- **Web UI (`openonyx serve`)**: el botón *LLM target* (subtítulo de la marca) abre un diálogo modal con perfil/modelo/esfuerzo y botones Apply/Cancel, sin exponer credenciales al navegador.

> Las credenciales **nunca** viven en el config: cada perfil resuelve su clave por `api_key_env` (o la variable estándar del proveedor → `ONYX_API_KEY`). Un perfil sin clave o con `api_base` rechazada se lista como `unavailable` con su motivo accionable.

---

## 3. Conversaciones Multi-Turno, Guardia de Raíz Activa y `/new`

### Continuación automática (`continue_agent`)
Al terminar un turno (`Settled`), escribir un nuevo mensaje y pulsar `Enter` **no crea un agente amnésico**: `openonyx` reactiva al mismo agente raíz mediante `continue_agent`, conservando toda su `rel_memory` con contador de pasos monótono.

### Cola de seguimiento mientras el agente trabaja (`queued:N`)
Si el agente raíz está ocupado (`thinking`, `acting`, `waiting_children` o `waiting_approval`), puedes escribir tu siguiente instrucción y pulsar `Enter`: en lugar de crear un segundo agente raíz que compita por el workspace, la TUI la **encola automáticamente** (`queued:N` en cabecera y borde inferior, previsualizada si la entrada está vacía, y limpiable en cualquier momento con **`Ctrl+X`**) y la despacha al instante en cuanto el turno termina. Los comandos inmediatos (`/agent`, `/mode`, `/services`, `/service`, `/undo`) siguen ejecutándose al momento.

### Autocompletado `/`, `@archivo`, cursor e historial de entrada
- **Cursor UTF-8 visible**: muévete dentro del prompt con `Left`, `Right`, `Home`, `End`, `Backspace` y `Delete`.
- **Historial de prompts**: navega tus prompts anteriores con **`Alt+Up` / `Alt+Down`** (o **`Ctrl+P` / `Ctrl+N`**), conservando automáticamente tu borrador si vuelves abajo.
- **Popup `/`**: al empezar con `/`, aparece un menú flotante con todos los comandos slash y su descripción (`↑`/`↓`, `Tab`/`Enter` para aceptar, `Esc` para cerrar).
- **Popup `@`**: al escribir `@` al inicio de una palabra (p. ej. `revisa @tui`), se filtran en tiempo real las rutas del workspace (ignorando `.git`, `target`, `node_modules` y `.onyx/vault`) y al pulsar `Tab`/`Enter` se inserta la ruta relativa limpia.

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

## 5. Vistas Modulares `F1..F5`, Paleta `Ctrl+K` y `/view`

Además de `F1 Chat` (traza, paneles laterales, input y aprobaciones), la TUI expone cuatro vistas efímeras a las que saltas con la tecla funcional, `/view <nombre>` o la paleta `Ctrl+K`:

| Vista | Tecla | Slash | Contenido |
| --- | --- | --- | --- |
| `F1 Chat` | `F1` | `/view chat` | Traza `Memory & Trace`, paneles `Live Plan` / `Services (Sprout)`, input y aprobaciones |
| `F2 Agents` | `F2` | `/view agents` | Agentes del grafo + jobs asíncronos `[bg]` (con badge `→ <driver>` si el worktree lo corre un harness CLI externo) |
| `F3 Sprout` | `F3` | `/view sprout` | Servicios `sprout.toml` (badges de estado) + logs en vivo del servicio seleccionado |
| `F4 Plan & Diff` | `F4` | `/view plan` | Plan activo (`ws.plan.text`) arriba + ledger de efectos Git (tick, tool, target, reversibilidad, `(undone)`) abajo |
| `F5 Timeline` | `F5` | `/view timeline` | Hitos de `snapshots.onyxpack` (tick, tag, tamaño, checksum) para `/rewind` / `/fork` |

- **Navegación 100 % efímera**: cambiar de vista **no** escribe en el grafo, no genera eventos de bus ni avanza ticks — es solo un "cristal" distinto sobre el mismo estado determinista.
- **`F2 Agents`**: `Up`/`Down` selecciona (el agente enfocado lleva el marcador `*`), `Enter` enfoca la traza `Memory & Trace` del agente y vuelve a `F1 Chat`, `Esc` vuelve a `F1 Chat`.
- **`F3 Sprout`**: `Up`/`Down`/`PgUp`/`PgDn` navegan servicios; `s`/`x`/`r`/`a` (start/stop/restart/all) y `f` (follow de logs) están **bajo gate**: en modo `PLAN`/`EXPLORE` o con workspace `[UNTRUSTED]` la cabecera de la vista muestra `MUTATIONS BLOCKED` y la acción se rechaza con un aviso.
- **`F4 Plan & Diff`**: `Up`/`Down` desplazan el ledger de efectos Git.
- **`F5 Timeline`**: `Up`/`Down` seleccionan hito, `Enter` lanza `/rewind <tick>`, `f` lanza `/fork`.
- **`Esc`**: cierra popups o la paleta si están abiertos; en `F1 Chat` sale limpiamente; en `F2..F5` vuelve a `F1 Chat` (la etiqueta de la tira de pestañas dice `Esc:Quit` o `Esc:Back`).
- **`/view <chat|agents|sprout|plan|timeline>`** (case-insensitive): se intercepta en `Enter` a nivel de TUI — 0 eventos de bus, 0 ticks, 0 escrituras al grafo. `/view` sin argumento muestra el usage; un nombre desconocido muestra un aviso transitorio.
- **Paleta `Ctrl+K`**: popup centrado (`Ctrl+K Palette — type to filter`) con tres grupos — **VIEW** (`F1..F5`), **LLM** (perfiles `[[llm_profiles]]`, proveedores y esfuerzos) y **AGENT** (agente por ID/nombre). Escribe para filtrar (todos los caracteres imprimibles filtran — `j`/`k` incluidos, nunca navegan), `Up`/`Down` (o `Ctrl+P`/`Ctrl+N`) eligen, `Enter` ejecuta y `Esc`/`Ctrl+K` cierran. Tiene prioridad máxima (salvo `Ctrl+C`) y traga todas las teclas mientras está abierta; las acciones LLM se envían por el bus determinista y quedan selladas en `ONYXTR2`, igual que tecleadas.

---

## 6. Flujo Multi-Agente: Subagentes, Vista `F2 Agents` y `/agent`

El agente raíz puede delegar subtareas de dos formas: subagentes **síncronos** del grafo (`spawn_agent`, que comparten la workspace y a los que el padre espera) y jobs **asíncronos** en worktrees Git aislados (`spawn_subagent` / `/spawn`, capítulo 05).

- **Presupuesto seguro por defecto**: los subagentes sincrónos heredan un techo por defecto de **`32` pasos** (`DEFAULT_SUBAGENT_MAX_STEPS`, modificable si el padre pasa `max_steps` explícitamente en los argumentos JSON de `spawn_agent`), evitando bucles infinitos en modelos locales. Mientras existen hijos activos, el padre pasa a `WaitingChildren`.
- **Inspección en vivo con la vista `F2 Agents`**: pulsa **`F2`** en cualquier momento para saltar a la vista **Agents** (a pantalla completa):
  - Lista todos los agentes del grafo (raíz y subagentes) con ID, nombre, rol (`root`/`sub`), estado actual y objetivo.
  - También lista los jobs asíncronos `[bg]` con su estado (`running`, `done`, `merged`, `failed`), rama de worktree y, si el job lo corre un harness externo, el badge `→ <driver>`.
  - Navega con **`Up`** / **`Down`**, pulsa **`Enter`** para enfocar la traza `Memory & Trace` del agente (vuelve a `F1 Chat`) o **`Esc`** para volver a `F1 Chat`.
- **Atajos alternativos**:
  - **`/agent`** (o `/agents`): abre el popup **Agent Selector** centrado (o rota al siguiente agente si solo existe uno).
  - **`/agent <id|nombre>`**: salta directamente a un agente concreto (por ejemplo `/agent 1` o `/agent researcher`).
  - **`Ctrl+K`**: la paleta también lista los agentes; `Enter` enfoca y vuelve a `F1 Chat`.
  - **`Ctrl+Tab`**: rota rápidamente al siguiente agente enfocado (alias dependiente del terminal; `F2`, `/agent` y la paleta funcionan en todos los terminales).
- **Drivers externos desde la Web UI**: en `openonyx serve`, el selector *Subagent driver* permite elegir un `[[external_drivers]]` declarado; al enviar texto libre con un driver no nativo, el navegador emite `/spawn --driver <id> <texto>` (capítulo 05).

---

## 7. Resumen de Atajos y Comandos en `chat`

| Tecla / Comando | Acción |
| --- | --- |
| `Enter` | Envía objetivo, seguimiento al agente raíz o comando slash |
| `Tab` | Alterna en caliente entre `[BUILD]` y `[PLAN]` |
| `F1..F5` | Cambia de vista efímera (Chat, Agents, Sprout, Plan & Diff, Timeline) |
| `Ctrl+K` | Abre la paleta de comandos (vistas + slash LLM + agentes); escribe para filtrar, `Enter` ejecuta |
| `Ctrl+Tab` | Rota rápidamente el agente enfocado (alias opcional de `F2` / `/agent`) |
| `y` / `n` | Aprueba o rechaza la herramienta mutante pendiente (**solo en `F1 Chat`**; en otras vistas la TUI avisa a pulsar `F1`) |
| `Up`/`Down`/`PgUp`/`PgDn`/`Home`/`End`/`Ctrl+U`/`Ctrl+D` | Desplaza el historial `Memory & Trace` |
| `Esc` | Cierra popup/paleta; en `F2..F5` vuelve a `F1 Chat`; en `F1 Chat` sale limpiamente guardando snapshot y transcripción |
| `Ctrl+C` | Sale limpiamente (snapshot de salida y transcripción, si están configurados) |
| `/view <chat\|agents\|sprout\|plan\|timeline>` | Cambia de vista desde el prompt (efímero: 0 eventos de bus, 0 ticks, 0 grafo) |
| `/agent [id\|name]` | Abre el selector de agentes o enfoca el agente indicado |
| `/mode [plan\|build\|explore]` | Cambia de modo o alterna `PLAN` ↔ `BUILD` |
| `/undo` | Revierte el último checkpoint Git reversible (y limpia archivos nuevos de `fs_write`) |
| `/rewind [tick]` | Lista hitos o retrocede en caliente a `<tick>` creando un fork seguro |
| `/fork [name]` | Crea una rama de snapshots hasta el tick actual en un directorio hermano |
| `/compact [focus]` | Compacta turnos anteriores rescatando archivos y coincidencias del foco |
| `/services` | Muestra estado `@TOON v1` de servicios definidos en `sprout.toml` |
| `/service <start\|stop\|logs> <id\|all> [tail]` | Controla servicios Sprout directamente desde la TUI |
| `/new [goal]` | Prepara o lanza un agente raíz limpio |
| `/profile [id]`, `/provider <gemini\|openai>`, `/model <name>`, `/effort <auto\|low\|medium\|high>` | Cambian el objetivo LLM en caliente (sección 2.1) |
| `/spawn [--driver <id>] <goal>` | Lanza un job asíncrono en worktree Git aislado (capítulo 05) |
| `/subagents` | Lista los jobs asíncronos `[bg]` (estado, rama, archivos tocados) |
| `/merge <job_id>` | Fusión 3-way del worktree del job (governance `Ask`/`Deny`, capítulo 03) |
| `/discard <job_id>` | Cancela el job y elimina su worktree y rama temporal |

---

[← 01. Primeros Pasos](01-primeros-pasos.md) · [Índice](index.md) · [Siguiente: 03. Modos, Permisos y Sandbox OS →](03-modos-permisos-y-sandbox.md)
