import express from 'express';
import cors from 'cors';
import dotenv from 'dotenv';
import path from 'path';
import fs from 'fs';
import { fileURLToPath } from 'url';

import { runAgentLoop } from './agent.js';
import { getGitStatus, getGitDiff, gitCommit, gitPush } from './git-service.js';
import { checkVercelStatus, deployToVercel } from './vercel-service.js';
import { createToolContext, detectMode, getToolDefinitions } from './tools.js';
import { runLLMStep } from './llm-step.js';
import * as gh from './github-service.js';
import {
  createAuthRouter,
  requireAuth,
  getAuth,
  oauthConfigured,
  isCloud,
  repoFullName,
  repoBranch
} from './auth.js';

dotenv.config();

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);
const ROOT = path.resolve(__dirname, '..');
const PUBLIC_DIR = path.join(ROOT, 'public');
const WORKSPACE_ROOT = process.cwd();

const app = express();

app.use(cors({ origin: true, credentials: true }));
app.use(express.json({ limit: '50mb' }));

// never let the browser serve a stale harness shell. this app is its own
// deploy pipeline — a cached app.js can reference DOM that no longer exists
// and silently brick the whole ui (this exact bug shipped twice).
app.use((req, res, next) => {
  res.setHeader('Cache-Control', 'no-store, max-age=0');
  next();
});

app.use(express.static(PUBLIC_DIR));

// builds the per-request tool context the agent and the ide endpoints share
function toolCtx(req) {
  return createToolContext({
    mode: detectMode(),
    token: req.auth?.token,
    repo: repoFullName(),
    branch: repoBranch()
  });
}

// turns flat github tree paths into the nested shape the ide expects
function nestPaths(entries) {
  const root = [];
  const dirs = new Map();

  const sorted = [...entries].sort((a, b) => a.path.localeCompare(b.path));
  for (const entry of sorted) {
    const parts = entry.path.split('/');
    const name = parts.pop();
    let siblings = root;

    let prefix = '';
    for (const part of parts) {
      prefix = prefix ? `${prefix}/${part}` : part;
      let dir = dirs.get(prefix);
      if (!dir) {
        dir = { name: part.toLowerCase(), path: prefix.toLowerCase(), type: 'directory', children: [] };
        dirs.set(prefix, dir);
        siblings.push(dir);
      }
      siblings = dir.children;
    }

    if (entry.type === 'directory') {
      if (!dirs.has(entry.path)) {
        const dir = { name: name.toLowerCase(), path: entry.path.toLowerCase(), type: 'directory', children: [] };
        dirs.set(entry.path, dir);
        siblings.push(dir);
      }
    } else {
      siblings.push({
        name: name.toLowerCase(),
        path: entry.path.toLowerCase(),
        type: 'file',
        size: entry.size
      });
    }
  }
  return root;
}

// ---- auth ------------------------------------------------------------

app.use('/api/auth', createAuthRouter());

// 1. health & platform status endpoint
app.get('/api/status', async (req, res) => {
  const auth = getAuth(req);
  const apiKey = process.env.OPENROUTER_API_KEY || process.env.API_KEY;
  const vercelInfo = await checkVercelStatus();
  const mode = detectMode();

  let github = {
    repo: repoFullName(),
    branch: repoBranch(),
    mode
  };

  if (mode === 'github') {
    github.connected = Boolean(auth.token);
    if (auth.token) {
      try {
        github.recent_commits = await gh.listCommits(auth.token, repoFullName(), repoBranch(), 5);
      } catch (err) {
        github.error = err.message;
      }
    }
  } else {
    const gitInfo = await getGitStatus();
    github = {
      ...github,
      branch: gitInfo.branch || repoBranch(),
      clean: gitInfo.clean,
      modified_count: gitInfo.modified_files ? gitInfo.modified_files.length : 0
    };
  }

  res.json({
    status: 'ok',
    harness: 'vanish',
    mode,
    auth: {
      authenticated: auth.authenticated,
      login: auth.login || null,
      avatar: auth.avatar || null,
      oauth_configured: oauthConfigured(),
      can_write: Boolean(auth.token)
    },
    has_api_key: Boolean(apiKey),
    key_prefix: auth.authenticated && apiKey ? `${apiKey.slice(0, 10)}...` : null,
    default_model: 'stealth/ox-alpha',
    github,
    vercel: vercelInfo
  });
});

// 1b. single-step llm relay — the browser-owned agent loop calls this once
// per turn. the function lives only for one llm round-trip (~3-25s), far
// below any platform time limit. state never lives here: conversation,
// staging area, and budgets are all client-side.
app.post('/api/agent/step', requireAuth, async (req, res) => {
  const apiKey = process.env.OPENROUTER_API_KEY || process.env.API_KEY;
  if (!apiKey) {
    return res.status(401).json({ error: 'no openrouter api key configured (OPENROUTER_API_KEY)' });
  }

  const {
    messages = [],
    model = 'stealth/ox-alpha',
    reasoningEffort = 'high'
  } = req.body;

  // sanitize: only well-formed conversation messages
  const safeMessages = Array.isArray(messages)
    ? messages.filter((m) =>
        m &&
        typeof m.role === 'string' &&
        ['system', 'user', 'assistant', 'tool'].includes(m.role)
      ).slice(-80)
    : [];

  if (safeMessages.length === 0) {
    return res.status(400).json({ error: 'messages array required' });
  }

  const ctx = toolCtx(req);

  res.writeHead(200, {
    'Content-Type': 'text/event-stream; charset=utf-8',
    'Cache-Control': 'no-cache, no-transform',
    'Connection': 'keep-alive',
    'X-Accel-Buffering': 'no'
  });
  if (typeof res.flushHeaders === 'function') res.flushHeaders();

  const sendEvent = (eventData) => {
    if (!res.writableEnded) {
      res.write(`data: ${JSON.stringify(eventData)}\n\n`);
    }
  };

  try {
    sendEvent({ type: 'step_started' });

    const { message } = await runLLMStep({
      apiKey,
      messages: safeMessages,
      model,
      reasoningEffort,
      tools: getToolDefinitions(ctx.mode),
      signal: req.signal // express 5 populates this; harmless if undefined
    });

    sendEvent({ type: 'step_complete', message });
  } catch (err) {
    if (err.name !== 'AbortError') {
      sendEvent({ type: 'step_error', error: err.message || String(err) });
    }
  } finally {
    res.write('data: [DONE]\n\n');
    res.end();
  }
});

// 1c. commit-files relay — flushes the client's staging area to github as one
// atomic commit. replaces the server-resident staging area entirely.
app.post('/api/git/commit-files', requireAuth, async (req, res) => {
  if (detectMode() !== 'github') {
    return res.status(400).json({
      success: false,
      error: 'commit-files is cloud-mode only; local mode uses git via run_command.'
    });
  }
  try {
    const { message, files } = req.body;
    if (!Array.isArray(files) || files.length === 0) {
      return res.status(400).json({ success: false, error: 'files array required' });
    }

    const cleanFiles = files
      .filter((f) => f && typeof f.path === 'string' && typeof f.content === 'string')
      .map((f) => ({ path: f.path.replace(/^\/+/, ''), content: f.content }))
      .slice(0, 50); // sanity cap

    if (cleanFiles.length === 0) {
      return res.status(400).json({ success: false, error: 'no valid files to commit' });
    }

    const result = await gh.commitFiles(
      req.auth.token,
      repoFullName(),
      repoBranch(),
      message || 'update vanish harness via browser loop',
      cleanFiles
    );
    res.json({ ...result, note: 'committed to github. vercel will auto-deploy.' });
  } catch (err) {
    res.status(500).json({ success: false, error: err.message });
  }
});

// 2. autonomous agent loop endpoint with live sse streaming
app.post('/api/agent/run', requireAuth, async (req, res) => {
  const apiKey = process.env.OPENROUTER_API_KEY || process.env.API_KEY;
  if (!apiKey) {
    return res.status(401).json({
      error: 'no openrouter api key configured (OPENROUTER_API_KEY)'
    });
  }

  const ctx = toolCtx(req);
  if (ctx.mode === 'github' && !ctx.token) {
    return res.status(403).json({
      error: 'this session has no github token, so the agent cannot read or write the repository. sign in with github.',
      login_url: '/api/auth/github'
    });
  }

  const {
    prompt,
    model = 'stealth/ox-alpha',
    reasoningEffort = 'high',
    maxSteps = 20,
    keepGoing = false,
    loopMode = false
  } = req.body;

  // sanitize incoming history: only well-formed conversation messages,
  // capped so a runaway session cannot exceed provider limits.
  const safeHistory = Array.isArray(req.body.history)
    ? req.body.history
        .filter(
          (m) =>
            m &&
            typeof m.role === 'string' &&
            ((m.role === 'tool' &&
              typeof m.tool_call_id === 'string' &&
              (typeof m.content === 'string' || m.content === null)) ||
              ((m.role === 'user' || m.role === 'assistant') &&
                (typeof m.content === 'string' || m.content === null || Array.isArray(m.tool_calls))))
        )
        .slice(-60)
    : [];

  res.writeHead(200, {
    'Content-Type': 'text/event-stream; charset=utf-8',
    'Cache-Control': 'no-cache, no-transform',
    'Connection': 'keep-alive',
    'X-Accel-Buffering': 'no'
  });
  if (typeof res.flushHeaders === 'function') {
    res.flushHeaders();
  }

  const abortController = new AbortController();
  res.on('close', () => {
    if (!res.writableFinished) {
      abortController.abort();
    }
  });

  const sendEvent = (eventData) => {
    if (!res.writableEnded) {
      res.write(`data: ${JSON.stringify(eventData)}\n\n`);
    }
  };

  try {
    await runAgentLoop({
      prompt,
      history: safeHistory,
      apiKey,
      model,
      reasoningEffort,
      maxSteps: Math.min(Number(maxSteps) || 20, 100),
      keepGoing: keepGoing === true && loopMode !== true,
      loopMode: loopMode === true,
      signal: abortController.signal,
      onEvent: sendEvent,
      toolContext: ctx
    });

    sendEvent({ type: 'done' });
    res.write('data: [DONE]\n\n');
    res.end();
  } catch (err) {
    if (!res.writableEnded) {
      sendEvent({ type: 'error', error: err.message || 'agent error' });
      res.write('data: [DONE]\n\n');
      res.end();
    }
  }
});

// 3. workspace file tree endpoint
app.get('/api/files/tree', requireAuth, async (req, res) => {
  try {
    if (detectMode() === 'github') {
      const flat = await gh.getTree(req.auth.token, repoFullName(), repoBranch());
      return res.json({ success: true, source: `github:${repoBranch()}`, tree: nestPaths(flat) });
    }

    const buildTree = (dir, relPath = '') => {
      const items = fs.readdirSync(dir, { withFileTypes: true });
      const nodes = [];

      for (const item of items) {
        if (item.name === 'node_modules' || item.name === '.git' || item.name === '.vercel') {
          continue;
        }
        const itemRelPath = path.join(relPath, item.name).replace(/\\/g, '/');
        const itemAbsPath = path.join(dir, item.name);

        if (item.isDirectory()) {
          nodes.push({
            name: item.name.toLowerCase(),
            path: itemRelPath.toLowerCase(),
            type: 'directory',
            children: buildTree(itemAbsPath, itemRelPath)
          });
        } else {
          nodes.push({
            name: item.name.toLowerCase(),
            path: itemRelPath.toLowerCase(),
            type: 'file',
            size: fs.statSync(itemAbsPath).size
          });
        }
      }
      return nodes;
    };

    res.json({ success: true, source: 'local', tree: buildTree(WORKSPACE_ROOT) });
  } catch (err) {
    res.status(500).json({ error: err.message });
  }
});

// 4. file content read endpoint
app.get('/api/files/read', requireAuth, async (req, res) => {
  try {
    const relPath = req.query.path;
    if (!relPath) return res.status(400).json({ error: 'path parameter required' });

    if (detectMode() === 'github') {
      const file = await gh.readFile(req.auth.token, repoFullName(), relPath, repoBranch());
      return res.json({ success: true, path: relPath, content: file.content, sha: file.sha });
    }

    const safePath = path.resolve(WORKSPACE_ROOT, relPath);
    if (!safePath.startsWith(WORKSPACE_ROOT)) {
      return res.status(403).json({ error: 'access denied' });
    }
    if (!fs.existsSync(safePath)) {
      return res.status(404).json({ error: 'file not found' });
    }

    const content = fs.readFileSync(safePath, 'utf8');
    res.json({ success: true, path: relPath, content });
  } catch (err) {
    const status = err.status === 404 ? 404 : 500;
    res.status(status).json({ error: err.message });
  }
});

// 5. file content write endpoint
//
// in cloud mode there is nowhere to "save" a file to: the function filesystem
// is read-only and disappears. so a save from the ide commits straight to
// github, which is also what triggers the redeploy.
app.post('/api/files/write', requireAuth, async (req, res) => {
  try {
    const { path: relPath, content, message } = req.body;
    if (!relPath || typeof content !== 'string') {
      return res.status(400).json({ error: 'path and content required' });
    }

    if (detectMode() === 'github') {
      const result = await gh.commitFiles(
        req.auth.token,
        repoFullName(),
        repoBranch(),
        message || `update ${relPath} via vanish ide`,
        [{ path: relPath.replace(/^\/+/, ''), content }]
      );
      return res.json({
        ...result,
        path: relPath,
        bytes: Buffer.byteLength(content, 'utf8'),
        note: 'committed to github, vercel will redeploy'
      });
    }

    const safePath = path.resolve(WORKSPACE_ROOT, relPath);
    if (!safePath.startsWith(WORKSPACE_ROOT)) {
      return res.status(403).json({ error: 'access denied' });
    }

    const parentDir = path.dirname(safePath);
    if (!fs.existsSync(parentDir)) {
      fs.mkdirSync(parentDir, { recursive: true });
    }

    fs.writeFileSync(safePath, content, 'utf8');
    res.json({ success: true, path: relPath, bytes: Buffer.byteLength(content, 'utf8') });
  } catch (err) {
    res.status(500).json({ error: err.message });
  }
});

// 6. git endpoints
app.get('/api/git/status', requireAuth, async (req, res) => {
  if (detectMode() === 'github') {
    try {
      const commits = await gh.listCommits(req.auth.token, repoFullName(), repoBranch(), 10);
      return res.json({
        mode: 'github',
        repo: repoFullName(),
        branch: repoBranch(),
        clean: true,
        modified_files: [],
        note: 'cloud mode writes commit directly to github, so there is no persistent working tree.',
        recent_commits: commits
      });
    } catch (err) {
      return res.status(500).json({ error: err.message });
    }
  }
  res.json(await getGitStatus());
});

app.get('/api/git/diff', requireAuth, async (req, res) => {
  if (detectMode() === 'github') {
    try {
      const sha = req.query.sha || repoBranch();
      const result = await gh.getCommitDiff(req.auth.token, repoFullName(), sha);
      return res.json({ mode: 'github', ...result });
    } catch (err) {
      return res.status(500).json({ error: err.message });
    }
  }
  res.json(await getGitDiff(req.query.staged === 'true'));
});

app.post('/api/git/commit', requireAuth, async (req, res) => {
  if (detectMode() === 'github') {
    return res.status(400).json({
      success: false,
      error: 'in cloud mode there is no staging area between requests. saving a file from the ide commits it, and the agent commits its own batched changes with git_commit.'
    });
  }
  res.json(await gitCommit(req.body.message || 'update vanish harness'));
});

app.post('/api/git/push', requireAuth, async (req, res) => {
  if (detectMode() === 'github') {
    return res.json({
      success: true,
      note: 'cloud mode commits write straight to origin via the github api, so there is nothing to push.'
    });
  }
  res.json(await gitPush(req.body.branch || 'main'));
});

// 7. vercel deployment endpoint
app.post('/api/deploy/vercel', requireAuth, async (req, res) => {
  res.json(await deployToVercel(req.body.prod === true));
});

// 8. chat completions sse proxy fallback
app.post('/api/chat', requireAuth, async (req, res) => {
  const apiKey = process.env.OPENROUTER_API_KEY || process.env.API_KEY;
  if (!apiKey) {
    return res.status(401).json({ error: 'no openrouter api key configured (OPENROUTER_API_KEY)' });
  }

  const {
    messages = [],
    model = 'stealth/ox-alpha',
    temperature = 1,
    top_p = 0.95,
    max_tokens,
    reasoning = { effort: 'high' }
  } = req.body;

  res.writeHead(200, {
    'Content-Type': 'text/event-stream; charset=utf-8',
    'Cache-Control': 'no-cache, no-transform',
    'Connection': 'keep-alive',
    'X-Accel-Buffering': 'no'
  });
  if (typeof res.flushHeaders === 'function') {
    res.flushHeaders();
  }

  const abortController = new AbortController();
  res.on('close', () => {
    if (!res.writableFinished) {
      abortController.abort();
    }
  });

  try {
    const openrouterRes = await fetch('https://openrouter.ai/api/v1/chat/completions', {
      method: 'POST',
      headers: {
        'Authorization': `Bearer ${apiKey}`,
        'Content-Type': 'application/json',
        'HTTP-Referer': process.env.PUBLIC_BASE_URL || 'https://vanish.vercel.app',
        'X-Title': 'vanish web harness'
      },
      body: JSON.stringify({
        model: model || 'stealth/ox-alpha',
        messages,
        stream: true,
        temperature: Number(temperature),
        top_p: Number(top_p),
        max_tokens: max_tokens ? Number(max_tokens) : undefined,
        reasoning
      }),
      signal: abortController.signal
    });

    if (!openrouterRes.ok) {
      const errText = await openrouterRes.text();
      res.write(`data: ${JSON.stringify({ error: errText })}\n\n`);
      res.write('data: [DONE]\n\n');
      return res.end();
    }

    const reader = openrouterRes.body.getReader();
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
        if (trimmed.startsWith('data: ')) {
          res.write(`${trimmed}\n\n`);
        }
      }
    }
    res.write('data: [DONE]\n\n');
    res.end();
  } catch (err) {
    if (!res.writableEnded) {
      res.write(`data: ${JSON.stringify({ error: err.message })}\n\n`);
      res.end();
    }
  }
});

// spa-ish fallback so a hard refresh on the harness url still serves the ide
app.get('*', (req, res, next) => {
  if (req.path.startsWith('/api/')) return next();
  const indexPath = path.join(PUBLIC_DIR, 'index.html');
  if (fs.existsSync(indexPath)) return res.sendFile(indexPath);
  next();
});

export { app, ROOT, PUBLIC_DIR, isCloud };
export default app;
