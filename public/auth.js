// vanish github oauth gate
// self-contained: injects its own styles and dom so the main ide markup
// stays untouched. all copy stays lowercase per the harness ui rules.

(() => {
  const STYLES = `
    #vanish-auth-overlay {
      position: fixed;
      inset: 0;
      z-index: 9999;
      display: none;
      align-items: center;
      justify-content: center;
      background: rgba(8, 9, 12, 0.82);
      backdrop-filter: blur(18px);
      -webkit-backdrop-filter: blur(18px);
      font-family: 'jetbrains mono', ui-monospace, monospace;
    }
    #vanish-auth-overlay.visible { display: flex; }

    .vanish-auth-card {
      width: min(420px, calc(100vw - 48px));
      padding: 36px 32px 30px;
      border-radius: 16px;
      border: 1px solid rgba(255, 255, 255, 0.09);
      background: linear-gradient(160deg, rgba(24, 26, 33, 0.96), rgba(14, 15, 19, 0.96));
      box-shadow: 0 30px 90px rgba(0, 0, 0, 0.6);
      text-align: center;
      color: #e6e8ee;
    }
    .vanish-auth-glyph {
      width: 46px; height: 46px;
      margin: 0 auto 18px;
      display: grid; place-items: center;
      border-radius: 12px;
      background: linear-gradient(140deg, #6d7cff, #a855f7);
      font-weight: 700; font-size: 22px; color: #fff;
    }
    .vanish-auth-title {
      margin: 0 0 6px; font-size: 19px; font-weight: 600; letter-spacing: -0.01em;
    }
    .vanish-auth-sub {
      margin: 0 0 22px; font-size: 12px; line-height: 1.6; color: #8b90a0;
    }
    .vanish-auth-repo {
      display: inline-block; margin-bottom: 22px; padding: 5px 11px;
      border-radius: 999px; border: 1px solid rgba(255, 255, 255, 0.09);
      background: rgba(255, 255, 255, 0.03);
      font-size: 11px; color: #9aa0b0;
    }
    .vanish-auth-btn {
      width: 100%; padding: 12px 18px;
      display: inline-flex; align-items: center; justify-content: center; gap: 9px;
      border: 0; border-radius: 10px; cursor: pointer;
      background: #e9ebf2; color: #14151a;
      font-family: inherit; font-size: 13px; font-weight: 600;
      transition: transform 0.12s ease, opacity 0.12s ease;
    }
    .vanish-auth-btn:hover { transform: translateY(-1px); }
    .vanish-auth-btn:disabled { opacity: 0.4; cursor: not-allowed; transform: none; }
    .vanish-auth-note {
      margin-top: 18px; font-size: 10.5px; line-height: 1.7; color: #6d7283;
    }
    .vanish-auth-error {
      margin-bottom: 18px; padding: 10px 12px;
      border-radius: 9px; border: 1px solid rgba(248, 113, 113, 0.28);
      background: rgba(248, 113, 113, 0.09);
      font-size: 11px; line-height: 1.6; color: #fca5a5;
      word-break: break-word; text-align: left;
    }
    .vanish-auth-error:empty { display: none; }
    .vanish-auth-missing {
      margin-top: 10px; font-size: 10.5px; color: #8b90a0; text-align: left;
    }
    .vanish-auth-missing code {
      display: block; margin-top: 4px; color: #cbd0dd;
    }

    #vanish-auth-chip {
      position: fixed; top: 12px; right: 14px; z-index: 9998;
      display: none; align-items: center; gap: 8px;
      padding: 5px 9px 5px 5px;
      border-radius: 999px;
      border: 1px solid rgba(255, 255, 255, 0.08);
      background: rgba(20, 21, 27, 0.85);
      backdrop-filter: blur(10px);
      font-family: 'jetbrains mono', ui-monospace, monospace;
      font-size: 11px; color: #b8bdcc;
    }
    #vanish-auth-chip.visible { display: flex; }
    #vanish-auth-chip img { width: 20px; height: 20px; border-radius: 50%; }
    #vanish-auth-chip .vanish-chip-mode {
      padding: 2px 7px; border-radius: 999px;
      background: rgba(109, 124, 255, 0.15); color: #a5b0ff; font-size: 10px;
    }
    #vanish-auth-chip button {
      border: 0; background: none; cursor: pointer;
      color: #6d7283; font-family: inherit; font-size: 10.5px; padding: 2px 2px;
    }
    #vanish-auth-chip button:hover { color: #e6e8ee; }
  `;

  const GITHUB_MARK = `<svg viewBox="0 0 16 16" width="15" height="15" fill="currentColor" aria-hidden="true"><path d="M8 0C3.58 0 0 3.58 0 8c0 3.54 2.29 6.53 5.47 7.59.4.07.55-.17.55-.38 0-.19-.01-.82-.01-1.49-2.01.37-2.53-.49-2.69-.94-.09-.23-.48-.94-.82-1.13-.28-.15-.68-.52-.01-.53.63-.01 1.08.58 1.23.82.72 1.21 1.87.87 2.33.66.07-.52.28-.87.51-1.07-1.78-.2-3.64-.89-3.64-3.95 0-.87.31-1.59.82-2.15-.08-.2-.36-1.02.08-2.12 0 0 .67-.21 2.2.82.64-.18 1.32-.27 2-.27.68 0 1.36.09 2 .27 1.53-1.04 2.2-.82 2.2-.82.44 1.1.16 1.92.08 2.12.51.56.82 1.27.82 2.15 0 3.07-1.87 3.75-3.65 3.95.29.25.54.73.54 1.48 0 1.07-.01 1.93-.01 2.2 0 .21.15.46.55.38A8.01 8.01 0 0 0 16 8c0-4.42-3.58-8-8-8Z"/></svg>`;

  const style = document.createElement('style');
  style.textContent = STYLES;
  document.head.appendChild(style);

  const overlay = document.createElement('div');
  overlay.id = 'vanish-auth-overlay';
  overlay.innerHTML = `
    <div class="vanish-auth-card">
      <div class="vanish-auth-glyph">v</div>
      <h1 class="vanish-auth-title">vanish</h1>
      <p class="vanish-auth-sub">autonomous self-editing coding harness.<br />sign in to give the agent write access to its own source.</p>
      <div class="vanish-auth-error" id="vanish-auth-error"></div>
      <div class="vanish-auth-repo" id="vanish-auth-repo"></div>
      <button class="vanish-auth-btn" id="vanish-auth-signin">${GITHUB_MARK} sign in with github</button>
      <p class="vanish-auth-note" id="vanish-auth-note">
        the agent commits through your github account. only authorized logins can run it.
      </p>
    </div>
  `;

  const chip = document.createElement('div');
  chip.id = 'vanish-auth-chip';

  document.addEventListener('DOMContentLoaded', () => {
    document.body.appendChild(overlay);
    document.body.appendChild(chip);
    init();
  });

  const $ = (id) => document.getElementById(id);

  // distinguishes "never signed in" from "session lapsed mid-use" so a first
  // visit is not greeted with an expiry warning
  let wasAuthenticated = false;

  function showOverlay(message) {
    const errorEl = $('vanish-auth-error');
    if (errorEl && message) errorEl.textContent = message.toLowerCase();
    overlay.classList.add('visible');
    chip.classList.remove('visible');
  }

  function renderChip(session) {
    const avatar = session.avatar
      ? `<img src="${session.avatar}" alt="" />`
      : '';
    const mode = session.cloud ? 'cloud' : 'local';
    chip.innerHTML = `
      ${avatar}
      <span>${(session.login || 'local-dev').toLowerCase()}</span>
      <span class="vanish-chip-mode">${mode}</span>
      ${session.authenticated ? '<button id="vanish-auth-signout">sign out</button>' : ''}
    `;
    chip.classList.add('visible');

    const out = $('vanish-auth-signout');
    if (out) {
      out.addEventListener('click', async () => {
        await fetch('/api/auth/logout', { method: 'POST' });
        window.location.reload();
      });
    }
  }

  async function init() {
    // surface anything github handed back on the redirect
    const params = new URLSearchParams(window.location.search);
    const authError = params.get('auth_error');
    if (authError || params.get('signed_in')) {
      params.delete('auth_error');
      params.delete('signed_in');
      const qs = params.toString();
      window.history.replaceState({}, '', window.location.pathname + (qs ? `?${qs}` : ''));
    }

    let session;
    try {
      const res = await fetch('/api/auth/session');
      session = await res.json();
    } catch {
      showOverlay('could not reach the harness api.');
      return;
    }

    $('vanish-auth-repo').textContent = `${session.repo} · ${session.branch}`;

    if (session.authenticated || session.open_access) {
      wasAuthenticated = true;
      overlay.classList.remove('visible');
      renderChip(session);
      if (authError) console.warn('vanish auth:', authError);
      return;
    }

    if (!session.oauth_configured) {
      const btn = $('vanish-auth-signin');
      btn.disabled = true;
      btn.innerHTML = 'github oauth not configured';
      $('vanish-auth-note').innerHTML = `
        <div class="vanish-auth-missing">
          set these environment variables on the deployment, then redeploy:
          <code>GITHUB_CLIENT_ID</code>
          <code>GITHUB_CLIENT_SECRET</code>
          <code>SESSION_SECRET</code>
        </div>`;
    } else {
      $('vanish-auth-signin').addEventListener('click', () => {
        window.location.href = session.login_url || '/api/auth/github';
      });
    }

    showOverlay(authError || '');
  }

  // any endpoint answering 401 means the session lapsed. put the wall back up
  // instead of letting the ide fail silently.
  const originalFetch = window.fetch;
  window.fetch = async (...args) => {
    const res = await originalFetch(...args);
    const url = typeof args[0] === 'string' ? args[0] : args[0]?.url || '';
    if (res.status === 401 && url.startsWith('/api/') && !url.includes('/api/auth/')) {
      showOverlay(wasAuthenticated ? 'your session expired. sign in again.' : '');
    }
    return res;
  };
})();
