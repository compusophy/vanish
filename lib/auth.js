import crypto from 'crypto';
import express from 'express';
import {
  readSession,
  setSessionCookie,
  clearSessionCookie,
  setStateCookie,
  readStateCookie,
  clearStateCookie
} from './session.js';
import { getViewer } from './github-service.js';

const GITHUB_AUTHORIZE = 'https://github.com/login/oauth/authorize';
const GITHUB_TOKEN = 'https://github.com/login/oauth/access_token';

// `repo` covers both public and private repositories. read:user is only used
// to resolve the signed-in login for the allowlist check.
const DEFAULT_SCOPE = 'repo read:user';

export function isCloud() {
  return Boolean(process.env.VERCEL);
}

export function oauthConfigured() {
  return Boolean(
    process.env.GITHUB_CLIENT_ID &&
    process.env.GITHUB_CLIENT_SECRET &&
    process.env.SESSION_SECRET
  );
}

export function repoFullName() {
  return process.env.GITHUB_REPO || 'compusophy/vanish';
}

export function repoBranch() {
  return process.env.GITHUB_BRANCH || 'main';
}

// who is allowed to drive this harness. defaults to the owner of the
// connected repository so a fresh deployment is never wide open.
function allowedLogins() {
  const configured = (process.env.ALLOWED_GITHUB_LOGINS || '')
    .split(',')
    .map((s) => s.trim().toLowerCase())
    .filter(Boolean);

  if (configured.length) return configured;
  return [repoFullName().split('/')[0].toLowerCase()];
}

function baseUrl(req) {
  if (process.env.PUBLIC_BASE_URL) {
    return process.env.PUBLIC_BASE_URL.replace(/\/+$/, '');
  }
  const proto = req.headers['x-forwarded-proto'] || req.protocol || 'http';
  const host = req.headers['x-forwarded-host'] || req.headers.host;
  return `${proto}://${host}`;
}

export function callbackUrl(req) {
  return `${baseUrl(req)}/api/auth/github/callback`;
}

// resolves the identity + github token for a request.
export function getAuth(req) {
  const session = readSession(req);
  if (session?.token) {
    return {
      authenticated: true,
      login: session.login,
      name: session.name,
      avatar: session.avatar,
      token: session.token,
      source: 'oauth'
    };
  }

  // headless fallback for cron jobs / local runs where no browser is involved
  if (process.env.GITHUB_TOKEN) {
    return {
      authenticated: true,
      login: process.env.GITHUB_TOKEN_LOGIN || 'service-token',
      token: process.env.GITHUB_TOKEN,
      source: 'env'
    };
  }

  return { authenticated: false };
}

// gate for every endpoint that spends money, mutates files, or writes to git.
//
// when oauth is not configured at all we allow local development through, but
// a cloud deployment is always locked: an open /api/agent/run on a public url
// would let anyone drain the openrouter key.
export function requireAuth(req, res, next) {
  const auth = getAuth(req);
  if (auth.authenticated) {
    req.auth = auth;
    return next();
  }

  if (!isCloud() && !oauthConfigured()) {
    req.auth = { authenticated: true, login: 'local-dev', token: null, source: 'local' };
    return next();
  }

  return res.status(401).json({
    error: 'not authenticated',
    detail: oauthConfigured()
      ? 'sign in with github to use this harness'
      : 'github oauth is not configured on this deployment (missing GITHUB_CLIENT_ID / GITHUB_CLIENT_SECRET / SESSION_SECRET)',
    login_url: '/api/auth/github'
  });
}

export function createAuthRouter() {
  const router = express.Router();

  // 1. kick off the oauth dance
  router.get('/github', (req, res) => {
    if (!oauthConfigured()) {
      return res.status(500).json({
        error: 'github oauth is not configured',
        missing: [
          !process.env.GITHUB_CLIENT_ID && 'GITHUB_CLIENT_ID',
          !process.env.GITHUB_CLIENT_SECRET && 'GITHUB_CLIENT_SECRET',
          !process.env.SESSION_SECRET && 'SESSION_SECRET'
        ].filter(Boolean)
      });
    }

    const state = crypto.randomBytes(16).toString('hex');
    setStateCookie(req, res, state);

    const params = new URLSearchParams({
      client_id: process.env.GITHUB_CLIENT_ID,
      redirect_uri: callbackUrl(req),
      scope: process.env.GITHUB_OAUTH_SCOPE || DEFAULT_SCOPE,
      state,
      allow_signup: 'false'
    });

    res.redirect(`${GITHUB_AUTHORIZE}?${params}`);
  });

  // 2. exchange the code, verify the user, seal the session
  router.get('/github/callback', async (req, res) => {
    const { code, state, error: oauthError } = req.query;

    if (oauthError) {
      return res.redirect(`/?auth_error=${encodeURIComponent(String(oauthError))}`);
    }
    if (!code) {
      return res.redirect('/?auth_error=missing_code');
    }

    // csrf: the state we handed to github must match the cookie we set
    const expected = readStateCookie(req);
    clearStateCookie(req, res);
    if (!expected || expected !== state) {
      return res.redirect('/?auth_error=state_mismatch');
    }

    try {
      const tokenRes = await fetch(GITHUB_TOKEN, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json', Accept: 'application/json' },
        body: JSON.stringify({
          client_id: process.env.GITHUB_CLIENT_ID,
          client_secret: process.env.GITHUB_CLIENT_SECRET,
          code,
          redirect_uri: callbackUrl(req)
        })
      });

      const tokenData = await tokenRes.json();
      if (!tokenData.access_token) {
        return res.redirect(
          `/?auth_error=${encodeURIComponent(tokenData.error_description || tokenData.error || 'token_exchange_failed')}`
        );
      }

      const user = await getViewer(tokenData.access_token);
      const login = String(user.login || '').toLowerCase();

      if (!allowedLogins().includes(login)) {
        return res.redirect(`/?auth_error=${encodeURIComponent(`${user.login} is not authorized for this harness`)}`);
      }

      setSessionCookie(req, res, {
        login: user.login,
        name: user.name,
        avatar: user.avatar_url,
        token: tokenData.access_token,
        scope: tokenData.scope
      });

      res.redirect('/?signed_in=1');
    } catch (err) {
      res.redirect(`/?auth_error=${encodeURIComponent(err.message || 'oauth failed')}`);
    }
  });

  // 3. session introspection for the ui
  router.get('/session', (req, res) => {
    const auth = getAuth(req);
    res.json({
      authenticated: auth.authenticated,
      login: auth.login || null,
      name: auth.name || null,
      avatar: auth.avatar || null,
      source: auth.source || null,
      oauth_configured: oauthConfigured(),
      // local dev with no oauth configured is deliberately unlocked, so the
      // ui should not put a sign-in wall in front of it
      open_access: !isCloud() && !oauthConfigured(),
      cloud: isCloud(),
      repo: repoFullName(),
      branch: repoBranch(),
      can_write: Boolean(auth.token),
      login_url: '/api/auth/github'
    });
  });

  // 4. sign out
  router.post('/logout', (req, res) => {
    clearSessionCookie(req, res);
    res.json({ success: true });
  });

  return router;
}
