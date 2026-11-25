// openOnyx Web UI Client — 100% DOM-safe (createElement + textContent only)
// Compatible with strict CSP `script-src 'self'; require-trusted-types-for 'script'`.

(() => {
  'use strict';

  const SLASH_HINTS = [
    { key: '/cancel', desc: 'Stop active subagent or interrupt current turn' },
    { key: '/cancel all', desc: 'Interrupt both subagents and root turn' },
    { key: '/subagents', desc: 'List background worktree subagent jobs' },
    { key: '/spawn', desc: 'Launch background subagent without a root turn (`[--driver <id>] <task>`)' },
    { key: '/merge', desc: '3-way merge finished async subagent (`<job_id>`)' },
    { key: '/discard', desc: 'Cancel & discard async subagent worktree (`<job_id>`)' },
    { key: '/mode plan', desc: 'Switch to read-only PLAN mode' },
    { key: '/mode build', desc: 'Switch to BUILD mode (edits enabled)' },
    { key: '/profile', desc: 'List LLM profiles or switch to a registered one (`<id>`)' },
    { key: '/provider openai', desc: 'Hot-swap LLM provider (`gemini|openai|anthropic`)' },
    { key: '/model <name>', desc: 'Change the LLM model of the active target' },
    { key: '/effort high', desc: 'Set LLM reasoning effort (`auto|low|medium|high`)' },
    { key: '/undo', desc: 'Revert latest reversible Git checkpoint' },
    { key: '/compact', desc: 'Compact older turns into verified summary' },
    { key: '/fork', desc: 'Branch snapshots into sibling folder' },
    { key: '/rewind', desc: 'List milestones or jump to <tick>' },
    { key: '/time', desc: 'TimeFreeze: pause here (freeze) or resume at present' },
    { key: '/go', desc: 'Return to present tick and resume (unfreeze)' },
    { key: '/step', desc: 'Advance one deterministic tick without LLM (`/step 1`)' },
    { key: '/step-llm', desc: 'Advance one LLM turn while frozen' },
    { key: '/read', desc: 'Read a workspace file into context (safe under freeze)' },
    { key: '/inject', desc: 'Stage a memory entry without spending a turn (safe under freeze)' },
    { key: '/graph inspect mem', desc: 'Inspect root memory entries with their seq ids' },
    { key: '/graph drop-mem', desc: 'Drop a root memory entry by seq (`/graph drop-mem <seq>`)' },
    { key: '/graph edit-mem', desc: 'Edit a root memory entry (`/graph edit-mem <seq> <texto>`)' },
    { key: '/graph set-goal', desc: 'Replace the root agent goal (`/graph set-goal <texto>`)' },
    { key: '/services', desc: 'Show sprout.toml services status' },
    { key: '/service start all', desc: 'Start all enabled Sprout services' },
    { key: '/service stop all', desc: 'Stop all running Sprout services' },
    { key: '/trust', desc: 'Approve workspace config and sprout.toml' },
    { key: '/trust status', desc: 'Inspect Workspace Trust blake3 hashes' },
    { key: '/trust revoke', desc: 'Revoke Workspace Trust for this repo' },
    { key: '/new', desc: 'Start a fresh root agent conversation' },
  ];

  const dom = {
    brandOrbitBtn: document.getElementById('brand-orbit-btn'),
    brandLogo: document.getElementById('brand-logo'),
    connPill: document.getElementById('conn-pill'),
    llmConfigBtn: document.getElementById('llm-config-btn'),
    llmModal: document.getElementById('llm-modal'),
    llmModalClose: document.getElementById('llm-modal-close'),
    llmModalCancel: document.getElementById('llm-modal-cancel'),
    llmModalApply: document.getElementById('llm-modal-apply'),
    llmProfileList: document.getElementById('llm-profile-list'),
    llmProviderSelect: document.getElementById('llm-provider-select'),
    llmEffortSelect: document.getElementById('llm-effort-select'),
    llmModelInput: document.getElementById('llm-model-input'),
    llmModalStatus: document.getElementById('llm-modal-status'),
    modeBtn: document.getElementById('mode-btn'),
    sandboxPill: document.getElementById('sandbox-pill'),
    trustPill: document.getElementById('trust-pill'),
    agentsBtn: document.getElementById('agents-btn'),
    tickPill: document.getElementById('tick-pill'),
    // `SDD-07` Fase 4A: Time Debugger bar (TimeFreeze) + diálogo de edición de memoria.
    timeDebugBar: document.getElementById('time-debug-bar'),
    tdbStatus: document.getElementById('tdb-status'),
    tdbPrev: document.getElementById('tdb-prev'),
    tdbSlider: document.getElementById('tdb-slider'),
    tdbNext: document.getElementById('tdb-next'),
    tdbFreeze: document.getElementById('tdb-freeze'),
    tdbStep: document.getElementById('tdb-step'),
    tdbStepLlm: document.getElementById('tdb-step-llm'),
    tdbResume: document.getElementById('tdb-resume'),
    tdbRevertWarn: document.getElementById('tdb-revert-warn'),
    tdbError: document.getElementById('tdb-error'),
    tdbBench: document.getElementById('tdb-bench'),
    tdbBenchHint: document.getElementById('tdb-bench-hint'),
    tdbBenchCopy: document.getElementById('tdb-bench-copy'),
    editMemDialog: document.getElementById('edit-mem-dialog'),
    editMemSeq: document.getElementById('edit-mem-seq'),
    editMemInput: document.getElementById('edit-mem-input'),
    editMemError: document.getElementById('edit-mem-error'),
    editMemCancel: document.getElementById('edit-mem-cancel'),
    editMemSave: document.getElementById('edit-mem-save'),
    editMemClose: document.getElementById('edit-mem-close'),
    vaultPill: document.getElementById('vault-pill'),
    ctxBarFill: document.getElementById('ctx-bar-fill'),
    ctxLabel: document.getElementById('ctx-label'),
    themeBtn: document.getElementById('theme-btn'),
    logoutBtn: document.getElementById('logout-btn'),
    workspaceShell: document.getElementById('workspace-shell'),
    mobileTabs: Array.from(document.querySelectorAll('.mobile-tab')),
    agentEmblemWrap: document.getElementById('agent-emblem-wrap'),
    agentEmblem: document.getElementById('agent-emblem'),
    agentBusyProgress: document.getElementById('agent-busy-progress'),
    agentBannerName: document.getElementById('agent-banner-name'),
    agentBannerGoal: document.getElementById('agent-banner-goal'),
    stopBtn: document.getElementById('stop-btn'),
    traceModeBtn: document.getElementById('trace-mode-btn'),
    quickUndoBtn: document.getElementById('quick-undo-btn'),
    quickCompactBtn: document.getElementById('quick-compact-btn'),
    quickForkBtn: document.getElementById('quick-fork-btn'),
    messages: document.getElementById('messages'),
    approvalDock: document.getElementById('approval-dock'),
    approvalCard: document.querySelector('.approval-card'),
    approvalTool: document.getElementById('approval-tool'),
    approvalAgent: document.getElementById('approval-agent'),
    approvalImpact: document.getElementById('approval-impact'),
    approvalArgs: document.getElementById('approval-args'),
    approvalDiff: document.getElementById('approval-diff'),
    approveBtn: document.getElementById('approve-btn'),
    denyBtn: document.getElementById('deny-btn'),
    queueBar: document.getElementById('queue-bar'),
    queueLabel: document.getElementById('queue-label'),
    queueClearBtn: document.getElementById('queue-clear-btn'),
    autocomplete: document.getElementById('autocomplete'),
    composerForm: document.getElementById('composer-form'),
    driverSelect: document.getElementById('driver-select'),
    promptInput: document.getElementById('prompt-input'),
    sendBtn: document.getElementById('send-btn'),
    planBadge: document.getElementById('plan-badge'),
    planBody: document.getElementById('plan-body'),
    servicesBody: document.getElementById('services-body'),
    svcStartAll: document.getElementById('svc-start-all'),
    svcStopAll: document.getElementById('svc-stop-all'),
    subagentsCard: document.getElementById('subagents-card'),
    subagentsBadge: document.getElementById('subagents-badge'),
    subagentsBody: document.getElementById('subagents-body'),
    costBadge: document.getElementById('cost-badge'),
    telIn: document.getElementById('tel-in'),
    telCache: document.getElementById('tel-cache'),
    telOut: document.getElementById('tel-out'),
    telThink: document.getElementById('tel-think'),
    loginDialog: document.getElementById('login-dialog'),
    loginForm: document.getElementById('login-form'),
    apiKeyInput: document.getElementById('api-key-input'),
    loginError: document.getElementById('login-error'),
    agentDialog: document.getElementById('agent-dialog'),
    agentDialogClose: document.getElementById('agent-dialog-close'),
    agentList: document.getElementById('agent-list'),
    trustDialog: document.getElementById('trust-dialog'),
    trustDialogClose: document.getElementById('trust-dialog-close'),
    trustCancelBtn: document.getElementById('trust-cancel-btn'),
    trustApproveBtn: document.getElementById('trust-approve-btn'),
    goalDialog: document.getElementById('goal-dialog'),
    goalDialogClose: document.getElementById('goal-dialog-close'),
    goalDialogText: document.getElementById('goal-dialog-text'),
  };

  let state = null;
  let liveStream = null;
  let focusedGoalFull = '';
  let eventSource = null;
  let acItems = [];
  let acSelected = 0;
  let acTriggerKind = null;
  let acQueryTimer = null;
  let traceMode = 'compact'; // 'compact' (default: crisp tags, no thought wall) | 'full' (all expanded + live thoughts)
  let lastHistoryKey = '';
  const traceOpenOverrides = new Map(); // key `${agentId}:${seq}` -> boolean

  const historyContainer = document.createElement('div');
  historyContainer.className = 'messages-history';
  const liveSlot = document.createElement('div');
  liveSlot.className = 'live-activity-slot';
  dom.messages.append(historyContainer, liveSlot);

  // Reusable live activity nodes so the orbital logo never resets animation during stream ticks
  const liveCard = {
    root: null,
    phaseLabel: null,
    meta: null,
    thoughtPre: null,
    body: null,
    lastStreamText: '',
  };

  // Fallback for light-dismiss on browsers lacking native <dialog closedby="any">
  if (!('closedBy' in HTMLDialogElement.prototype)) {
    const outside = (e, dlg) => {
      const r = dlg.getBoundingClientRect();
      return e.clientX < r.left || e.clientX > r.right || e.clientY < r.top || e.clientY > r.bottom;
    };
    for (const dlg of document.querySelectorAll('dialog[closedby="any"]')) {
      let downOutside = false;
      dlg.addEventListener('pointerdown', (e) => { downOutside = outside(e, dlg); });
      dlg.addEventListener('pointerup', (e) => {
        if (downOutside && outside(e, dlg)) dlg.close();
      });
    }
  }

  for (const dlg of [dom.loginDialog, dom.agentDialog, dom.trustDialog, dom.goalDialog, dom.llmModal]) {
    dlg.addEventListener('close', () => { dlg.returnValue = ''; });
  }

  // Accessible pause/resume control for orbital logo rotation (WCAG 2.2.2)
  function applyMotionPaused(paused) {
    if (paused) {
      document.documentElement.setAttribute('data-motion', 'paused');
      dom.brandOrbitBtn.setAttribute('aria-label', 'Resume logo rotation');
    } else {
      document.documentElement.removeAttribute('data-motion');
      dom.brandOrbitBtn.setAttribute('aria-label', 'Pause logo rotation');
    }
    try { localStorage.setItem('onyx_motion_paused', paused ? '1' : '0'); } catch (_) {}
  }

  let savedMotionPaused = false;
  try { savedMotionPaused = localStorage.getItem('onyx_motion_paused') === '1'; } catch (_) {}
  applyMotionPaused(savedMotionPaused);

  dom.brandOrbitBtn.addEventListener('click', () => {
    const isPaused = document.documentElement.getAttribute('data-motion') === 'paused';
    applyMotionPaused(!isPaused);
  });

  // Trace visibility mode (Compact by default -> Full on click)
  function applyTraceMode(mode, clearOverrides) {
    traceMode = mode === 'full' ? 'full' : 'compact';
    dom.traceModeBtn.textContent = traceMode === 'full' ? 'Trace: Full' : 'Trace: Compact';
    dom.traceModeBtn.classList.toggle('is-active', traceMode === 'full');
    if (clearOverrides) traceOpenOverrides.clear();
    try { localStorage.setItem('onyx_trace_mode', traceMode); } catch (_) {}
    if (state) {
      lastHistoryKey = '';
      renderState(state);
    }
  }

  let savedTraceMode = 'compact';
  try { savedTraceMode = localStorage.getItem('onyx_trace_mode') || 'compact'; } catch (_) {}
  applyTraceMode(savedTraceMode, false);

  dom.traceModeBtn.addEventListener('click', () => {
    applyTraceMode(traceMode === 'compact' ? 'full' : 'compact', true);
  });

  // Theme management (Auto -> Dark -> Light)
  const THEMES = ['auto', 'dark', 'light'];
  function applyTheme(choice) {
    const root = document.documentElement;
    if (choice === 'dark' || choice === 'light') {
      root.setAttribute('data-theme', choice);
    } else {
      root.removeAttribute('data-theme');
    }
    const labels = { auto: '◐', dark: '☾', light: '☀' };
    dom.themeBtn.textContent = labels[choice] || '◐';
    dom.themeBtn.title = `Theme: ${choice} (click to switch)`;
    try { localStorage.setItem('onyx_theme', choice); } catch (_) {}
  }

  let savedTheme = 'auto';
  try { savedTheme = localStorage.getItem('onyx_theme') || 'auto'; } catch (_) {}
  applyTheme(THEMES.includes(savedTheme) ? savedTheme : 'auto');

  dom.themeBtn.addEventListener('click', () => {
    const current = document.documentElement.getAttribute('data-theme') || 'auto';
    const next = THEMES[(THEMES.indexOf(current) + 1) % THEMES.length];
    applyTheme(next);
  });

  // Mobile Tabs
  for (const tabBtn of dom.mobileTabs) {
    tabBtn.addEventListener('click', () => {
      const tab = tabBtn.dataset.tab;
      dom.workspaceShell.dataset.activeTab = tab;
      for (const b of dom.mobileTabs) {
        b.classList.toggle('is-active', b === tabBtn);
      }
    });
  }

  async function apiJson(path, opts = {}) {
    const headers = Object.assign({ 'Content-Type': 'application/json' }, opts.headers || {});
    const res = await fetch(path, Object.assign({}, opts, { headers, credentials: 'same-origin' }));
    if (res.status === 401) {
      openLoginModal();
      throw new Error('Unauthorized');
    }
    const text = await res.text();
    let parsed = null;
    try { parsed = text ? JSON.parse(text) : null; } catch (_) {}
    if (!res.ok) {
      throw new Error((parsed && parsed.error) || text || `HTTP ${res.status}`);
    }
    return parsed;
  }

  function openLoginModal() {
    if (eventSource) {
      eventSource.close();
      eventSource = null;
    }
    setConnPill('Locked', false);
    if (!dom.loginDialog.open) {
      dom.loginError.textContent = '';
      dom.loginDialog.showModal();
      dom.apiKeyInput.focus();
    }
  }

  function setConnPill(label, isLive) {
    dom.connPill.textContent = label;
    dom.connPill.classList.toggle('pill-live', isLive);
    dom.connPill.classList.toggle('pill-muted', !isLive);
  }

  function connectSse() {
    if (eventSource) eventSource.close();
    setConnPill('Connecting', false);
    const es = new EventSource('/api/v1/events');
    eventSource = es;

    es.addEventListener('open', () => {
      setConnPill('Live', true);
    });

    es.addEventListener('state', (evt) => {
      try {
        const next = JSON.parse(evt.data);
        state = next;
        liveStream = next.stream_preview || null;
        renderState(next);
      } catch (_) {}
    });

    es.addEventListener('stream', (evt) => {
      try {
        liveStream = JSON.parse(evt.data);
        if (!state) return;
        const agents = state.agents || [];
        const focused = agents.find((a) => a.id === state.focused_agent_id) || agents[0] || null;
        const wasNearBottom = (dom.messages.scrollHeight - dom.messages.scrollTop - dom.messages.clientHeight) < 88;
        renderLiveActivity(focused, liveStream, state.tick || 0);
        if (wasNearBottom) {
          dom.messages.scrollTop = dom.messages.scrollHeight;
        }
      } catch (_) {}
    });

    es.addEventListener('error', async () => {
      setConnPill('Reconnecting', false);
      try {
        const probe = await fetch('/api/v1/state', { credentials: 'same-origin' });
        if (probe.status === 401) {
          openLoginModal();
        }
      } catch (_) {}
    });
  }

  // `SDD-07` Fase 4A: Time Debugger (TimeFreeze) — paridad con el TUI.
  // Scrubs efímeros vía `POST /api/v1/time/scrub` (0 ticks, 0 `ONYXTR2`);
  // el push SSE inmediato de `pump_idle` trae el snapshot `time_debug`
  // fresco (preview proyección + `pending_revert_files` de `live_effects`).
  let tdbAvailable = []; // ticks scrubeables (ascendente), último snapshot
  let tdbUserClosed = false; // el usuario cerró la barra manualmente

  function timeDebugOf() {
    return (state && state.time_debug) || null;
  }

  function scrub(action, extra) {
    return apiJson('/api/v1/time/scrub', {
      method: 'POST',
      body: JSON.stringify(Object.assign({ action }, extra || {})),
    });
  }

  // El slider cruza `available_ticks` (no contiguos): el valor se "snapea"
  // al tick scrubeable más alto ≤ al valor (si no existe, al primero).
  function scrubToNearest(raw) {
    if (!tdbAvailable.length) return null;
    for (let i = tdbAvailable.length - 1; i >= 0; i--) {
      if (tdbAvailable[i] <= raw) return tdbAvailable[i];
    }
    return tdbAvailable[0];
  }

  let tdbErrorTimer = null;
  function showTdbError(msg) {
    if (!dom.tdbError) return;
    dom.tdbError.textContent = String(msg);
    dom.tdbError.classList.remove('hidden');
    if (tdbErrorTimer) clearTimeout(tdbErrorTimer);
    tdbErrorTimer = setTimeout(() => {
      dom.tdbError.classList.add('hidden');
      dom.tdbError.textContent = '';
    }, 8000);
  }

  async function onScrub(fn) {
    try {
      await fn();
    } catch (err) {
      // Errores accionables (tick purgado/futuro → 400, prompt plano → 409…).
      showTdbError(err.message || String(err));
    }
  }

  function openTdb() {
    dom.timeDebugBar.classList.remove('hidden');
    dom.tickPill.setAttribute('aria-expanded', 'true');
  }

  function closeTdb() {
    dom.timeDebugBar.classList.add('hidden');
    dom.tickPill.setAttribute('aria-expanded', 'false');
    tdbUserClosed = true;
  }

  function renderTimeDebugBar(s) {
    const td = (s && s.time_debug) || null;
    const presentTick = s.tick || 0;
    if (!td) {
      // Sesión sin depurador temporal (p. ej. headless): pill plano.
      dom.tickPill.textContent = `tick ${presentTick}`;
      dom.tickPill.classList.remove('is-frozen');
      dom.timeDebugBar.classList.add('hidden');
      dom.tickPill.setAttribute('aria-expanded', 'false');
      return;
    }

    const frozen = Boolean(td.frozen);
    const cursor = Number(td.cursor_tick || 0);
    const present = Number(td.present_tick || presentTick);
    tdbAvailable = Array.isArray(td.available_ticks) ? td.available_ticks.map(Number) : [];

    dom.tickPill.textContent = frozen ? `⏸ ${cursor}/${present}` : `tick ${present}`;
    dom.tickPill.classList.toggle('is-frozen', frozen);
    dom.tdbStatus.textContent = frozen
      ? `⏸ TIME FROZEN · tick ${cursor} / present ${present}`
      : `LIVE · tick ${present}`;
    dom.tdbStatus.classList.toggle('is-frozen', frozen);

    const min = tdbAvailable.length ? tdbAvailable[0] : 0;
    const max = Math.max(present, min);
    dom.tdbSlider.min = String(min);
    dom.tdbSlider.max = String(max);
    dom.tdbSlider.value = String(Math.min(cursor, max));
    dom.tdbSlider.disabled = cursor >= present;

    // `Freeze` / `Present` según estado (auto-congela si el cursor ya ancla).
    const anchored = frozen || cursor < present;
    dom.tdbFreeze.textContent = anchored ? 'Present' : 'Freeze';

    // Aviso ámbar: archivos que se revertirían al bifurcar desde el cursor
    // (fuente: `live_effects` del grafo vivo, 0 I/O).
    const files = Array.isArray(td.pending_revert_files) ? td.pending_revert_files : [];
    if (files.length) {
      dom.tdbRevertWarn.textContent =
        `⚠ branch from cursor reverts ${files.length} file(s): ` +
        files.slice(0, 6).join(', ') +
        (files.length > 6 ? ` (+${files.length - 6} more)` : '');
      dom.tdbRevertWarn.classList.remove('hidden');
    } else {
      dom.tdbRevertWarn.classList.add('hidden');
    }

    // Sinergia `SDD-05` §9.4: bench determinista copiable desde este cursor.
    if (td.bench_hint) {
      dom.tdbBenchHint.textContent = td.bench_hint;
      dom.tdbBench.classList.remove('hidden');
    } else {
      dom.tdbBench.classList.add('hidden');
    }

    // Auto-abre la barra si está congelado o anclado en el pasado (una vez;
    // el cierre manual la respeta hasta que vuelva a present).
    const shouldAutoOpen = frozen || cursor < present;
    if (shouldAutoOpen && !tdbUserClosed) openTdb();
    if (!shouldAutoOpen) tdbUserClosed = false;
  }

  function setupTimeDebugBar() {
    dom.tickPill.addEventListener('click', () => {
      if (dom.timeDebugBar.classList.contains('hidden')) openTdb();
      else closeTdb();
    });
    dom.tdbPrev.addEventListener('click', () =>
      onScrub(() => scrub('scrub_delta', { delta: -1 }))
    );
    dom.tdbNext.addEventListener('click', () =>
      onScrub(() => scrub('scrub_delta', { delta: 1 }))
    );
    dom.tdbSlider.addEventListener('change', () => {
      const target = scrubToNearest(Number(dom.tdbSlider.value));
      if (target == null) return;
      onScrub(() => scrub('scrub_to', { tick: target }));
    });
    dom.tdbFreeze.addEventListener('click', () => {
      const td = timeDebugOf();
      const anchored = Boolean(
        td && (td.frozen || Number(td.cursor_tick || 0) < Number(td.present_tick || 0))
      );
      onScrub(() => scrub(anchored ? 'return_to_present' : 'freeze'));
    });
    // Step / Step LLM / Resume van por el túnel de comandos sellado (mismo
    // `SessionAction` que el TUI: gate de mutación + bifurcación al confirmar).
    dom.tdbStep.addEventListener('click', () => onScrub(() => sendCommand('/step 1')));
    dom.tdbStepLlm.addEventListener('click', () => onScrub(() => sendCommand('/step-llm')));
    dom.tdbResume.addEventListener('click', () => onScrub(() => sendCommand('/go')));
    dom.tdbBenchCopy.addEventListener('click', async () => {
      const cmd = dom.tdbBenchHint.textContent || '';
      let ok = false;
      try {
        await navigator.clipboard.writeText(cmd);
        ok = true;
      } catch (_) {
        ok = false;
      }
      dom.tdbBenchCopy.textContent = ok ? 'Copied' : 'Copy failed';
      setTimeout(() => {
        dom.tdbBenchCopy.textContent = 'Copy';
      }, 1500);
    });
  }
  setupTimeDebugBar();

  // `SDD-07` Fase 4A: memarias del root bajo TIME FROZEN — ✎ edita (vía
  // diálogo) y × suelta; ambos encuelan el slash sellado correspondiente
  // (`/graph edit-mem` / `/graph drop-mem`) por el túnel autenticado de
  // comandos (0 efectos hasta el tick confirmado, 0 `ONYXTR2`).
  let editMemSeq = 0;
  function openEditMemDialog(seq, currentText) {
    editMemSeq = Number(seq);
    dom.editMemSeq.textContent = `#${editMemSeq}`;
    dom.editMemInput.value = String(currentText || '').replace(/\s*\n\s*/g, ' ').slice(0, 1000);
    dom.editMemError.textContent = '';
    dom.editMemDialog.showModal();
    dom.editMemInput.focus();
  }

  function attachMemActions(card, seq, text) {
    const wrap = document.createElement('div');
    wrap.className = 'mem-wrap';
    wrap.append(card);

    const actions = document.createElement('div');
    actions.className = 'mem-actions';

    const editBtn = document.createElement('button');
    editBtn.type = 'button';
    editBtn.className = 'mem-btn';
    editBtn.textContent = '✎';
    editBtn.title = `Edit memory #${seq} (/graph edit-mem ${seq} <texto>)`;
    editBtn.setAttribute('aria-label', `Edit memory ${seq}`);
    editBtn.addEventListener('click', (e) => {
      e.preventDefault();
      e.stopPropagation();
      openEditMemDialog(seq, text);
    });

    const dropBtn = document.createElement('button');
    dropBtn.type = 'button';
    dropBtn.className = 'mem-btn mem-btn-drop';
    dropBtn.textContent = '×';
    dropBtn.title = `Drop memory #${seq} (/graph drop-mem ${seq})`;
    dropBtn.setAttribute('aria-label', `Drop memory ${seq}`);
    dropBtn.addEventListener('click', (e) => {
      e.preventDefault();
      e.stopPropagation();
      const ok = window.confirm(
        `Drop root memory #${seq}? Stages /graph drop-mem ${seq} (takes effect on the next confirmed tick).`
      );
      if (!ok) return;
      onScrub(() => sendCommand(`/graph drop-mem ${seq}`));
    });

    actions.append(editBtn, dropBtn);
    wrap.append(actions);
    return wrap;
  }

  dom.editMemSave.addEventListener('click', async () => {
    const raw = dom.editMemInput.value.replace(/\s*\n\s*/g, ' ').trim();
    if (!raw) {
      dom.editMemError.textContent = 'Memory text cannot be empty';
      return;
    }
    try {
      await sendCommand(`/graph edit-mem ${editMemSeq} ${raw}`);
      dom.editMemDialog.close();
    } catch (err) {
      dom.editMemError.textContent = err.message || String(err);
    }
  });
  dom.editMemCancel.addEventListener('click', () => dom.editMemDialog.close());
  dom.editMemClose.addEventListener('click', () => dom.editMemDialog.close());

  async function bootstrap() {
    try {
      const initial = await apiJson('/api/v1/state', { method: 'GET' });
      state = initial;
      liveStream = initial.stream_preview || null;
      renderState(initial);
      connectSse();
    } catch (_) {
      openLoginModal();
    }
  }

  dom.loginForm.addEventListener('submit', async (e) => {
    e.preventDefault();
    dom.loginError.textContent = '';
    const key = dom.apiKeyInput.value.trim();
    if (!key) return;
    try {
      const res = await fetch('/api/v1/auth/login', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        credentials: 'same-origin',
        body: JSON.stringify({ api_key: key }),
      });
      if (!res.ok) {
        const err = await res.json().catch(() => ({}));
        dom.loginError.textContent = err.error || 'Invalid API key';
        dom.apiKeyInput.select();
        return;
      }
      dom.apiKeyInput.value = '';
      dom.loginDialog.close();
      await bootstrap();
    } catch (err) {
      dom.loginError.textContent = `Connection failed: ${err.message}`;
    }
  });

  dom.logoutBtn.addEventListener('click', async () => {
    try {
      await fetch('/api/v1/auth/logout', { method: 'POST', credentials: 'same-origin' });
    } catch (_) {}
    openLoginModal();
  });

  async function sendCommand(command) {
    const trimmed = command.trim();
    if (!trimmed) return;
    await apiJson('/api/v1/command', {
      method: 'POST',
      body: JSON.stringify({ command: trimmed }),
    });
  }

  // `SDD-10` Fase 4: el composer refleja el driver seleccionado: `native` → "Send" (turno
  // de root); driver externo → "Spawn" + placeholder indicando que la tarea corre como
  // subagente en background sin gastar turno de root.
  const NATIVE_PLACEHOLDER = 'Ask openOnyx, type / for commands, or @ for workspace files…';
  function updateComposerDriverUI() {
    const driver = dom.driverSelect.value || 'native';
    const spawning = driver !== 'native';
    dom.sendBtn.textContent = spawning ? 'Spawn' : 'Send';
    dom.sendBtn.setAttribute('aria-label', spawning ? 'Spawn background task' : 'Send message');
    dom.promptInput.placeholder = spawning
      ? `Background task for ${driver} (no root turn)…`
      : NATIVE_PLACEHOLDER;
  }

  // `SDD-07` Fase 4A: bajo TIME FROZEN el composer avisa de que los prompts
  // planos serán rechazados (409) y sugiere las vías de escape selladas.
  const FROZEN_PLACEHOLDER =
    'TIME FROZEN: /go to resume · /inject to stage memory · /step-llm · /graph…';
  function updateComposerFreezeUI() {
    const td = timeDebugOf();
    const frozen = Boolean(td && td.frozen);
    if (frozen) {
      dom.promptInput.placeholder = FROZEN_PLACEHOLDER;
    } else {
      updateComposerDriverUI();
    }
  }
  dom.driverSelect.addEventListener('change', updateComposerDriverUI);
  updateComposerDriverUI();

  dom.composerForm.addEventListener('submit', async (e) => {
    e.preventDefault();
    const text = dom.promptInput.value.trim();
    if (!text) return;
    // `SDD-10` Fase 4: driver no nativo + texto libre (no slash) → `/spawn --driver <id> <task>`,
    // sin gastar un turno de root. Un slash command explítico siempre se envía tal cual.
    const driver = dom.driverSelect.value || 'native';
    const command =
      driver !== 'native' && !text.startsWith('/') ? `/spawn --driver ${driver} ${text}` : text;
    closeAutocomplete();
    dom.promptInput.value = '';
    autoResizeTextarea();
    let sent = false;
    try {
      await sendCommand(command);
      sent = true;
    } catch (_) {}
    if (sent) {
      // La selección de driver es de un solo uso: tras el envío exitoso vuelve a `native`.
      dom.driverSelect.value = 'native';
      updateComposerDriverUI();
    }
  });

  let renderedDriversKey = null;
  function renderDriverSelect(s) {
    const drivers = Array.isArray(s.drivers) && s.drivers.length > 0 ? s.drivers : ['native'];
    const key = drivers.join('\u0000');
    if (key === renderedDriversKey) return;
    renderedDriversKey = key;
    const prev = dom.driverSelect.value;
    dom.driverSelect.replaceChildren();
    for (const d of drivers) {
      const opt = document.createElement('option');
      opt.value = d;
      opt.textContent = d;
      dom.driverSelect.append(opt);
    }
    if (drivers.includes(prev)) dom.driverSelect.value = prev;
    updateComposerDriverUI();
  }

  dom.modeBtn.addEventListener('click', () => {
    if (!state) return;
    const next = state.mode === 'BUILD' ? '/mode plan' : '/mode build';
    sendCommand(next).catch(() => {});
  });

  dom.stopBtn.addEventListener('click', () => sendCommand('/cancel').catch(() => {}));
  dom.quickUndoBtn.addEventListener('click', () => sendCommand('/undo').catch(() => {}));
  dom.quickCompactBtn.addEventListener('click', () => sendCommand('/compact').catch(() => {}));
  dom.quickForkBtn.addEventListener('click', () => sendCommand('/fork').catch(() => {}));
  dom.svcStartAll.addEventListener('click', () => sendCommand('/service start all').catch(() => {}));
  dom.svcStopAll.addEventListener('click', () => sendCommand('/service stop all').catch(() => {}));

  dom.queueClearBtn.addEventListener('click', () => {
    apiJson('/api/v1/queue/clear', { method: 'POST', body: '{}' }).catch(() => {});
  });

  dom.approveBtn.addEventListener('click', () => submitApproval(true));
  dom.denyBtn.addEventListener('click', () => submitApproval(false));

  async function submitApproval(approve) {
    if (!state || !state.pending_approval) return;
    const p = state.pending_approval;
    await apiJson('/api/v1/approval', {
      method: 'POST',
      body: JSON.stringify({
        agent_id: p.agent_id,
        request: p.request,
        approve,
        reason: approve ? '' : 'denied by user in Web UI',
      }),
    }).catch(() => {});
  }

  // Agent Selector Dialog
  dom.agentsBtn.addEventListener('click', () => {
    if (!state) return;
    renderAgentPicker(state);
    dom.agentDialog.showModal();
  });
  dom.agentDialogClose.addEventListener('click', () => dom.agentDialog.close());

  // LLM target selector (`SDD-03` Fase 4): el objetivo sale del snapshot SSE
  // (fuente live `LlmProfileHandle`); los cambios se aplican con los slash
  // estándar por `/api/v1/command` — sin endpoints nuevos, sellado en
  // ONYXTR2 por el mismo túnel que la TUI.
  let llmProfilesKey = '';
  function llmProfilesKeyOf(s) {
    const p = Array.isArray(s.profiles) ? s.profiles : [];
    // `s.profile_id` en la clave: si cambia el perfil activo (hot-swap con
    // el modal abierto) se refresca el resaltado `.is-focused`.
    return (
      `${s.profile_id || ''}|` +
      p
        .map((x) => `${x.id}|${x.provider}|${x.model}|${x.effort}|${x.available ? 1 : 0}|${x.unavailable_reason || ''}`)
        .join(';')
    );
  }

  function renderLlmProfiles(s) {
    // Idempotente: no reconstruir si los datos de perfiles no cambiaron.
    const key = llmProfilesKeyOf(s);
    if (key === llmProfilesKey) return;
    llmProfilesKey = key;
    dom.llmProfileList.replaceChildren();
    const profiles = Array.isArray(s.profiles) ? s.profiles : [];
    if (profiles.length === 0) {
      const empty = document.createElement('p');
      empty.className = 'dialog-sub';
      empty.textContent = 'No registered profiles — using the CLI startup target.';
      dom.llmProfileList.append(empty);
      return;
    }
    for (const p of profiles) {
      const btn = document.createElement('button');
      btn.type = 'button';
      btn.className = 'agent-row-btn llm-profile-btn';
      if (p.id === s.profile_id) btn.classList.add('is-focused');
      const title = document.createElement('span');
      title.className = 'agent-row-title';
      title.textContent = p.id;
      const meta = document.createElement('span');
      meta.className = 'agent-row-goal';
      meta.textContent = `${p.provider}/${p.model} · effort:${p.effort}`;
      btn.append(title, meta);
      if (!p.available) {
        btn.disabled = true;
        btn.title = p.unavailable_reason || 'Profile unavailable';
      } else {
        btn.addEventListener('click', () => {
          applyLlmCommands([`/profile ${p.id}`]);
        });
      }
      dom.llmProfileList.append(btn);
    }
  }

  function syncLlmProviderAvailability(s) {
    const providers = Array.isArray(s.available_providers) ? s.available_providers : null;
    for (const opt of dom.llmProviderSelect.options) {
      opt.disabled = !!(providers && providers.length) && !providers.includes(opt.value);
    }
  }

  // Solo al abrir el modal: puebla provider/effort/modelo desde el snapshot
  // actual (con la sesión en reposo no hay push SSE que lo haga).
  function populateLlmModalInputs(s) {
    dom.llmProviderSelect.value = s.provider || 'gemini';
    dom.llmEffortSelect.value = s.effort || 'auto';
    dom.llmModelInput.value = s.model || '';
  }

  function renderLlmModal(s) {
    // La etiqueta del botón es de solo lectura: siempre al día (hot-swap).
    // Sin `provider` (snapshot sin handle, perfil CLI degradado) no se
    // antepone el prefijo literal `provider/`.
    dom.llmConfigBtn.textContent =
      `${s.provider ? `${s.provider}/${s.model || 'model'}` : (s.model || 'model')} · effort:${s.effort || 'auto'}`;
    if (!dom.llmModal.open) return;
    // Re-render idempotente (push SSE con el dialog abierto): lista de
    // perfiles + disponibilidad de providers. **Nunca** se tocan los valores
    // del formulario — si el usuario cambia Provider a `openai` y mueve el
    // foco, el siguiente push no revierte su edición a espaldas (los valores
    // solo se pueblan al abrir, en `populateLlmModalInputs`).
    renderLlmProfiles(s);
    syncLlmProviderAvailability(s);
  }

  async function applyLlmCommands(commands) {
    for (const c of commands) {
      try {
        await sendCommand(c);
      } catch (_) {
        if (dom.llmModal.open) dom.llmModalStatus.textContent = `Command failed: ${c}`;
        return;
      }
    }
    if (dom.llmModal.open) {
      dom.llmModalStatus.textContent = '';
      dom.llmModal.close();
    }
  }

  dom.llmConfigBtn.addEventListener('click', () => {
    if (!state) return;
    llmProfilesKey = '';
    dom.llmModalStatus.textContent = '';
    dom.llmModalApply.disabled = false;
    dom.llmModal.showModal();
    // Poblar al abrir: con la sesión en reposo no llega push SSE.
    populateLlmModalInputs(state);
    syncLlmProviderAvailability(state);
    renderLlmProfiles(state);
  });
  dom.llmModalClose.addEventListener('click', () => dom.llmModal.close());
  dom.llmModalCancel.addEventListener('click', () => dom.llmModal.close());
  dom.llmModal.addEventListener('close', () => { llmProfilesKey = ''; });
  dom.llmModalApply.addEventListener('click', () => {
    if (!state) return;
    const s = state;
    const commands = [];
    const provider = dom.llmProviderSelect.value;
    const effort = dom.llmEffortSelect.value;
    const model = dom.llmModelInput.value.trim();
    // Solo los campos modificados — cada uno es un slash estándar.
    if (provider && provider !== s.provider) commands.push(`/provider ${provider}`);
    if (model && model !== s.model) commands.push(`/model ${model}`);
    if (effort && effort !== (s.effort || 'auto')) commands.push(`/effort ${effort}`);
    if (commands.length === 0) {
      dom.llmModalStatus.textContent = 'No changes to apply.';
      return;
    }
    dom.llmModalApply.disabled = true;
    applyLlmCommands(commands).finally(() => {
      dom.llmModalApply.disabled = false;
    });
  });

  // Workspace Trust Dialog
  dom.trustPill.addEventListener('click', () => dom.trustDialog.showModal());
  dom.trustDialogClose.addEventListener('click', () => dom.trustDialog.close());
  dom.trustCancelBtn.addEventListener('click', () => dom.trustDialog.close());
  dom.trustApproveBtn.addEventListener('click', () => {
    dom.trustDialog.close();
    sendCommand('/trust').catch(() => {});
  });

  // Full Goal Inspection Dialog (strict 1-line banner, expands only on intentional click)
  dom.agentBannerGoal.addEventListener('click', () => {
    if (!focusedGoalFull) return;
    dom.goalDialogText.textContent = focusedGoalFull;
    dom.goalDialog.showModal();
  });
  dom.goalDialogClose.addEventListener('click', () => dom.goalDialog.close());

  // Textarea ergonomics + `/` & `@` autocomplete
  function autoResizeTextarea() {
    dom.promptInput.style.height = 'auto';
    dom.promptInput.style.height = `${Math.min(dom.promptInput.scrollHeight, 160)}px`;
  }

  dom.promptInput.addEventListener('input', () => {
    autoResizeTextarea();
    updateAutocomplete();
  });

  dom.promptInput.addEventListener('keydown', (e) => {
    if (!dom.autocomplete.classList.contains('hidden')) {
      if (e.key === 'ArrowDown') {
        e.preventDefault();
        acSelected = (acSelected + 1) % acItems.length;
        renderAutocomplete();
        return;
      }
      if (e.key === 'ArrowUp') {
        e.preventDefault();
        acSelected = (acSelected - 1 + acItems.length) % acItems.length;
        renderAutocomplete();
        return;
      }
      if (e.key === 'Tab' || (e.key === 'Enter' && !e.shiftKey)) {
        e.preventDefault();
        acceptAutocomplete(acSelected);
        return;
      }
      if (e.key === 'Escape') {
        e.preventDefault();
        closeAutocomplete();
        return;
      }
    }
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault();
      dom.composerForm.requestSubmit();
    }
  });

  function closeAutocomplete() {
    acItems = [];
    acTriggerKind = null;
    dom.autocomplete.classList.add('hidden');
    dom.autocomplete.replaceChildren();
  }

  function updateAutocomplete() {
    const val = dom.promptInput.value;
    const pos = dom.promptInput.selectionStart ?? val.length;
    const before = val.slice(0, pos);

    if (before.startsWith('/') && !before.includes(' ')) {
      const q = before.toLowerCase();
      acTriggerKind = '/';
      acItems = SLASH_HINTS.filter((h) => h.key.toLowerCase().startsWith(q));
      acSelected = 0;
      if (acItems.length > 0) renderAutocomplete();
      else closeAutocomplete();
      return;
    }

    const atMatch = /(^|\s)@([^\s@]*)$/.exec(before);
    if (atMatch) {
      const query = atMatch[2];
      acTriggerKind = '@';
      clearTimeout(acQueryTimer);
      acQueryTimer = setTimeout(async () => {
        try {
          const data = await apiJson(`/api/v1/files?q=${encodeURIComponent(query)}`, { method: 'GET' });
          acItems = (data.files || []).map((f) => ({ key: f, desc: 'workspace file' }));
          acSelected = 0;
          if (acItems.length > 0) renderAutocomplete();
          else closeAutocomplete();
        } catch (_) {
          closeAutocomplete();
        }
      }, 90);
      return;
    }

    closeAutocomplete();
  }

  function renderAutocomplete() {
    dom.autocomplete.replaceChildren();
    acItems.forEach((item, idx) => {
      const btn = document.createElement('button');
      btn.type = 'button';
      btn.className = idx === acSelected ? 'ac-item is-selected' : 'ac-item';
      btn.setAttribute('role', 'option');
      btn.setAttribute('aria-selected', idx === acSelected ? 'true' : 'false');

      const k = document.createElement('span');
      k.className = 'ac-key';
      k.textContent = acTriggerKind === '@' ? `@${item.key}` : item.key;

      const d = document.createElement('span');
      d.className = 'ac-desc';
      d.textContent = item.desc;

      btn.append(k, d);
      btn.addEventListener('click', () => acceptAutocomplete(idx));
      dom.autocomplete.append(btn);
    });
    dom.autocomplete.classList.remove('hidden');
  }

  function acceptAutocomplete(idx) {
    const item = acItems[idx];
    if (!item) return;
    const val = dom.promptInput.value;
    const pos = dom.promptInput.selectionStart ?? val.length;
    const before = val.slice(0, pos);
    const after = val.slice(pos);

    if (acTriggerKind === '/') {
      dom.promptInput.value = `${item.key} ${after}`.trimStart();
    } else if (acTriggerKind === '@') {
      const replaced = before.replace(/(^|\s)@[^\s@]*$/, `$1@${item.key} `);
      dom.promptInput.value = replaced + after;
    }
    closeAutocomplete();
    dom.promptInput.focus();
    autoResizeTextarea();
  }

  function formatTokens(n) {
    if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`;
    if (n >= 1_000) return `${(n / 1_000).toFixed(1)}k`;
    return String(n || 0);
  }

  function appendFormattedProse(parent, text) {
    const parts = String(text || '').split(/```/);
    for (let i = 0; i < parts.length; i++) {
      const chunk = parts[i];
      if (i % 2 === 1) {
        const nl = chunk.indexOf('\n');
        const codeText = nl >= 0 ? chunk.slice(nl + 1) : chunk;
        const pre = document.createElement('pre');
        pre.className = 'code-block';
        const code = document.createElement('code');
        code.textContent = codeText.replace(/\n$/, '');
        pre.append(code);
        parent.append(pre);
      } else if (chunk.trim()) {
        const p = document.createElement('p');
        const inlineParts = chunk.split(/(`[^`\n]+`)/g);
        for (const seg of inlineParts) {
          if (seg.startsWith('`') && seg.endsWith('`') && seg.length > 2) {
            const c = document.createElement('code');
            c.className = 'inline-code';
            c.textContent = seg.slice(1, -1);
            p.append(c);
          } else {
            p.append(document.createTextNode(seg));
          }
        }
        parent.append(p);
      }
    }
  }

  function buildSubagentCard(job, showTitlePrefix) {
    const card = document.createElement('article');
    card.className = 'bg-subagent-card';

    const header = document.createElement('header');
    const title = document.createElement('span');
    title.className = 'bg-subagent-id';
    // `SDD-10` Fase 4: badge del driver en tarjetas externas (`[bg #job_id] → opencode`);
    // el driver nativo es el default y no se muestra.
    const driverSuffix = job.driver && job.driver !== 'native' ? ` → ${job.driver}` : '';
    title.textContent = `${showTitlePrefix ? '[bg #' : '#'}${job.job_id}${showTitlePrefix ? ']' : ''}${driverSuffix}`;
    const badge = document.createElement('span');
    badge.className = 'pill pill-mono';
    if (job.status === 'done' || job.status === 'merged') badge.classList.add('pill-live');
    else if (job.status === 'failed') badge.classList.add('pill-danger');
    badge.textContent = job.status;
    header.append(title, badge);

    const filesCount = Array.isArray(job.touched_files) ? job.touched_files.length : 0;
    const meta = document.createElement('p');
    meta.className = 'bg-subagent-meta';
    meta.textContent = `${job.branch || ''} · ${filesCount} file(s)${job.goal ? ` · ${job.goal}` : ''}`;

    card.append(header, meta);

    const canMerge = job.status === 'done' || (job.status === 'failed' && filesCount > 0);
    const canDiscard = ['running', 'done', 'failed'].includes(job.status);
    if (canMerge || canDiscard) {
      const footer = document.createElement('footer');
      if (canMerge) {
        const mergeBtn = document.createElement('button');
        mergeBtn.type = 'button';
        mergeBtn.className = 'chip-btn is-active';
        mergeBtn.textContent = 'Merge';
        mergeBtn.title = `3-way merge ${job.job_id} into root`;
        mergeBtn.addEventListener('click', () => {
          dom.agentDialog.close();
          sendCommand(`/merge ${job.job_id}`).catch(() => {});
        });
        footer.append(mergeBtn);
      }
      if (canDiscard) {
        const discardBtn = document.createElement('button');
        discardBtn.type = 'button';
        discardBtn.className = 'chip-btn chip-btn-stop';
        discardBtn.textContent = job.status === 'running' ? 'Stop' : 'Discard';
        discardBtn.title = `Discard worktree ${job.job_id}`;
        discardBtn.addEventListener('click', () => {
          dom.agentDialog.close();
          sendCommand(`/discard ${job.job_id}`).catch(() => {});
        });
        footer.append(discardBtn);
      }
      card.append(footer);
    }
    return card;
  }

  function renderAgentPicker(s) {
    dom.agentList.replaceChildren();
    const hasGraphAgents = Array.isArray(s.agents) && s.agents.length > 0;
    const hasBg = Array.isArray(s.subagents) && s.subagents.length > 0;
    if (!hasGraphAgents && !hasBg) {
      const p = document.createElement('p');
      p.className = 'empty-note';
      p.textContent = 'No active agents yet.';
      dom.agentList.append(p);
      return;
    }
    for (const ag of (s.agents || [])) {
      const btn = document.createElement('button');
      btn.type = 'button';
      btn.className = ag.id === s.focused_agent_id ? 'agent-row-btn is-focused' : 'agent-row-btn';

      const left = document.createElement('div');
      const title = document.createElement('div');
      title.className = 'agent-row-title';
      title.textContent = `#${ag.id} ${ag.name} (${ag.is_root ? 'root' : 'sub'})`;
      const goal = document.createElement('div');
      goal.className = 'agent-row-goal';
      goal.textContent = ag.goal;
      left.append(title, goal);

      const badge = document.createElement('span');
      badge.className = 'pill pill-mono';
      badge.textContent = ag.state;

      btn.append(left, badge);
      btn.addEventListener('click', async () => {
        dom.agentDialog.close();
        await apiJson('/api/v1/focus', {
          method: 'POST',
          body: JSON.stringify({ agent_id: ag.id }),
        }).catch(() => {});
      });
      dom.agentList.append(btn);
    }
    if (hasBg) {
      for (const job of s.subagents) {
        dom.agentList.append(buildSubagentCard(job, true));
      }
    }
  }

  function renderState(s) {
    // Top bar — `SDD-03` Fase 4: botón LLM vivo (etiqueta + modal idempotente)
    renderLlmModal(s);
    dom.modeBtn.textContent = s.mode || 'BUILD';
    dom.modeBtn.classList.toggle('is-plan', s.mode !== 'BUILD');
    dom.sandboxPill.textContent = `SANDBOX:${s.sandbox_badge || 'workspace'}`;
    dom.trustPill.classList.toggle('hidden', !s.untrusted);
    // `SDD-07` Fase 4A: Time Debugger (TimeFreeze) — pill + barra ámbar.
    renderTimeDebugBar(s);
    updateComposerFreezeUI();
    renderDriverSelect(s);

    if (s.vault_label) {
      dom.vaultPill.textContent = `vault:${s.vault_label}`;
      dom.vaultPill.classList.remove('hidden');
    } else {
      dom.vaultPill.classList.add('hidden');
    }

    const agents = s.agents || [];
    const bgJobs = Array.isArray(s.subagents) ? s.subagents : [];
    const activeBgCount = bgJobs.filter((j) => j.status === 'running').length;
    const subCount = agents.filter((a) => !a.is_root).length;
    const focused = agents.find((a) => a.id === s.focused_agent_id) || agents[0] || null;
    const agentParts = [`Agent #${focused ? focused.id : 1}`];
    if (subCount > 0) agentParts.push(`sub:${subCount}`);
    if (activeBgCount > 0) agentParts.push(`bg:${activeBgCount}`);
    dom.agentsBtn.textContent = agentParts.join(' · ');

    // Any agent busy rotates the real orbital openOnyx emblem (like the TUI!)
    const anyActive = agents.some((a) =>
      ['thinking', 'acting', 'waiting_children', 'waiting_approval'].includes(a.state)
    );
    const anyBusy = !s.pending_approval && agents.some((a) =>
      ['thinking', 'acting', 'waiting_children'].includes(a.state)
    );
    dom.brandLogo.classList.toggle('onyx-spinning', anyBusy);
    dom.brandOrbitBtn.classList.toggle('is-busy', anyBusy);
    dom.stopBtn.classList.toggle('hidden', !anyActive);
    const hasSubActive = agents.some((a) =>
      !a.is_root && ['thinking', 'acting', 'waiting_children', 'waiting_approval'].includes(a.state)
    );
    dom.stopBtn.textContent = hasSubActive ? 'Stop sub' : 'Stop';

    // Context telemetry
    const tel = s.telemetry || {};
    const pct = Math.min(100, Math.max(0, Number(tel.context_fill_pct || 0)));
    dom.ctxBarFill.style.inlineSize = `${pct}%`;
    dom.ctxBarFill.classList.toggle('is-warn', pct >= 60 && pct < 80);
    dom.ctxBarFill.classList.toggle('is-high', pct >= 80);
    dom.ctxLabel.textContent = `ctx ${pct}%`;
    dom.costBadge.textContent = `$${Number(tel.estimated_cost_usd || 0).toFixed(4)}`;
    dom.telIn.textContent = formatTokens(tel.input_tokens);
    dom.telCache.textContent = formatTokens(tel.cached_tokens);
    dom.telOut.textContent = formatTokens(tel.output_tokens);
    dom.telThink.textContent = formatTokens(tel.thinking_tokens);

    // Agent banner emblem
    if (focused) {
      const busy = ['thinking', 'acting', 'waiting_children'].includes(focused.state);
      const waitingApproval = focused.state === 'waiting_approval';
      dom.agentEmblemWrap.classList.toggle('is-busy', busy);
      dom.agentEmblemWrap.classList.toggle('is-approval', waitingApproval);
      dom.agentEmblem.classList.toggle('onyx-spinning', busy);
      dom.agentBusyProgress.classList.toggle('hidden', !busy);
      dom.agentBannerName.textContent = `#${focused.id} ${focused.name} [${focused.state}]`;
      focusedGoalFull = (focused.goal || '').trim();
      const singleLineGoal = focusedGoalFull.replace(/\s+/g, ' ');
      dom.agentBannerGoal.textContent = singleLineGoal || 'Active session';
      dom.agentBannerGoal.disabled = !focusedGoalFull;
      dom.agentBannerGoal.title = focusedGoalFull
        ? 'Click to view full goal'
        : '';
    } else {
      dom.agentEmblemWrap.classList.remove('is-busy', 'is-approval');
      dom.agentEmblem.classList.remove('onyx-spinning');
      dom.agentBusyProgress.classList.add('hidden');
      dom.agentBannerName.textContent = 'Ready';
      focusedGoalFull = '';
      dom.agentBannerGoal.textContent = 'Type a goal or slash command below.';
      dom.agentBannerGoal.disabled = true;
      dom.agentBannerGoal.title = '';
    }

    // Queue bar
    const queued = s.queued_prompts || [];
    if (queued.length > 0) {
      dom.queueLabel.textContent = `queued:${queued.length} — next: "${queued[0]}"`;
      dom.queueBar.classList.remove('hidden');
    } else {
      dom.queueBar.classList.add('hidden');
    }

    // Approval dock
    if (s.pending_approval) {
      const p = s.pending_approval;
      dom.approvalTool.textContent = p.tool;
      dom.approvalAgent.textContent = `agent #${p.agent_id} ${p.agent_name || ''}`.trim();
      dom.approvalArgs.textContent = p.args || '';

      const isHard = Boolean(p.impact && p.impact.hard_threshold_hit);
      dom.approvalCard.classList.toggle('is-hard-impact', isHard);
      if (p.impact && p.impact.summary_line) {
        dom.approvalImpact.textContent = p.impact.summary_line;
        dom.approvalImpact.classList.toggle('is-hard', isHard);
        dom.approvalImpact.classList.remove('hidden');
      } else {
        dom.approvalImpact.classList.add('hidden');
      }

      if (p.diff) {
        dom.approvalDiff.replaceChildren();
        for (const rawLine of p.diff.split('\n')) {
          const lineEl = document.createElement('div');
          lineEl.className = 'diff-line';
          if (rawLine.startsWith('+') && !rawLine.startsWith('+++')) lineEl.classList.add('is-add');
          else if (rawLine.startsWith('-') && !rawLine.startsWith('---')) lineEl.classList.add('is-del');
          else if (rawLine.startsWith('@@')) lineEl.classList.add('is-hunk');
          lineEl.textContent = rawLine;
          dom.approvalDiff.append(lineEl);
        }
        dom.approvalDiff.classList.remove('hidden');
      } else {
        dom.approvalDiff.classList.add('hidden');
      }
      dom.approvalDock.classList.remove('hidden');
    } else {
      dom.approvalDock.classList.add('hidden');
    }

    // Sidebar: Plan & Services
    if (s.plan && s.plan.trim()) {
      dom.planBadge.textContent = 'Active';
      dom.planBadge.classList.add('pill-live');
      const pre = document.createElement('pre');
      pre.className = 'plan-lines';
      pre.textContent = s.plan;
      dom.planBody.replaceChildren(pre);
    } else {
      dom.planBadge.textContent = 'Idle';
      dom.planBadge.classList.remove('pill-live');
      const p = document.createElement('p');
      p.className = 'empty-note';
      p.textContent = 'No active plan steps yet.';
      dom.planBody.replaceChildren(p);
    }

    if (s.services && s.services.trim()) {
      const pre = document.createElement('pre');
      pre.className = 'services-lines';
      pre.textContent = s.services;
      dom.servicesBody.replaceChildren(pre);
    } else {
      const p = document.createElement('p');
      p.className = 'empty-note';
      p.textContent = 'No sprout.toml services loaded.';
      dom.servicesBody.replaceChildren(p);
    }

    if (bgJobs.length > 0) {
      dom.subagentsCard.classList.remove('hidden');
      dom.subagentsBadge.textContent = `${activeBgCount} active`;
      dom.subagentsBadge.classList.toggle('pill-live', activeBgCount > 0);
      const list = document.createElement('div');
      list.className = 'bg-subagents-list';
      for (const job of bgJobs) {
        list.append(buildSubagentCard(job, false));
      }
      dom.subagentsBody.replaceChildren(list);
    } else {
      dom.subagentsCard.classList.add('hidden');
      dom.subagentsBody.replaceChildren();
    }

    // Conversation & transparent trace stream
    const wasNearBottom = (dom.messages.scrollHeight - dom.messages.scrollTop - dom.messages.clientHeight) < 88;
    renderMessages(focused, liveStream || s.stream_preview, s.tick || 0);
    if (wasNearBottom) {
      dom.messages.scrollTop = dom.messages.scrollHeight;
    }
  }

  function summarizeToolCall(rawText) {
    const trimmed = String(rawText || '').trim();
    let toolName = '';
    let argsRaw = '';
    let parsedObj = null;

    if (trimmed.startsWith('{')) {
      try {
        const obj = JSON.parse(trimmed);
        if (obj && typeof obj === 'object') {
          toolName = obj.tool || obj.name || '';
          parsedObj = obj.args && typeof obj.args === 'object' ? obj.args : obj;
        }
      } catch (_) {}
    }

    if (!toolName) {
      const m = /^([a-zA-Z0-9_.:-]+)(?:\s+([\s\S]*))?$/.exec(trimmed);
      if (m) {
        toolName = m[1];
        argsRaw = (m[2] || '').trim();
        if (argsRaw.startsWith('{')) {
          try { parsedObj = JSON.parse(argsRaw); } catch (_) {}
        }
      }
    }

    let summary = '';
    if (parsedObj && typeof parsedObj === 'object') {
      const p = parsedObj.path || parsedObj.file || parsedObj.rel_path || '';
      if (p) {
        if (parsedObj.start_line || parsedObj.end_line) {
          summary = `${p} (L${parsedObj.start_line || 1}–L${parsedObj.end_line || '…'})`;
        } else if (Array.isArray(parsedObj.edits)) {
          summary = `${p} (${parsedObj.edits.length} edits)`;
        } else {
          summary = String(p);
        }
      } else if (parsedObj.command) {
        summary = String(parsedObj.command);
      } else if (parsedObj.symbol) {
        summary = String(parsedObj.symbol);
      } else if (parsedObj.pattern || parsedObj.query) {
        summary = String(parsedObj.pattern || parsedObj.query);
      } else if (parsedObj.name) {
        summary = String(parsedObj.name);
      } else if (parsedObj.goal) {
        summary = String(parsedObj.goal);
      } else if (parsedObj.service || parsedObj.id) {
        summary = String(parsedObj.service || parsedObj.id);
      } else if (Array.isArray(parsedObj.steps)) {
        summary = `${parsedObj.steps.length} plan steps`;
      } else {
        const keys = Object.keys(parsedObj).filter((k) => k !== 'tool' && k !== 'name');
        summary = keys.map((k) => `${k}=${JSON.stringify(parsedObj[k])}`).join(' ');
      }
    } else {
      summary = argsRaw.split('\n')[0] || trimmed.split('\n')[0];
    }

    const formattedPayload = parsedObj
      ? JSON.stringify(parsedObj, null, 2)
      : trimmed;

    return {
      toolName,
      summary: summary.slice(0, 140),
      payload: formattedPayload,
    };
  }

  function summarizeObservation(rawText) {
    const text = String(rawText || '');
    const lines = text.split('\n').filter((l) => l.trim().length > 0);
    const first = (lines[0] || '(empty output)').trim();
    const suffix = lines.length > 1 ? ` · ${lines.length} lines` : '';
    return {
      summary: `${first.slice(0, 115)}${suffix}`,
      payload: text,
    };
  }

  function renderMessages(focused, streamPreview, currentTick) {
    const memory = (focused && Array.isArray(focused.memory)) ? focused.memory : [];
    const lastEntry = memory.length > 0 ? memory[memory.length - 1] : null;
    // `SDD-07` Fase 4A: el estado de freeze entra en la key para que los
    // botones ✎/× de memoria aparezcan/desaparezcan al congelar/descongelar.
    const td = (state && state.time_debug) || null;
    const memFrozen = Boolean(td && td.frozen);
    const canEditMem = Boolean(focused && focused.is_root && memFrozen);
    const historyKey = focused
      ? `${focused.id}|${focused.goal}|${memory.length}|${lastEntry ? lastEntry.seq : 0}|${lastEntry ? (lastEntry.text || '').length : 0}|${traceMode}|${memFrozen ? 'F' : 'L'}`
      : 'empty';

    if (historyKey !== lastHistoryKey) {
      lastHistoryKey = historyKey;
      historyContainer.replaceChildren();

      if (!focused || (!memory.length && !focused.goal)) {
        const card = document.createElement('div');
        card.className = 'welcome-card';
        const logo = document.createElement('img');
        logo.className = 'welcome-logo';
        logo.src = '/logo.jpg';
        logo.alt = 'openOnyx';
        logo.width = 68;
        logo.height = 68;
        const h2 = document.createElement('h2');
        h2.textContent = 'openOnyx Ready';
        const p = document.createElement('p');
        p.textContent = 'Deterministic coding agent with transparent execution trace, Scent impact analysis, Sprout orchestration, and Session Vault snapshots.';
        const prompts = document.createElement('div');
        prompts.className = 'welcome-prompts';
        for (const sample of ['Summarize this repository architecture', '/services', '/mode plan']) {
          const b = document.createElement('button');
          b.type = 'button';
          b.className = 'chip-btn';
          b.textContent = sample;
          b.addEventListener('click', () => sendCommand(sample).catch(() => {}));
          prompts.append(b);
        }
        card.append(logo, h2, p, prompts);
        historyContainer.append(card);
      } else {
        if (focused.goal) {
          historyContainer.append(buildMessageCard('user', 'You', focused.goal, false));
        }

        for (let i = 0; i < memory.length; i++) {
          const entry = memory[i];
          const kind = entry.kind || 'trace';
          const text = entry.text || '';
          const seq = entry.seq != null ? entry.seq : i;
          const tick = entry.tick != null ? entry.tick : 0;

          let card;
          if (kind === 'user') {
            card = buildMessageCard('user', 'You', text, false);
          } else if (kind === 'say' || kind === 'result') {
            card = buildMessageCard('assistant', 'openOnyx', text, false);
          } else if (kind === 'system') {
            card = buildSystemCard(text, tick);
          } else {
            card = buildToolCard(focused.id, seq, kind, text, tick);
          }

          // `SDD-07` Fase 4A: memarias del root con `seq >= 0` exponen
          // ✎/× solo bajo TIME FROZEN (sellado vía `/graph edit-mem|drop-mem`).
          if (canEditMem && Number(seq) >= 0) {
            card = attachMemActions(card, Number(seq), text);
          }
          historyContainer.append(card);
        }
      }
    }

    renderLiveActivity(focused, streamPreview, currentTick);
  }

  function ensureLiveCard() {
    if (liveCard.root) return liveCard;

    const card = document.createElement('article');
    card.className = 'trace-live-card';

    const head = document.createElement('div');
    head.className = 'trace-live-head';

    const orb = document.createElement('img');
    orb.className = 'msg-stream-orb onyx-spinning';
    orb.src = '/logo.jpg';
    orb.alt = '';
    orb.setAttribute('aria-hidden', 'true');
    orb.width = 16;
    orb.height = 16;

    const phaseLabel = document.createElement('span');
    const meta = document.createElement('span');
    meta.className = 'trace-live-meta';

    head.append(orb, phaseLabel, meta);

    const thoughtPre = document.createElement('pre');
    thoughtPre.className = 'live-thought hidden';

    const body = document.createElement('div');
    body.className = 'msg-body hidden';

    card.append(head, thoughtPre, body);

    liveCard.root = card;
    liveCard.phaseLabel = phaseLabel;
    liveCard.meta = meta;
    liveCard.thoughtPre = thoughtPre;
    liveCard.body = body;
    liveCard.lastStreamText = '';
    return liveCard;
  }

  function renderLiveActivity(focused, streamPreview, currentTick) {
    if (!focused) {
      if (liveSlot.childNodes.length) liveSlot.replaceChildren();
      liveCard.lastStreamText = '';
      return;
    }

    const isBusy = ['thinking', 'acting', 'waiting_children'].includes(focused.state);
    const hasStream = Boolean(
      streamPreview &&
      streamPreview.agent_id === focused.id &&
      (streamPreview.text || streamPreview.thought || streamPreview.thought_chars > 0)
    );

    if (!isBusy && !hasStream) {
      if (liveSlot.childNodes.length) liveSlot.replaceChildren();
      liveCard.lastStreamText = '';
      return;
    }

    const lc = ensureLiveCard();
    if (lc.root.parentNode !== liveSlot) {
      liveSlot.replaceChildren(lc.root);
    }

    const thoughtChars = hasStream
      ? Number(streamPreview.thought_chars || (streamPreview.thought ? streamPreview.thought.length : 0))
      : 0;
    const streamText = hasStream && streamPreview.text ? String(streamPreview.text) : '';
    const streamThought = hasStream && streamPreview.thought ? String(streamPreview.thought) : '';

    if (focused.state === 'acting') {
      lc.phaseLabel.textContent = 'Executing tool…';
    } else if (focused.state === 'waiting_children') {
      lc.phaseLabel.textContent = 'Waiting for subagent(s)…';
    } else if (streamText) {
      lc.phaseLabel.textContent = 'openOnyx · streaming…';
    } else if (thoughtChars > 0) {
      lc.phaseLabel.textContent = traceMode === 'full'
        ? `openOnyx · thinking (${formatTokens(thoughtChars)} chars)…`
        : `Thinking (${formatTokens(thoughtChars)} chars) · switch to Full to inspect`;
    } else {
      lc.phaseLabel.textContent = 'Thinking…';
    }

    const stepText = focused.max_steps > 0
      ? `step ${focused.steps || 0}/${focused.max_steps} · `
      : '';
    lc.meta.textContent = `${stepText}t${currentTick}`;

    // Option A: raw thought text is rendered ONLY on-demand in `Trace: Full` mode
    if (traceMode === 'full' && streamThought) {
      if (lc.thoughtPre.textContent !== streamThought) {
        lc.thoughtPre.textContent = streamThought;
        lc.thoughtPre.scrollTop = lc.thoughtPre.scrollHeight;
      }
      lc.thoughtPre.classList.remove('hidden');
    } else {
      lc.thoughtPre.classList.add('hidden');
    }

    // Live streamed answer text (or `[calling tool...]` hint) is always lightweight and helpful
    if (streamText) {
      if (lc.lastStreamText !== streamText) {
        lc.lastStreamText = streamText;
        lc.body.replaceChildren();
        appendFormattedProse(lc.body, streamText);
      }
      lc.body.classList.remove('hidden');
    } else {
      lc.lastStreamText = '';
      lc.body.classList.add('hidden');
    }
  }

  function buildSystemCard(text, tick) {
    const div = document.createElement('div');
    div.className = 'msg msg-system';

    const tag = document.createElement('span');
    tag.className = 'tool-tag tool-tag-system';
    tag.textContent = 'system';

    const body = document.createElement('span');
    body.className = 'msg-system-text';
    body.textContent = text;

    div.append(tag, body);
    if (tick > 0) {
      const tickSpan = document.createElement('span');
      tickSpan.className = 'tool-tick';
      tickSpan.textContent = `t${tick}`;
      div.append(tickSpan);
    }
    return div;
  }

  function buildMessageCard(role, label, text, spinningOrb) {
    const el = document.createElement('article');
    el.className = role === 'user' ? 'msg msg-user' : 'msg msg-assistant';
    const meta = document.createElement('div');
    meta.className = 'msg-meta';
    if (spinningOrb) {
      const orb = document.createElement('img');
      orb.className = 'msg-stream-orb onyx-spinning';
      orb.src = '/logo.jpg';
      orb.alt = '';
      orb.setAttribute('aria-hidden', 'true');
      orb.width = 16;
      orb.height = 16;
      meta.append(orb);
    }
    const labelSpan = document.createElement('span');
    labelSpan.textContent = label;
    meta.append(labelSpan);
    const body = document.createElement('div');
    body.className = 'msg-body';
    appendFormattedProse(body, text);
    el.append(meta, body);
    return el;
  }

  function buildToolCard(agentId, seq, kind, text, tick) {
    const isError = kind === 'tool_error' || kind === 'error' || kind === 'denied';
    const isObserve = kind === 'observe';
    const isCall = kind === 'call';

    const details = document.createElement('details');
    details.className = 'tool-card';
    if (isObserve) details.classList.add('is-observe');
    if (isError) details.classList.add('is-error');

    const cardKey = `${agentId}:${seq}`;
    const defaultOpen = traceMode === 'full' || isError;
    const isOpen = traceOpenOverrides.has(cardKey)
      ? traceOpenOverrides.get(cardKey)
      : defaultOpen;
    details.open = Boolean(isOpen);

    details.addEventListener('toggle', () => {
      traceOpenOverrides.set(cardKey, details.open);
    });

    const summary = document.createElement('summary');

    const chevron = document.createElement('span');
    chevron.className = 'tool-chevron';
    chevron.setAttribute('aria-hidden', 'true');
    chevron.textContent = '▸';

    const tag = document.createElement('span');
    tag.className = 'tool-tag';
    tag.textContent = kind === 'tool_error' ? 'error' : kind;

    summary.append(chevron, tag);

    let payloadText = text;
    if (isCall) {
      const parsed = summarizeToolCall(text);
      if (parsed.toolName) {
        const nameSpan = document.createElement('span');
        nameSpan.className = 'tool-name';
        nameSpan.textContent = parsed.toolName;
        summary.append(nameSpan);
      }
      const preview = document.createElement('span');
      preview.className = 'tool-preview';
      preview.textContent = parsed.summary;
      summary.append(preview);
      payloadText = parsed.payload;
    } else if (isObserve) {
      const obs = summarizeObservation(text);
      const preview = document.createElement('span');
      preview.className = 'tool-preview';
      preview.textContent = obs.summary;
      summary.append(preview);
      payloadText = obs.payload;
    } else {
      const preview = document.createElement('span');
      preview.className = 'tool-preview';
      preview.textContent = String(text || '').split('\n')[0].slice(0, 140);
      summary.append(preview);
    }

    if (tick > 0) {
      const tickSpan = document.createElement('span');
      tickSpan.className = 'tool-tick';
      tickSpan.textContent = `t${tick}`;
      summary.append(tickSpan);
    }

    const payload = document.createElement('pre');
    payload.className = 'tool-payload';
    payload.textContent = payloadText;

    details.append(summary, payload);
    return details;
  }

  bootstrap();
})();
