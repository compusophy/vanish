// vanish autonomous self-editing coding harness client
// all labels, logs, tool outputs, and rendering enforced in lowercase

document.addEventListener('DOMContentLoaded', () => {
  const THREADS_KEY = 'vanish_threads';
  const ACTIVE_THREAD_KEY = 'vanish_active_thread';

  const state = {
    activeTab: 'agent',
    activeFile: null,
    fileContentOriginal: '',
    isAgentRunning: false,
    abortController: null,
    history: [],
    config: {
      reasoningEffort: 'high',
      model: 'stealth/ox-alpha'
    },
    loopMode: false,
    threads: {},
    activeThread: null
  };

  try {
    state.loopMode = localStorage.getItem('vanish_loop_mode') === 'true';
  } catch (e) {}

  // ---- chat threads (d4) ----------------------------------------------
  // each thread owns its own conversation history; switching preserves it.
  // threads render as a list in the LEFT sidebar (#thread-list).
  //
  // storage: localStorage primary, in-memory fallback. every failure is
  // surfaced to the ui — a silent catch here is exactly what produced the
  // "empty dropdown doing nothing" bug.

  let storageAvailable = true;
  function probeStorage() {
    try {
      localStorage.setItem('vanish_storage_probe', '1');
      localStorage.removeItem('vanish_storage_probe');
    } catch (e) {
      storageAvailable = false;
    }
  }
  probeStorage();

  function loadThreads() {
    if (!storageAvailable) return {};
    try {
      const parsed = JSON.parse(localStorage.getItem(THREADS_KEY) || '{}');
      if (parsed && typeof parsed === 'object') return parsed;
    } catch (e) {
      console.error('vanish: failed to parse saved threads', e);
    }
    return {};
  }

  function persistThreads() {
    if (!storageAvailable) {
      showStorageWarning();
      return;
    }
    try {
      localStorage.setItem(THREADS_KEY, JSON.stringify(state.threads));
      localStorage.setItem(ACTIVE_THREAD_KEY, String(state.activeThread));
    } catch (e) {
      storageAvailable = false;
      showStorageWarning();
    }
  }

  let storageWarned = false;
  function showStorageWarning() {
    if (storageWarned || !el.agentStepsFeed) return;
    storageWarned = true;
    const warn = document.createElement('div');
    warn.className = 'death-report';
    warn.innerHTML =
      '<div class="death-title">⚠ conversations will not persist</div>' +
      '<div class="death-reason">browser storage is unavailable (private mode or blocked cookies?). threads work this session but are lost on refresh.</div>';
    el.agentStepsFeed.appendChild(warn);
  }

  function currentThread() {
    return state.threads[state.activeThread] || null;
  }

  // renders threads into the left-sidebar conversation list
  function renderThreadList() {
    if (!el.threadList) return;
    el.threadList.innerHTML = '';
    for (const [id, t] of Object.entries(state.threads)) {
      const item = document.createElement('button');
      item.className = `thread-item ${id === state.activeThread ? 'active' : ''}`;
      item.dataset.threadId = id;

      const dot = document.createElement('span');
      dot.className = 'thread-dot';
      dot.textContent = t.history && t.history.length > 0 ? '●' : '○';

      const name = document.createElement('span');
      name.className = 'thread-name';
      name.textContent = t.name.toLowerCase();

      item.appendChild(dot);
      item.appendChild(name);
      item.addEventListener('click', () => switchThread(id));
      el.threadList.appendChild(item);
    }
    if (el.configThreadLabel) {
      el.configThreadLabel.textContent =
        `thread: ${currentThread()?.name.toLowerCase() || 'main'}`;
    }
  }

  function createThread() {
    const id = `t_${Date.now()}_${Math.floor(Math.random() * 1e4)}`;
    state.threads[id] = {
      name: `thread ${Object.keys(state.threads).length + 1}`,
      history: []
    };
    state.activeThread = id;
    state.history = [];
    persistThreads();
    renderThreadList();
    el.agentStepsFeed.innerHTML = '';
    el.agentHero.classList.remove('hidden');
    showToast('new conversation started — previous ones stay listed');
  }

  function switchThread(id) {
    if (!state.threads[id] || state.isAgentRunning || id === state.activeThread) return;
    const cur = currentThread();
    if (cur) cur.history = state.history;

    state.activeThread = id;
    state.history = state.threads[id].history || [];
    persistThreads();
    renderThreadList();
    el.agentStepsFeed.innerHTML = '';
    el.agentHero.classList.toggle('hidden', state.history.length > 0);
    showToast(`switched to ${state.threads[id].name.toLowerCase()}`);
  }

  function initThreads() {
    state.threads = loadThreads();
    if (!state.threads['main']) state.threads['main'] = { name: 'main', history: [] };
    let saved = null;
    try { saved = storageAvailable ? localStorage.getItem(ACTIVE_THREAD_KEY) : null; } catch (e) {}
    state.activeThread = saved && state.threads[saved] ? saved : 'main';
    state.history = state.threads[state.activeThread].history || [];
    persistThreads();
    renderThreadList();
    if (!storageAvailable) showStorageWarning();
  }

  function saveHistory() {
    const cur = currentThread();
    if (cur) {
      cur.history = state.history;
      persistThreads();
    }
  }

  // dom elements
  const el = {
    // tabs
    tabAgent: document.getElementById('tab-agent'),
    tabEditor: document.getElementById('tab-editor'),
    tabDiff: document.getElementById('tab-diff'),
    viewAgent: document.getElementById('view-agent'),
    viewEditor: document.getElementById('view-editor'),
    viewDiff: document.getElementById('view-diff'),
    diffCountBadge: document.getElementById('diff-count-badge'),

    // sidebar & file tree
    sidebar: document.getElementById('sidebar'),
    btnToggleSidebar: document.getElementById('btn-toggle-sidebar'),
    btnRefreshWorkspace: document.getElementById('btn-refresh-workspace'),
    fileTreeList: document.getElementById('file-tree-list'),
    btnNewFile: document.getElementById('btn-new-file'),
    paramEffort: document.getElementById('param-effort'),
    valEffort: document.getElementById('val-effort'),
    threadList: document.getElementById('thread-list'),
    configThreadLabel: document.getElementById('config-thread-label'),
    sidebarGithubLabel: document.getElementById('sidebar-github-label'),
    sidebarVercelLabel: document.getElementById('sidebar-vercel-label'),

    // agent live feed
    agentStreamContainer: document.getElementById('agent-stream-container'),
    agentHero: document.getElementById('agent-hero'),
    agentStepsFeed: document.getElementById('agent-steps-feed'),
    agentPromptInput: document.getElementById('agent-prompt-input'),
    btnAgentRun: document.getElementById('btn-agent-run'),
    btnAgentStop: document.getElementById('btn-agent-stop'),
    dockAgentStatus: document.getElementById('dock-agent-status'),
    chkLoopMode: document.getElementById('chk-loop-mode'),

    // editor
    editorActiveFile: document.getElementById('editor-active-file'),
    editorStatus: document.getElementById('editor-status'),
    btnEditorReload: document.getElementById('btn-editor-reload'),
    btnEditorSave: document.getElementById('btn-editor-save'),
    codeEditorTextarea: document.getElementById('code-editor-textarea'),

    // diff viewer
    diffMeta: document.getElementById('diff-meta'),
    diffCode: document.getElementById('diff-code'),
    btnDiffRefresh: document.getElementById('btn-diff-refresh'),
    commitMsgInput: document.getElementById('commit-msg-input'),
    btnCommitChanges: document.getElementById('btn-commit-changes'),

    // toast
    toastContainer: document.getElementById('toast-container')
  };

  // markdown parser setup
  if (window.marked) {
    window.marked.setOptions({
      highlight: function(code, lang) {
        if (window.hljs && lang && window.hljs.getLanguage(lang)) {
          try { return window.hljs.highlight(code, { language: lang }).value; } catch (err) {}
        }
        if (window.hljs) {
          try { return window.hljs.highlightAuto(code).value; } catch (err) {}
        }
        return code;
      },
      breaks: true,
      gfm: true
    });
  }

  function showToast(message) {
    const toast = document.createElement('div');
    toast.className = 'toast';
    toast.textContent = message.toLowerCase();
    el.toastContainer.appendChild(toast);
    setTimeout(() => {
      toast.style.opacity = '0';
      setTimeout(() => toast.remove(), 200);
    }, 2800);
  }

  function switchTab(tabName) {
    state.activeTab = tabName;
    [el.tabAgent, el.tabEditor, el.tabDiff].forEach(tab => {
      tab.classList.toggle('active', tab.dataset.tab === tabName);
    });
    [el.viewAgent, el.viewEditor, el.viewDiff].forEach(view => {
      view.classList.toggle('active', view.id === `view-${tabName}`);
    });

    if (tabName === 'diff') {
      loadGitDiff();
    }
  }

  // 1. workspace file tree explorer
  async function loadFileTree() {
    try {
      const res = await fetch('/api/files/tree');
      const data = await res.json();
      if (data.success && data.tree) {
        el.fileTreeList.innerHTML = '';
        renderTreeNodes(data.tree, el.fileTreeList);
      }
    } catch (err) {
      console.error('failed to load file tree:', err);
    }
  }

  function renderTreeNodes(nodes, container) {
    nodes.forEach(node => {
      const item = document.createElement('div');
      item.className = 'tree-item-wrapper';

      if (node.type === 'directory') {
        const row = document.createElement('div');
        row.className = 'tree-node';
        row.innerHTML = `
          <span class="tree-icon">📁</span>
          <span>${escapeHtml(node.name).toLowerCase()}</span>
        `;
        const childrenContainer = document.createElement('div');
        childrenContainer.className = 'tree-children';
        renderTreeNodes(node.children || [], childrenContainer);

        row.addEventListener('click', () => {
          childrenContainer.classList.toggle('collapsed');
          row.querySelector('.tree-icon').textContent = childrenContainer.classList.contains('collapsed') ? '📁' : '📂';
        });

        item.appendChild(row);
        item.appendChild(childrenContainer);
      } else {
        const row = document.createElement('div');
        row.className = `tree-node ${state.activeFile === node.path ? 'active-file' : ''}`;
        row.dataset.path = node.path;
        row.innerHTML = `
          <span class="tree-icon">📄</span>
          <span>${escapeHtml(node.name).toLowerCase()}</span>
        `;
        row.addEventListener('click', () => openFileInEditor(node.path));
        item.appendChild(row);
      }

      container.appendChild(item);
    });
  }

  // 2. code editor operations
  async function openFileInEditor(relPath) {
    try {
      const res = await fetch(`/api/files/read?path=${encodeURIComponent(relPath)}`);
      const data = await res.json();
      if (data.success) {
        state.activeFile = relPath;
        state.fileContentOriginal = data.content;
        el.editorActiveFile.textContent = relPath.toLowerCase();
        el.editorStatus.textContent = 'clean';
        el.codeEditorTextarea.value = data.content;
        switchTab('editor');

        // highlight active in file tree
        document.querySelectorAll('.tree-node').forEach(node => {
          node.classList.toggle('active-file', node.dataset.path === relPath);
        });
      } else {
        showToast(`error: ${data.error || 'could not read file'}`);
      }
    } catch (err) {
      showToast('failed to open file');
    }
  }

  async function saveActiveFile() {
    if (!state.activeFile) {
      showToast('no file selected to save');
      return;
    }
    const content = el.codeEditorTextarea.value;
    try {
      const res = await fetch('/api/files/write', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ path: state.activeFile, content })
      });
      const data = await res.json();
      if (data.success) {
        state.fileContentOriginal = content;
        el.editorStatus.textContent = 'saved';
        showToast(`saved ${state.activeFile}`);
        checkGitStatus();
      } else {
        showToast(`save error: ${data.error}`);
      }
    } catch (err) {
      showToast('failed to save file');
    }
  }

  el.codeEditorTextarea.addEventListener('input', () => {
    if (el.codeEditorTextarea.value !== state.fileContentOriginal) {
      el.editorStatus.textContent = 'modified';
    } else {
      el.editorStatus.textContent = 'clean';
    }
  });

  el.btnEditorSave.addEventListener('click', saveActiveFile);
  el.btnEditorReload.addEventListener('click', () => {
    if (state.activeFile) openFileInEditor(state.activeFile);
  });

  // 3. git status & diff viewer
  async function checkGitStatus() {
    try {
      const res = await fetch('/api/git/status');
      const data = await res.json();
      if (data.modified_files && data.modified_files.length > 0) {
        el.diffCountBadge.textContent = data.modified_files.length;
        el.diffCountBadge.classList.remove('hidden');
      } else {
        el.diffCountBadge.classList.add('hidden');
      }
      if (data.branch) {
        el.sidebarGithubLabel.textContent = `compusophy/vanish (${data.branch})`;
      }
    } catch (err) {}
  }

  async function loadGitDiff() {
    el.diffMeta.textContent = 'fetching git diff...';
    try {
      const res = await fetch('/api/git/diff');
      const data = await res.json();
      if (data.diff && data.diff.trim()) {
        el.diffCode.innerHTML = formatDiff(data.diff);
        el.diffMeta.textContent = 'uncommitted modifications';
      } else {
        el.diffCode.textContent = 'no changes detected in git working tree.';
        el.diffMeta.textContent = 'working tree clean';
      }
    } catch (err) {
      el.diffCode.textContent = 'failed to load git diff.';
      el.diffMeta.textContent = 'error';
    }
  }

  function formatDiff(rawDiff) {
    const lines = rawDiff.split('\n');
    return lines.map(line => {
      const escaped = escapeHtml(line).toLowerCase();
      if (line.startsWith('+') && !line.startsWith('+++')) {
        return `<span style="color: var(--text-diff-add); background: rgba(63, 185, 80, 0.1); display: block;">${escaped}</span>`;
      } else if (line.startsWith('-') && !line.startsWith('---')) {
        return `<span style="color: var(--text-diff-del); background: rgba(248, 81, 73, 0.1); display: block;">${escaped}</span>`;
      } else if (line.startsWith('@@')) {
        return `<span style="color: var(--accent-cyan); display: block;">${escaped}</span>`;
      }
      return `<span>${escaped}</span>`;
    }).join('\n');
  }

  el.btnDiffRefresh.addEventListener('click', loadGitDiff);

  el.btnCommitChanges.addEventListener('click', async () => {
    const msg = el.commitMsgInput.value.trim() || 'update harness via vanish';
    try {
      const res = await fetch('/api/git/commit', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ message: msg })
      });
      const data = await res.json();
      if (data.success) {
        showToast('changes committed');
        el.commitMsgInput.value = '';
        loadGitDiff();
        checkGitStatus();
      } else {
        showToast(`commit failed: ${data.error}`);
      }
    } catch (err) {
      showToast('commit failed');
    }
  });

  // 4. autonomous agent loop engine
  async function runAutonomousAgent(promptText) {
    if (!promptText || state.isAgentRunning) return;

    state.isAgentRunning = true;
    switchTab('agent');
    el.agentHero.classList.add('hidden');
    el.btnAgentRun.classList.add('hidden');
    el.btnAgentStop.classList.remove('hidden');
    el.dockAgentStatus.textContent = 'autonomous agent active...';

    state.abortController = new AbortController();

    // show what the human asked for, right above the run's steps
    const userBubble = document.createElement('div');
    userBubble.className = 'user-bubble';
    const bubbleLabel = document.createElement('span');
    bubbleLabel.className = 'user-bubble-label';
    bubbleLabel.textContent = 'you';
    const bubbleText = document.createElement('div');
    bubbleText.className = 'user-bubble-text';
    bubbleText.textContent = promptText;
    userBubble.appendChild(bubbleLabel);
    userBubble.appendChild(bubbleText);
    el.agentStepsFeed.appendChild(userBubble);
    scrollToBottom();

    // build step card tracker
    let currentStepCard = null;
    let currentThinkingContent = null;
    let currentContentBody = null;

    try {
      const response = await fetch('/api/agent/run', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          prompt: promptText,
          history: state.history,
          model: state.config.model,
          reasoningEffort: state.config.reasoningEffort,
          maxSteps: state.loopMode ? 100 : 20,
          keepGoing: true, // smart default: verify before finishing (d2)
          loopMode: state.loopMode
        }),
        signal: state.abortController.signal
      });

      if (!response.ok) {
        throw new Error(`server returned ${response.status}`);
      }

      const reader = response.body.getReader();
      const decoder = new TextDecoder('utf-8');
      let buffer = '';

      while (true) {
        const { done, value } = await reader.read();
        if (done) break;

        buffer += decoder.decode(value, { stream: true });
        const lines = buffer.split('\n');
        buffer = lines.pop() || '';

        for (const line of lines) {
          const trimmed = line.trim();
          if (!trimmed || !trimmed.startsWith('data: ')) continue;
          const jsonStr = trimmed.slice(6);
          if (jsonStr === '[DONE]') continue;

          let parsedEvent = null;
          try {
            parsedEvent = JSON.parse(jsonStr);
          } catch (e) { continue; }
          try {
            handleAgentEvent(parsedEvent);
          } catch (handlerErr) {
            // never swallow handler crashes — render them where the human sees
            console.error('vanish: agent event handler crashed', handlerErr, parsedEvent);
            const crash = document.createElement('div');
            crash.className = 'continue-nudge retry-note';
            crash.textContent =
              `☠ ui handler crash on "${parsedEvent.type}": ${handlerErr.message}`;
            el.agentStepsFeed.appendChild(crash);
          }
        }
      }
    } catch (err) {
      if (err.name !== 'AbortError') {
        showToast(`agent error: ${err.message}`);
      }
    } finally {
      state.isAgentRunning = false;
      state.abortController = null;
      el.btnAgentStop.classList.add('hidden');
      el.btnAgentRun.classList.remove('hidden');
      el.dockAgentStatus.textContent = 'ready';
      loadFileTree();
      checkGitStatus();
    }

    function handleAgentEvent(ev) {
      switch (ev.type) {
        case 'step_start': {
          currentStepCard = document.createElement('div');
          currentStepCard.className = 'agent-step-card';
          currentStepCard.innerHTML = `
            <div class="step-card-header">
              <span>step ${ev.step} / ${ev.maxSteps}</span>
              <span class="step-status">thinking...</span>
            </div>
            <div class="thinking-box" id="step-thinking-${ev.step}">
              <div class="thinking-header">
                <span>thought process</span>
                <span>▾</span>
              </div>
              <div class="thinking-content"></div>
            </div>
            <div class="step-tool-container" id="step-tools-${ev.step}"></div>
            <div class="markdown-body step-content" id="step-content-${ev.step}"></div>
          `;

          currentThinkingContent = currentStepCard.querySelector('.thinking-content');
          currentContentBody = currentStepCard.querySelector('.step-content');

          const thinkingBox = currentStepCard.querySelector('.thinking-box');
          currentStepCard.querySelector('.thinking-header').addEventListener('click', () => {
            thinkingBox.classList.toggle('collapsed');
          });

          el.agentStepsFeed.appendChild(currentStepCard);
          scrollToBottom();
          break;
        }

        case 'reasoning_chunk': {
          if (currentThinkingContent) {
            currentThinkingContent.textContent += ev.text.toLowerCase();
            scrollToBottom();
          }
          break;
        }

        case 'content_chunk': {
          if (currentContentBody) {
            currentContentBody.innerHTML = renderMarkdownSafe(
              (currentContentBody.dataset.raw = (currentContentBody.dataset.raw || '') + ev.text)
            );
            scrollToBottom();
          }
          break;
        }

        case 'tool_exec_start': {
          if (currentStepCard) {
            const toolBox = currentStepCard.querySelector(`#step-tools-${ev.step}`);
            const card = document.createElement('div');
            card.className = 'tool-call-card';
            card.id = `tool-card-${ev.toolId}`;
            card.innerHTML = `
              <div class="tool-call-header">
                <span>⚡ executing: ${escapeHtml(ev.name).toLowerCase()}</span>
                <span class="tool-spinner">running...</span>
              </div>
              <div class="tool-call-body">
                <div class="tool-args">args: ${escapeHtml(JSON.stringify(ev.args || {})).toLowerCase()}</div>
                <div class="tool-result">waiting for output...</div>
              </div>
            `;
            toolBox.appendChild(card);
            scrollToBottom();
          }
          break;
        }

        case 'tool_exec_result': {
          const card = document.getElementById(`tool-card-${ev.toolId}`);
          if (card) {
            card.querySelector('.tool-spinner').textContent = `${ev.duration}ms`;
            const resultBox = card.querySelector('.tool-result');
            resultBox.textContent = escapeHtml(JSON.stringify(ev.result, null, 2)).toLowerCase();
          }
          break;
        }

        case 'step_end': {
          if (currentStepCard) {
            currentStepCard.querySelector('.step-status').textContent = ev.hasToolCalls ? 'tools completed' : 'finalized';
            const thinkingBox = currentStepCard.querySelector('.thinking-box');
            if (thinkingBox && currentThinkingContent && currentThinkingContent.textContent.trim()) {
              thinkingBox.classList.add('collapsed');
            }
          }
          break;
        }

        case 'continue_nudge': {
          if (currentStepCard) {
            currentStepCard.querySelector('.step-status').textContent = ev.loopMode
              ? '∞ loop — continuing'
              : 'keep going — verifying before finish';
            const nudge = document.createElement('div');
            nudge.className = 'continue-nudge';
            nudge.textContent = ev.loopMode
              ? `↻ ∞ loop: early finish refused at step ${ev.step} — agent continues until you press stop`
              : `↻ keep going: early finish refused, ${ev.remainingSteps || 1} verification step(s) left`;
            el.agentStepsFeed.appendChild(nudge);
            scrollToBottom();
          }
          break;
        }

        case 'step_retry': {
          if (currentStepCard) {
            currentStepCard.querySelector('.step-status').textContent =
              `transient error — retry ${ev.attempt}/${ev.maxAttempts}`;
            const note = document.createElement('div');
            note.className = 'continue-nudge retry-note';
            note.textContent = `↻ transient llm error (attempt ${ev.attempt}/${ev.maxAttempts}): ${ev.error || 'unknown'}`;
            el.agentStepsFeed.appendChild(note);
            scrollToBottom();
          }
          break;
        }

        case 'step_error': {
          state.isAgentRunning = false;
          if (currentStepCard) {
            const statusEl = currentStepCard.querySelector('.step-status');
            statusEl.textContent = 'step failed';
            statusEl.classList.add('fatal-status');
          }
          const box = document.createElement('div');
          box.className = 'death-report';
          box.innerHTML =
            `<div class="death-title">☠ step ${ev.step} failed</div>` +
            `<pre class="death-detail">${escapeHtml(ev.error || 'unknown error')}</pre>`;
          el.agentStepsFeed.appendChild(box);
          showToast(`agent failed at step ${ev.step} — cause shown above`);
          scrollToBottom();
          break;
        }

        case 'agent_died': {
          state.isAgentRunning = false;
          if (currentStepCard) {
            const statusEl = currentStepCard.querySelector('.step-status');
            statusEl.textContent = 'died';
            statusEl.classList.add('fatal-status');
          }
          const report = document.createElement('div');
          report.className = 'death-report fatal';
          report.innerHTML =
            `<div class="death-title">☠ agent died at step ${ev.step} / ${ev.maxSteps}</div>` +
            `<div class="death-reason">cause: ${escapeHtml(ev.reason || 'unknown')}</div>` +
            `<div class="death-meta">full post-mortem committed to memory/deaths.md (${ev.timestamp})</div>`;
          el.agentStepsFeed.appendChild(report);
          showToast('agent died — post-mortem logged to memory/deaths.md');
          scrollToBottom();
          break;
        }

        case 'agent_stopped': {
          state.isAgentRunning = false;
          if (currentStepCard) {
            currentStepCard.querySelector('.step-status').textContent = 'stopped by user';
          }
          showToast(`agent stopped: ${ev.reason || 'aborted'}`);
          break;
        }

        case 'time_wrapup': {
          const w = document.createElement('div');
          w.className = 'continue-nudge retry-note';
          w.textContent = '⏳ time budget low — agent told to commit staged work and wrap up';
          el.agentStepsFeed.appendChild(w);
          scrollToBottom();
          break;
        }

        case 'time_budget_exhausted': {
          state.isAgentRunning = false;
          const t = document.createElement('div');
          t.className = 'death-report';
          t.innerHTML =
            `<div class="death-title">⏳ run ended at the serverless time wall</div>` +
            `<div class="death-reason">stopped gracefully after ${ev.elapsedMs ? Math.round(ev.elapsedMs / 1000) : '?'}s — staged work preserved, nothing lost.</div>` +
            `<div class="death-meta">this is the hobby-tier 60s function cap, not a crash. upgrade the vercel plan or split work into smaller prompts to run longer.</div>`;
          el.agentStepsFeed.appendChild(t);
          showToast(`run wrapped at ~${Math.round((ev.elapsedMs || 0) / 1000)}s — work committed, not lost`);
          scrollToBottom();
          break;
        }

        case 'deployment_state': {
          // run-opening health check: only worth surfacing when it is red
          const r = ev.report || {};
          if (r.found && !r.succeeded && (r.state === 'ERROR' || r.state === 'CANCELED')) {
            const d = document.createElement('div');
            d.className = 'death-report fatal';
            d.innerHTML =
              `<div class="death-title">🔴 production is red — build ${escapeHtml(r.sha || '')} failed</div>` +
              `<div class="death-reason">${escapeHtml(r.error_message || r.state)}</div>` +
              (r.build_log_tail
                ? `<pre class="death-detail">${escapeHtml(r.build_log_tail)}</pre>`
                : '') +
              `<div class="death-meta">the agent has been told to fix this before anything else.</div>`;
            el.agentStepsFeed.appendChild(d);
            showToast('live deployment is failing — agent is on it');
            scrollToBottom();
          }
          break;
        }

        case 'deployment_watch': {
          const w = document.createElement('div');
          w.className = 'continue-nudge retry-note';
          w.textContent = `⏱ watching the build for ${ev.sha || 'the new commit'}…`;
          el.agentStepsFeed.appendChild(w);
          scrollToBottom();
          break;
        }

        case 'deployment_result': {
          const r = ev.report || {};
          const d = document.createElement('div');
          if (r.succeeded) {
            d.className = 'continue-nudge retry-note';
            d.textContent = `✅ ${ev.sha || 'commit'} deployed${r.url ? ` — ${r.url}` : ''}`;
          } else if (r.timed_out) {
            d.className = 'continue-nudge retry-note';
            d.textContent = `⏱ build for ${ev.sha || 'commit'} still running — result will show on the next check`;
          } else {
            d.className = 'death-report fatal';
            d.innerHTML =
              `<div class="death-title">🔴 build failed for ${escapeHtml(ev.sha || '')}</div>` +
              `<div class="death-reason">${escapeHtml(r.error_message || r.state || 'unknown')}</div>` +
              (r.build_log_tail
                ? `<pre class="death-detail">${escapeHtml(r.build_log_tail)}</pre>`
                : '');
            showToast('deploy failed — error handed back to the agent');
          }
          el.agentStepsFeed.appendChild(d);
          scrollToBottom();
          break;
        }

        case 'deployment_check_skipped':
        case 'deployment_check_failed': {
          const n = document.createElement('div');
          n.className = 'continue-nudge retry-note';
          n.textContent = `⚠ deploy check skipped: ${ev.reason || ev.error || 'unavailable'}`;
          el.agentStepsFeed.appendChild(n);
          scrollToBottom();
          break;
        }

        case 'agent_context': {
          if (Array.isArray(ev.messages)) {
            state.history = ev.messages;
            saveHistory();
          }
          break;
        }

        case 'agent_complete': {
          showToast(`agent finished in ${ev.totalSteps} steps (${ev.duration}) — history saved, keep chatting`);
          break;
        }
      }
    }
  }

  function renderMarkdownSafe(text) {
    if (!window.marked) return escapeHtml(text).replace(/\n/g, '<br>');
    try { return window.marked.parse(text); } catch (e) { return escapeHtml(text); }
  }

  function scrollToBottom() {
    el.agentStreamContainer.scrollTop = el.agentStreamContainer.scrollHeight;
  }

  function escapeHtml(str) {
    if (!str) return '';
    return String(str).replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
  }

  // event listeners
  el.tabAgent.addEventListener('click', () => switchTab('agent'));
  el.tabEditor.addEventListener('click', () => switchTab('editor'));
  el.tabDiff.addEventListener('click', () => switchTab('diff'));

  el.btnRefreshWorkspace.addEventListener('click', () => {
    loadFileTree();
    checkGitStatus();
    showToast('workspace refreshed');
  });

  el.btnNewFile.addEventListener('click', () => {
    const filename = prompt('enter relative file path (e.g. lib/custom-helper.js):');
    if (filename) {
      openFileInEditor(filename.trim());
    }
  });

  el.paramEffort.addEventListener('change', () => {
    state.config.reasoningEffort = el.paramEffort.value;
    el.valEffort.textContent = state.config.reasoningEffort;
  });

  // ∞ loop toggle: the only user-facing autonomy control. loop mode means
  // the agent never self-terminates — only the stop button ends it.
  if (el.chkLoopMode) {
    el.chkLoopMode.checked = state.loopMode;
    el.chkLoopMode.addEventListener('change', () => {
      state.loopMode = el.chkLoopMode.checked;
      try {
        localStorage.setItem('vanish_loop_mode', String(state.loopMode));
      } catch (e) {}
      showToast(
        state.loopMode
          ? '∞ loop on — agent runs until you press stop'
          : '∞ loop off — agent finishes when its work is verified done'
      );
    });
  }

  el.btnToggleSidebar.addEventListener('click', () => {
    el.sidebar.classList.toggle('open');
  });

  // new chat: spawn a fresh thread (old threads stay listed in the sidebar)
  if (el.threadList) {
    document.getElementById('btn-new-chat')?.addEventListener('click', () => {
      if (state.isAgentRunning) {
        showToast('stop the agent before starting a new conversation');
        return;
      }
      createThread();
    });
  }

  // bottom dock run & stop
  el.btnAgentRun.addEventListener('click', () => {
    const promptText = el.agentPromptInput.value.trim();
    if (promptText) {
      el.agentPromptInput.value = '';
      runAutonomousAgent(promptText);
    }
  });

  el.btnAgentStop.addEventListener('click', () => {
    if (state.abortController) {
      state.abortController.abort();
      showToast('stopping agent...');
    }
  });

  el.agentPromptInput.addEventListener('keydown', (e) => {
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault();
      const promptText = el.agentPromptInput.value.trim();
      if (promptText) {
        el.agentPromptInput.value = '';
        runAutonomousAgent(promptText);
      }
    }
  });

  // preset click handlers
  document.querySelectorAll('.preset-item, .starter-chip').forEach(btn => {
    btn.addEventListener('click', () => {
      const promptText = btn.getAttribute('data-prompt');
      if (promptText) {
        runAutonomousAgent(promptText);
      }
    });
  });

  // initial load
  initThreads();
  loadFileTree();
  checkGitStatus();
});
