# 04. Time-Travel, Snapshots y Forking

> 🇬🇧 **[Read this chapter in English](04-time-travel-snapshots-and-forking.md)**

Una de las capacidades diferenciales de `openonyx` es que el estado completo del agente vive en un grafo EAV determinista que puede guardarse, inspeccionarse, reproducirse sin red, revertirse en disco y **bifurcarse sin destruir la rama original**.

---

## 1. Bóveda de Sesiones (`.onyx/vault/`) y `--auto-save`

Para evitar ensuciar la raíz del repositorio con directorios `.snaps_*`, forks y archivos `.tr`, `openonyx` centraliza las sesiones en **`<workspace>/.onyx/vault/`**:

```text
<workspace>/.onyx/vault/
├── .gitignore                                  # creado automáticamente con `*`
├── 20261121-103500_qwen3-coder_main/           # generado por --auto-save
│   ├── snapshots.onyxpack
│   └── 20261121-103500_qwen3-coder_main.tr
├── 20261121-103500_qwen3-coder_main.fork-t12/  # rama creada por /fork o /rewind
├── feat-auth/                                  # --snapshot-dir feat-auth --record chat.tr
│   ├── snapshots.onyxpack
│   └── chat.tr
└── unassigned/                                 # solo --record suelta.tr (sin snapshot-dir)
    └── suelta.tr
```

### Reglas de resolución
- **`--auto-save`** *(o `ONYX_AUTO_SAVE=1`)*: crea automáticamente `<workspace>/.onyx/vault/<YYYYMMDD-HHMMSS>_<modelo>[_<rama|goal>]/` con `snapshots.onyxpack` y `<id>.tr` juntos, muestra `vault:<id>` en la cabecera de la TUI y al salir imprime el comando exacto `openonyx chat --resume <id>`.
- **Nombres cortos sin separador de ruta (`/` o `\`):**
  - `--snapshot-dir feat-auth` → `.onyx/vault/feat-auth/`
  - `--snapshot-dir feat-auth --record chat.tr` → el `.tr` se guarda **dentro** de `.onyx/vault/feat-auth/chat.tr`.
  - `--record solo.tr` (sin `--snapshot-dir`) → se guarda en `.onyx/vault/unassigned/solo.tr`.
- **Rutas explícitas intactas:** cualquier ruta absoluta o con separador (`./mi-dir`, `C:\tmp\snaps`, `../otro`) se respeta tal cual.
- **Búsqueda transparente al leer:** `--resume`, `inspect`, `rewind`, `replay` y `doctor` buscan primero la ruta literal en disco y, si es un nombre corto, la localizan automáticamente dentro de `.onyx/vault/` (y `.onyx/vault/unassigned/` para `.tr`).
- **Aislamiento total de Git y herramientas:** `openonyx` crea `.onyx/vault/.gitignore` (`*`) automáticamente para que `git status` y los checkpoints `/undo` nunca absorban el vault, mientras `WorkspacePolicy` oculta `.onyx/vault/` de `fs_list`/`glob_files`/`code_search` y lo protege como Control Plane de solo lectura.

---

## 2. Cómo funcionan `snapshots.onyxpack` y los hitos

Al pasar `--snapshot-dir <dir>` en `chat`, `run` o `replay`, `MilestoneSnapshotSystem` captura snapshots solo en **transiciones relevantes** en lugar de desperdiciar disco en ticks inactivos:

- `init` — estado inicial de la sesión.
- `spawn` — creación de agente raíz o subagente.
- `approval:<tool>` — solicitud de aprobación de herramienta mutante.
- `tool:<tool>` / `tool_error:<tool>` — resultado o fallo de herramienta.
- `cmd:/agent`, `cmd:/undo`, `cmd:/rewind`, `cmd:/fork`, `cmd:/compact`, `cmd:/mode ...`, `cmd:/new` — comandos slash ejecutados.
- `settled`, `failed`, `exit` — cierre de turno o salida limpia.
- `periodic` — hitos periódicos opcionales si indicas `--snapshot-every <n>`.

Todos los hitos se almacenan en un único contenedor append-only comprimido **`<dir>/snapshots.onyxpack`** (`ONYXPK1` con compresión zlib, payload canónico `ONYXAG\x01\0` y checksum FNV-1a de 64 bits):

- **Tolerancia a cortes**: si un crash deja un frame truncado al final del archivo, `PackSnapshotStore` ignora de forma segura la cola dañada al leer y la repara automáticamente en el siguiente append.
- **Compatibilidad legacy**: los directorios antiguos con `tick-*.bin` siguen siendo legibles automáticamente en `inspect`, `rewind` y `--resume`.
- **Retención opcional**: por defecto se conserva todo el historial para poder hacer *time-travel* a cualquier punto; puedes activar una ventana deslizante con `--max-snapshots <n>` (conserva `init` + últimos `n - 1` hitos).

---

## 3. Inspeccionar una sesión (`openonyx inspect`)

Sin API key y sin tocar el workspace, puedes auditar cualquier directorio de snapshots:

```sh
# Listar todos los hitos: tick, etiqueta, tamaño comprimido/raw, FNV-1a y tree_sha Git
openonyx inspect .snaps_feat --list

# Salida en JSON para automatización o depuración
openonyx inspect .snaps_feat --list --json

# Inspeccionar el último hito o un tick concreto
openonyx inspect .snaps_feat
openonyx inspect .snaps_feat --tick 24
openonyx inspect .snaps_feat --tick 24 --json
```

---

## 4. Deshacer cambios paso a paso (`/undo`)

Cada vez que `fs_write` o `fs_edit` va a modificar el disco en modo `BUILD`, `WorkspaceToolBridge` captura automáticamente un **checkpoint de árbol Git ligero** usando un `GIT_INDEX_FILE` temporal (`git add -A` + `git write-tree`):

- **Nunca toca `HEAD` ni tus commits.**
- **Nunca altera tu staging area (`git add`).**
- Funciona incluso en repositorios recién inicializados sin primer commit (*unborn `HEAD`*).

Cuando escribes **`/undo`** dentro de `openonyx chat`:
1. Se marca en el grafo el último efecto reversible como deshecho (`ws.effect.undone`).
2. Se restaura en disco el `tree_sha` exacto anterior a esa edición (`restore_git_tree_checkpoint_for_targets`).
3. **Limpieza segura de archivos nuevos**: si la acción deshecha fue un `fs_write` que creó un archivo nuevo no rastreado por Git, `/undo` lo elimina limpiamente sin borrar ningún otro archivo untracked tuyo.
4. Se invalidan automáticamente las cachés de sesión (`fs_read` y `scent_outline`) para que el agente vuelva a leer el contenido real del disco.

---

## 5. Bifurcar y viajar en el tiempo en vivo (dentro de la TUI)

Si iniciaste `chat` con `--snapshot-dir <dir>` (o `--resume <dir>`), puedes explorar alternativas **sin cerrar la TUI y sin sobrescribir el historial original**:

### `/fork [nombre]` — Bifurcar desde el tick actual
```text
/fork experimento-parser
# o sin nombre (crea <dir>.fork-t<tick>):
/fork
```
- Copia todos los hitos hasta el tick actual a un directorio hermano (`<dir>.fork-experimento-parser` o `<dir>.fork-t42`).
- Cambia en caliente el store activo (`SharedPackStore`) a la nueva rama.
- El `snapshots.onyxpack` original queda **100% intacto**.

### `/rewind [tick]` — Retroceder en caliente dentro de la TUI
```text
# 1. Ver lista de hitos disponibles con tick, etiqueta y checkpoint Git
/rewind

# 2. Retroceder al tick 18 (o al hito previo más cercano <= 18)
/rewind 18
```
- **Guardia de seguridad**: si hay un agente trabajando o una petición LLM en vuelo, `/rewind` rechaza ejecutarse hasta que la sesión esté en reposo para evitar estados corruptos.
- **No destructivo por defecto**: crea automáticamente un directorio fork hermano (`<dir>.fork-t18`), restaura el grafo al tick objetivo, reconcilia los efectos huérfanos en disco (restaurando checkpoints Git y limpiando archivos `fs_write` posteriores al tick) e informa del resumen directamente en `Memory & Trace`.

---

## 6. Time-Travel y Forking desde la CLI

También puedes retomar, bifurcar o auditar desde la línea de comandos:

### Reanudar en el último hito
```sh
openonyx chat --resume .snaps_feat
```

### Reanudar desde un tick histórico creando un fork seguro
```sh
# Crea .snaps_feat.fork-t12 y abre la TUI en el tick 12 (original intacto)
openonyx chat --resume .snaps_feat --tick 12

# Especificando ruta exacta del fork:
openonyx chat --resume .snaps_feat --tick 12 --fork-dir .snaps_ruta_b

# Atajo equivalente desde el subcomando rewind:
openonyx rewind .snaps_feat 12 --interactive
openonyx rewind .snaps_feat 12 --tui --fork-dir .snaps_ruta_b
```

### Modo destructivo opt-in (`--truncate-future`)
Si deliberadamente quieres recortar el `snapshots.onyxpack` original descartando los ticks posteriores a `<tick>` en lugar de crear un fork hermano, pasa `--truncate-future`:

```sh
openonyx chat --resume .snaps_feat --tick 12 --truncate-future
```

### Rewind headless con previsualización (`--dry-run`) y replay
```sh
# 1. Previsualizar qué efectos huérfanos y checkpoints Git se revertirían (0 cambios en disco)
openonyx rewind .snaps_feat 12 --dry-run

# 2. Retroceder y reproducir determinísticamente desde una transcripción (0 llamadas HTTP)
openonyx rewind .snaps_feat 12 --transcript feat.tr
```

---

## 7. Transcripciones y Replay 100% Offline (`ONYXTR2`)

Grabando una sesión con `--record <archivo.tr>` (alias `--save-transcript <archivo.tr>`), `RecordingBridge` registra todas las decisiones del LLM, llamadas estructuradas (`call_id`), comandos slash (`Tab`, `/mode`, `/undo`, `/compact`, etc.) y metadatos de checkpoints (`[checkpoint:<head>:<tree>]`). Las primitivas 0-tokens del depurador temporal se **sellan** en la transcripción como `/__onyx_det_*` (shell `!cmd`, `/read`, `/inject`, `/step`, `/step-llm` y la cirugía `/graph`): el replay las reproduce **bit-a-bit con 0 I/O y 0 red** (no se re-ejecuta ningún comando).

```sh
# Grabar una sesión
openonyx chat --snapshot-dir .snaps_demo --record demo.tr

# Reproducirla después sin red, sin API key y sin modificar archivos en disco
openonyx replay demo.tr --snapshot-dir .snaps_replay
```

Esto resulta invaluable para:
- **Depuración determinista**: reproducir un fallo exacto del harness o de un prompt sin volver a pagar tokens.
- **Auditoría**: verificar qué decisiones tomó el agente en una corrida pasada.
- **Regresión**: reconstruir el grafo completo desde una transcripción y compararlo con `inspect --json`.

---

## 8. Auditoría de Salud (`openonyx doctor`)

Tras un corte de energía, un error de red o antes de un rewind delicado, ejecuta:

```sh
openonyx doctor --snapshot-dir .snaps_feat --transcript feat.tr
```

`doctor` comprueba en un solo paso:
1. Estado del repositorio Git y posibles artefactos residuales.
2. Integridad de `snapshots.onyxpack` (cabecera `ONYXPK1`, checksums FNV-1a, payloads `ONYXAG`, detección de colas truncadas) y snapshots legacy `tick-*.bin`.
3. Validez de objetos `tree_sha` referenciados por el ledger de efectos.
4. Cabecera y legibilidad de la transcripción (`ONYXTR1` / `ONYXTR2`).
5. Capacidades de sandbox OS del sistema anfitrión (`bwrap`/`sandbox-exec`/`win32-job`, runtime OCI).
6. Validación del DAG y puertos ocupados de `sprout.toml` (si existe en el workspace).

---

## 9. Paridad Web: el Time Debugger en `openonyx serve`

La PWA embebida tiene **paridad completa** con el depurador temporal de la TUI (SDD-07 Fase 4A): el mismo estado de sesión alimenta ambas superficies, por lo que lo que haces desde el navegador afecta a la sesión en vivo y viceversa.

- **Pill de tick**: la cabecera muestra `#tick-pill` (botonera aria). En vivo dice `tick N`; bajo freeze o con el cursor anclado en el pasado dice `⏸ X/Y` y abre la **barra ámbar de Time Debugger** automáticamente (puedes cerrarla; se vuelve a abrir sola al congelar).
- **Barra ámbar**: estado (`⏸ TIME FROZEN · tick X / present Y` / `LIVE · tick Y`), slider que **snapea al tick disponible más cercano** (rechaza ticks purgados o futuros con aviso accionable), botones ◀/▶, `Freeze`/`Present`, `Step`, `Step 1 LLM` y `Resume` — estos tres últimos van por el **túnel de comandos sellado** (`/step 1`, `/step-llm`, `/go`), no por la API de scrub: la bifurcación se aplica en la sesión con sus gates.
- **API efímera**: `POST /api/v1/time/scrub` (JSON: `action: freeze|unfreeze|return_to_present|scrub_to|scrub_delta`) muta solo el estado RAM del depurador — **0 disco, 0 eventos en el bus** — y responde con el snapshot `time_debug` actualizado. Hereda la autenticación Bearer y el guard CSRF de todo `/api/v1/*` (401 sin credenciales, 403 cross-site, 400 accionable para tick futuro o acción desconocida).
- **Prompts planos bajo freeze**: `POST /api/v1/command` responde **`409 Conflict`** con mensaje accionable (`Paused in TIME FROZEN: use /go to resume or /inject to stage memory…`); `/go`, `/graph …`, `/inject …`, `/read …`, `/step …`, `/step-llm`, `/time` y `!cmd` siguen aceptándose.
- **Cirugía de memorias en la UI**: con el agente raíz congelado, cada memoria del root muestra `✎` (editar en un diálogo → `/graph edit-mem <seq> <texto>` plano) y `×` (eliminar con confirmación → `/graph drop-mem <seq>`).
- **Preview de proyección y aviso de revert**: el snapshot Web incluye `time_debug` completo (`frozen`, `cursor_tick`, `present_tick`, `available_ticks`, `pending_revert_files` desde los efectos del grafo vivo, 0 I/O) y, con `--snapshot-dir` + objetivo LLM real, el **`bench_hint`**: un comando `openonyx bench --snapshot-dir … --tick … --models provider:model` copiable para comparar el modelo vivo contra un benchmark determinista desde el mismo cursor (sinergia SDD-05 §9.4).

> **Non-goal**: los efectos de **Sprout** y de los **servidores MCP** son efectos externos de proceso: el TimeFreeze (TUI o Web) **no los rebobina**.

---

[← 03. Modos, Permisos y Sandbox OS](03-modos-permisos-y-sandbox.md) · [Índice](index.md) · [Siguiente: 05. Herramientas, Scent, Sprout y Skills →](05-herramientas-scent-sprout-y-skills.md)
