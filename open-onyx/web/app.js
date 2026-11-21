// openOnyx Web UI Client — 100% DOM-safe (createElement + textContent only)
// Compatible with strict CSP `script-src 'self'; require-trusted-types-for 'script'`.

(() => {
  'use strict';

  const SLASH_HINTS = [
    { key: '/cancel', desc: 'Stop active subagent or interrupt current turn' },
    { key: '/cancel all', desc: 'Interrupt both subagents and root turn' },
    { key: '/mode plan', desc: 'Switch to read-only PLAN mode' },
    { key: '/mode build', desc: 'Switch to BUILD mode (edits enabled)' },
    { key: '/undo', desc: 'Revert latest reversible Git checkpoint' },
    { key: '/compact', desc: 'Compact older turns into verified summary' },
    { key: '/fork', desc: 'Branch snapshots into sibling folder' },
    { key: '/rewind', desc: 'List milestones or jump to <tick>' },
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
    modelSubtitle: document.getElementById('model-subtitle'),
    modeBtn: document.getElementById('mode-btn'),
    sandboxPill: document.getElementById('sandbox-pill'),
    trustPill: document.getElementById('trust-pill'),
    agentsBtn: document.getElementById('agents-btn'),
    tickPill: document.getElementById('tick-pill'),
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
    promptInput: document.getElementById('prompt-input'),
    sendBtn: document.getElementById('send-btn'),
    planBadge: document.getElementById('plan-badge'),
    planBody: document.getElementById('plan-body'),
    servicesBody: document.getElementById('services-body'),
    svcStartAll: document.getElementById('svc-start-all'),
    svcStopAll: document.getElementById('svc-stop-all'),
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

  for (const dlg of [dom.loginDialog, dom.agentDialog, dom.trustDialog, dom.goalDialog]) {
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

  dom.composerForm.addEventListener('submit', async (e) => {
    e.preventDefault();
    const text = dom.promptInput.value.trim();
    if (!text) return;
    closeAutocomplete();
    dom.promptInput.value = '';
    autoResizeTextarea();
    try {
      await sendCommand(text);
    } catch (_) {}
  });

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

  function renderAgentPicker(s) {
    dom.agentList.replaceChildren();
    if (!s.agents || s.agents.length === 0) {
      const p = document.createElement('p');
      p.className = 'empty-note';
      p.textContent = 'No active agents yet.';
      dom.agentList.append(p);
      return;
    }
    for (const ag of s.agents) {
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
  }

  function renderState(s) {
    // Top bar
    dom.modelSubtitle.textContent = `${s.model || 'model'} · effort:${s.effort || 'auto'}`;
    dom.modeBtn.textContent = s.mode || 'BUILD';
    dom.modeBtn.classList.toggle('is-plan', s.mode !== 'BUILD');
    dom.sandboxPill.textContent = `SANDBOX:${s.sandbox_badge || 'workspace'}`;
    dom.trustPill.classList.toggle('hidden', !s.untrusted);
    dom.tickPill.textContent = `tick ${s.tick || 0}`;

    if (s.vault_label) {
      dom.vaultPill.textContent = `vault:${s.vault_label}`;
      dom.vaultPill.classList.remove('hidden');
    } else {
      dom.vaultPill.classList.add('hidden');
    }

    const agents = s.agents || [];
    const subCount = agents.filter((a) => !a.is_root).length;
    const focused = agents.find((a) => a.id === s.focused_agent_id) || agents[0] || null;
    dom.agentsBtn.textContent = subCount > 0
      ? `Agent #${focused ? focused.id : 1} · sub:${subCount}`
      : `Agent #${focused ? focused.id : 1}`;

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
    const historyKey = focused
      ? `${focused.id}|${focused.goal}|${memory.length}|${lastEntry ? lastEntry.seq : 0}|${lastEntry ? (lastEntry.text || '').length : 0}|${traceMode}`
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

          if (kind === 'user') {
            historyContainer.append(buildMessageCard('user', 'You', text, false));
          } else if (kind === 'say' || kind === 'result') {
            historyContainer.append(buildMessageCard('assistant', 'openOnyx', text, false));
          } else if (kind === 'system') {
            historyContainer.append(buildSystemCard(text, tick));
          } else {
            historyContainer.append(buildToolCard(focused.id, seq, kind, text, tick));
          }
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
