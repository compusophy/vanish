import { exec } from 'child_process';
import util from 'util';

const execPromise = util.promisify(exec);
const WORKSPACE_ROOT = process.cwd();
const API = 'https://api.vercel.com';

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

function isCloud() {
  return Boolean(process.env.VERCEL);
}

// ---- credentials ------------------------------------------------------
//
// the harness edits its own source and pushes it to github, which vercel
// then builds. without an api token that build is a black hole: the agent
// commits, the deploy fails, and the loop never learns. VERCEL_TOKEN turns
// the deployment into an observable part of the feedback loop.
//
//   VERCEL_TOKEN      - https://vercel.com/account/tokens (scoped to the team)
//   VERCEL_PROJECT_ID - prj_... the project that builds this repo
//   VERCEL_TEAM_ID    - team_... required when the project lives in a team

export function projectRef() {
  return process.env.VERCEL_PROJECT_ID || process.env.VERCEL_PROJECT || null;
}

export function vercelConfigured() {
  return Boolean(process.env.VERCEL_TOKEN && projectRef());
}

export function vercelConfigStatus() {
  const missing = [];
  if (!process.env.VERCEL_TOKEN) missing.push('VERCEL_TOKEN');
  if (!projectRef()) missing.push('VERCEL_PROJECT_ID');
  return {
    configured: missing.length === 0,
    missing,
    team: process.env.VERCEL_TEAM_ID || null,
    project: projectRef(),
    hint: missing.length
      ? `set ${missing.join(' and ')} in the vercel project env vars so the agent can read its own build results`
      : null
  };
}

function withQuery(pathname, params = {}) {
  const url = new URL(API + pathname);
  const team = process.env.VERCEL_TEAM_ID;
  if (team) url.searchParams.set('teamId', team);
  for (const [k, v] of Object.entries(params)) {
    if (v !== undefined && v !== null && v !== '') url.searchParams.set(k, String(v));
  }
  return url.toString();
}

async function api(pathname, params = {}, { raw = false } = {}) {
  const token = process.env.VERCEL_TOKEN;
  if (!token) {
    const err = new Error('VERCEL_TOKEN is not set on this deployment');
    err.code = 'NO_TOKEN';
    throw err;
  }

  const res = await fetch(withQuery(pathname, params), {
    headers: { Authorization: `Bearer ${token}` }
  });

  const text = await res.text();
  if (!res.ok) {
    let detail = text.slice(0, 400);
    try {
      detail = JSON.parse(text)?.error?.message || detail;
    } catch (e) {
      /* keep the raw body */
    }
    const err = new Error(`vercel api ${res.status}: ${detail}`);
    err.status = res.status;
    throw err;
  }

  if (raw) return text;
  try {
    return JSON.parse(text);
  } catch (e) {
    return text;
  }
}

// ---- deployments ------------------------------------------------------

function normalize(d) {
  if (!d) return null;
  const meta = d.meta || {};
  return {
    id: d.uid || d.id,
    state: d.readyState || d.state || 'UNKNOWN',
    target: d.target || null,
    url: d.url ? `https://${d.url}` : null,
    created: d.createdAt || d.created || null,
    sha: meta.githubCommitSha || meta.gitCommitSha || null,
    branch: meta.githubCommitRef || meta.gitCommitRef || null,
    message: (meta.githubCommitMessage || meta.gitCommitMessage || '').split('\n')[0] || null
  };
}

export async function listDeployments({ limit = 10, target } = {}) {
  const data = await api('/v6/deployments', {
    projectId: projectRef(),
    limit,
    target
  });
  return (data.deployments || []).map(normalize);
}

export async function getDeployment(idOrUrl) {
  const d = await api(`/v13/deployments/${encodeURIComponent(idOrUrl)}`);
  return {
    ...normalize(d),
    errorCode: d.errorCode || null,
    errorMessage: d.errorMessage || null,
    errorStep: d.errorStep || null
  };
}

// find the deployment vercel produced for a specific commit. github can take
// a few seconds to notify vercel, so callers retry via waitForDeployment.
export async function findDeploymentForSha(sha, { limit = 20 } = {}) {
  if (!sha) return null;
  const short = String(sha).slice(0, 7);
  const deployments = await listDeployments({ limit });
  return deployments.find((d) => d.sha && d.sha.startsWith(short)) || null;
}

// build errors arrive wrapped in ansi color escapes that waste tokens and
// make the message unreadable to a model. the escape byte is built from a
// char code so this line survives being rewritten by the agent itself.
const ESC = String.fromCharCode(27);
const ANSI_SEQ = new RegExp(ESC + '[[]([0-9;]*)([A-Za-z])', 'g');
const BARE_SGR = new RegExp('[[]([0-9;]+)m', 'g');

function stripAnsi(s) {
  return String(s).replace(ANSI_SEQ, '').replace(BARE_SGR, '');
}

/**
 * build logs for a deployment, as plain text lines oldest-first. the raw
 * event stream is json noise wrapped around a handful of meaningful lines.
 */
export async function getBuildLogs(id, { limit = 200, errorsOnly = false } = {}) {
  const body = await api(
    `/v3/deployments/${encodeURIComponent(id)}/events`,
    { builds: 1, limit, direction: 'backward' },
    { raw: true }
  );

  let events = [];
  try {
    const parsed = JSON.parse(body);
    events = Array.isArray(parsed) ? parsed : parsed.events || [];
  } catch (e) {
    // some responses come back as newline-delimited json
    events = body
      .split('\n')
      .filter(Boolean)
      .map((line) => {
        try {
          return JSON.parse(line);
        } catch (err) {
          return null;
        }
      })
      .filter(Boolean);
  }

  const lines = events
    .map((e) => {
      const text = typeof e.payload === 'string' ? e.payload : e.payload?.text || '';
      return { type: e.type, text: stripAnsi(text).replace(/\s+$/, '') };
    })
    .filter((e) => e.text.length)
    .filter((e) => !errorsOnly || e.type === 'stderr' || /error/i.test(e.text))
    .map((e) => e.text);

  // the api returns newest first when direction=backward
  return lines.reverse();
}

/**
 * the whole point of the vercel credentials: one call that answers
 * "did my last commit actually deploy, and if not, why".
 */
export async function deploymentReport(idOrSha, { logLines = 40 } = {}) {
  let dep = null;

  if (idOrSha && /^[0-9a-f]{7,40}$/i.test(idOrSha)) {
    const match = await findDeploymentForSha(idOrSha);
    if (!match) return { found: false, sha: idOrSha, note: 'no deployment for that commit yet' };
    dep = await getDeployment(match.id);
  } else if (idOrSha) {
    dep = await getDeployment(idOrSha);
  } else {
    const [latest] = await listDeployments({ limit: 1 });
    if (!latest) return { found: false, note: 'this project has no deployments' };
    dep = await getDeployment(latest.id);
  }

  const failed = dep.state === 'ERROR' || dep.state === 'CANCELED';
  const report = {
    found: true,
    id: dep.id,
    state: dep.state,
    succeeded: dep.state === 'READY',
    url: dep.url,
    sha: dep.sha ? dep.sha.slice(0, 7) : null,
    branch: dep.branch,
    commit_message: dep.message,
    error_message: dep.errorMessage || null,
    error_step: dep.errorStep || null
  };

  if (failed) {
    try {
      const lines = await getBuildLogs(dep.id, { limit: 200 });
      report.build_log_tail = lines.slice(-logLines).join('\n');
    } catch (err) {
      report.build_log_error = err.message;
    }
  }

  return report;
}

/**
 * block until a commit's deployment settles. bounded on purpose: the agent
 * runs inside a serverless function with a hard time wall, so it must never
 * sit on a poll loop indefinitely.
 */
export async function waitForDeployment(sha, { timeoutMs = 60000, pollMs = 3000 } = {}) {
  const started = Date.now();
  let dep = null;

  while (Date.now() - started < timeoutMs) {
    if (!dep) {
      dep = await findDeploymentForSha(sha);
      if (!dep) {
        await sleep(pollMs);
        continue;
      }
    }

    const current = await getDeployment(dep.id);
    if (['READY', 'ERROR', 'CANCELED'].includes(current.state)) {
      return await deploymentReport(current.id);
    }
    await sleep(pollMs);
  }

  return {
    found: Boolean(dep),
    timed_out: true,
    id: dep?.id || null,
    state: dep?.state || 'PENDING',
    note: `deployment did not settle within ${Math.round(timeoutMs / 1000)}s. call check_deployment again to see the result.`
  };
}

// ---- status / deploy (original public surface, now credential-aware) ---

export async function checkVercelStatus() {
  const base = { credentials: vercelConfigStatus() };

  // running on vercel there is no cli to shell out to, but the platform
  // injects everything worth reporting as environment variables.
  if (isCloud()) {
    Object.assign(base, {
      logged_in: true,
      running_on_vercel: true,
      env: process.env.VERCEL_ENV || 'unknown',
      url: process.env.VERCEL_URL ? `https://${process.env.VERCEL_URL}` : null,
      production_url: process.env.VERCEL_PROJECT_PRODUCTION_URL
        ? `https://${process.env.VERCEL_PROJECT_PRODUCTION_URL}`
        : null,
      region: process.env.VERCEL_REGION || null,
      commit: process.env.VERCEL_GIT_COMMIT_SHA
        ? process.env.VERCEL_GIT_COMMIT_SHA.slice(0, 7)
        : null,
      commit_message: process.env.VERCEL_GIT_COMMIT_MESSAGE || null,
      branch: process.env.VERCEL_GIT_COMMIT_REF || null,
      continuous_deployment: Boolean(process.env.VERCEL_GIT_COMMIT_SHA)
    });
  } else {
    try {
      const { stdout } = await execPromise('vercel whoami', { cwd: WORKSPACE_ROOT });
      Object.assign(base, {
        logged_in: true,
        running_on_vercel: false,
        user: stdout.trim().split('\n').pop()
      });
    } catch (err) {
      Object.assign(base, { logged_in: false, running_on_vercel: false, error: err.message });
    }
  }

  // with a token we can report what actually matters: the state of the
  // latest build, not just which machine we happen to be running on.
  if (vercelConfigured()) {
    try {
      const [latest] = await listDeployments({ limit: 1 });
      base.latest_deployment = latest;
    } catch (err) {
      base.latest_deployment_error = err.message;
    }
  }

  return base;
}

export async function deployToVercel(prod = false) {
  if (isCloud()) {
    const hook = process.env.VERCEL_DEPLOY_HOOK_URL;
    if (hook) {
      try {
        const res = await fetch(hook, { method: 'POST' });
        return { success: res.ok, triggered: 'deploy hook', status: res.status };
      } catch (err) {
        return { success: false, error: err.message };
      }
    }
    return {
      success: true,
      note: 'this deployment is connected to github with continuous deployment, so every commit to the connected branch deploys automatically. set VERCEL_DEPLOY_HOOK_URL to force a rebuild without a commit.'
    };
  }

  try {
    const flag = prod ? '--prod' : '';
    const { stdout, stderr } = await execPromise(`vercel ${flag} --yes`, {
      cwd: WORKSPACE_ROOT,
      timeout: 180000
    });

    const output = (stdout + '\n' + stderr).trim();
    const urlMatch = output.match(/https:\/\/[a-zA-Z0-9-]+\.vercel\.app/);

    return { success: true, url: urlMatch ? urlMatch[0] : null, output };
  } catch (err) {
    return { success: false, error: err.message, output: err.stdout || err.stderr || '' };
  }
}
