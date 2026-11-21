# 01. Primeros Pasos con openOnyx

> 🇬🇧 **[Read this chapter in English](01-first-steps.md)**

En menos de 10 minutos pasarás de compilar `openonyx` a ejecutar tu primera sesión interactiva con streaming SSE, cambios supervisados con diff en color, modo `PLAN`/`BUILD` y reversión instantánea con `/undo`.

---

## 1. Compilar o instalar `openonyx`

Desde la raíz del repositorio OnYX:

```sh
# Compilación optimizada (recomendada para uso diario)
cargo build --release -p open-onyx

# O instalar en ~/.cargo/bin para invocar `openonyx` desde cualquier proyecto
cargo install --path open-onyx
```

Comprueba que el binario responde y revisa el estado de tu entorno:

```sh
openonyx --version
openonyx --help
openonyx doctor
```

> **Tip:** si no instalaste el binario globalmente, puedes sustituir `openonyx` por `cargo run --release -p open-onyx --` en todos los comandos de este manual, o usar `./target/release/openonyx` (Windows: `.\target\release\openonyx.exe`).

---

## 2. Configurar tu proveedor LLM

`openonyx` soporta de forma nativa **Gemini** (por defecto) y cualquier API compatible con **OpenAI `/v1/chat/completions`** (OpenAI, OpenRouter, Ollama, vLLM, LM Studio).

### Opción A — Google Gemini (default)

**Windows (PowerShell):**
```powershell
$env:GEMINI_API_KEY = "tu-api-key"
openonyx chat --auto-save
```

**Linux / macOS (bash/zsh):**
```sh
export GEMINI_API_KEY="tu-api-key"
openonyx chat --auto-save
```

Para elegir modelo o esfuerzo de razonamiento (`low`, `medium`, `high`):
```sh
openonyx chat --model gemini-2.5-pro --effort high --auto-save
```

### Opción B — OpenAI / OpenRouter

**Windows (PowerShell):**
```powershell
$env:OPENAI_API_KEY = "sk-..."
openonyx chat --provider openai --model gpt-4o --effort medium --auto-save
```

**Linux / macOS:**
```sh
export OPENAI_API_KEY="sk-..."
openonyx chat --provider openai --model gpt-4o --effort medium --auto-save
```

Con OpenRouter u otro proxy compatible:
```sh
openonyx chat --provider openai --api-base https://openrouter.ai/api/v1 --model anthropic/claude-sonnet-4 --auto-save
```

### Opción C — Modelo local (Ollama, vLLM, LM Studio)

Cuando especificas `--api-base` (o `OPENAI_API_BASE` / `OPENAI_BASE_URL`), **no necesitas configurar `OPENAI_API_KEY`**.

**Ollama:**
```sh
openonyx chat --provider openai --api-base http://localhost:11434/v1 --model qwen2.5-coder:14b --effort low --auto-save
```

**vLLM / LM Studio:**
```sh
openonyx chat --provider openai --api-base http://localhost:8000/v1 --model Qwen/Qwen3-Coder-30B-A3B --auto-save
```

> **Consejo:** usar **`--auto-save`** (o fijar `ONYX_AUTO_SAVE=1` en tu entorno) guarda automáticamente los snapshots y la transcripción `.tr` juntos dentro de `.onyx/vault/<fecha>_<modelo>_<rama>/` (ignorado automáticamente por Git) para que nunca ensucies la raíz del repo y siempre puedas usar `/rewind` o `--resume`.

---

## 3. Tu primera sesión interactiva (`openonyx chat`)

1. Abre una terminal dentro de cualquier repositorio Git (o pasa `-w /ruta/al/proyecto`).
2. Lanza `openonyx chat --snapshot-dir .snaps_demo --record demo.tr`.
3. Verás la TUI con:
   - **Cabecera superior**: emblema de estado, modo `[BUILD]` (o `[PLAN]`), badge de sandbox `[SANDBOX:workspace]`, modelo y esfuerzo, y barra de telemetría (`ctx`, tokens `in`/`cache`/`out`/`think` y coste).
   - **Panel principal `Memory & Trace`**: conversación, llamadas a herramientas y respuestas streaming `[stream] ...▍`.
   - **Caja de entrada inferior**: tu prompt y accesos rápidos.

### Paso 1 — Explora sin riesgo en modo `[PLAN]`

Pulsa **`Tab`** para cambiar a **`[PLAN]`** (solo lectura; las herramientas mutantes quedan bloqueadas automáticamente). Escribe un objetivo de análisis y pulsa **`Enter`**:

```text
Analiza la estructura de este repositorio y propón un plan para añadir una función de saludo en el módulo principal.
```

Observa cómo el agente usa herramientas de solo lectura (`glob_files`, `scent_outline`, `code_search`, `fs_read`, `plan_update`) sin pedirte aprobación. Si tu terminal tiene `>= 100` columnas y el agente actualiza su lista de pasos, aparecerá el panel derecho **`Live Plan`**.

### Paso 2 — Pasa a `[BUILD]` y aprueba cambios con diff en color

Pulsa **`Tab`** de nuevo para pasar a **`[BUILD]`** y escribe:

```text
Implementa el cambio en el archivo principal y ejecuta los tests.
```

Cuando el agente invoque `fs_edit`, `fs_write` o `shell_exec`, la TUI se pausará en **`WaitingApproval`**:

- Verás qué agente pide la acción (`[agent #1 ...]`).
- Para ediciones de archivos, verás un **diff unificado en color** antes de tocar el disco.
- Puedes desplazarte por el historial mientras revisas con `Up`/`Down`/`PgUp`/`PgDn`.
- Pulsa **`y`** para aprobar o **`n`** para rechazar (si rechazas, el agente recibe la denegación y puede ajustar su propuesta).

### Paso 3 — Deshacer el último cambio con `/undo`

¿No te convenció el último `fs_edit` o `fs_write`? Escribe en la caja de entrada:

```text
/undo
```

`openonyx` restaura el checkpoint Git ligero capturado justo antes de la edición —sin modificar `HEAD` ni tu `git add`— y, si la operación fue un `fs_write` que creó un archivo nuevo no rastreado, lo elimina limpiamente sin tocar otros archivos tuyos.

### Paso 4 — Salir guardando todo

Pulsa **`Esc`** o **`Ctrl+C`**. `openonyx` guarda el hito final (`exit`) en `.snaps_demo/snapshots.onyxpack` y cierra la transcripción `demo.tr`.

---

## 4. Reanudar, inspeccionar o reproducir después

Una vez cerrada tu primera sesión, prueba estos tres comandos sin gastar tokens extra:

```sh
# 1. Ver todos los hitos guardados y sus checkpoints Git
openonyx inspect .snaps_demo --list

# 2. Reproducir la sesión 100% offline (0 HTTP, 0 cambios en disco)
openonyx replay demo.tr

# 3. Reanudar exactamente donde lo dejaste
openonyx chat --resume .snaps_demo
```

---

## 5. Ejecución de un solo turno (headless)

Si prefieres ejecutar una tarea puntual sin TUI (por ejemplo desde un script o CI):

```sh
openonyx run "resume los cambios pendientes en git diff" --record run.tr --snapshot-dir .snaps_run
```

Al terminar, `openonyx` imprime el resultado final y una línea `[openOnyx telemetry]` con los tokens y coste de la ejecución.

---

[← Índice del Manual](index.md) · [English Quickstart →](01-first-steps.md) · [Siguiente: 02. Interfaz TUI y Flujo Diario →](02-interfaz-tui-y-flujo-diario.md)
