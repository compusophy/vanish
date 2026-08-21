import express from 'express';
import cors from 'cors';
import dotenv from 'dotenv';
import path from 'path';
import fs from 'fs';
import { fileURLToPath } from 'url';
import { runAgentLoop } from './lib/agent.js';
import { getGitStatus, getGitDiff, gitCommit, gitPush } from './lib/git-service.js';
import { checkVercelStatus, deployToVercel } from './lib/vercel-service.js';

dotenv.config();

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);

const app = express();
const PORT = process.env.PORT || 3000;
const WORKSPACE_ROOT = process.cwd();

app.use(cors());
app.use(express.json({ limit: '50mb' }));
app.use(express.static(path.join(__dirname, 'public')));

// 1. health & platform status endpoint
app.get('/api/status', async (req, res) => {
  const apiKey = process.env.OPENROUTER_API_KEY || process.env.API_KEY;
  const vercelInfo = await checkVercelStatus();
  const gitInfo = await getGitStatus();

  res.json({
    status: 'ok',
    harness: 'vanish',
    has_api_key: Boolean(apiKey),
    key_prefix: apiKey ? `${apiKey.slice(0, 10)}...` : null,
    default_model: 'stealth/ox-alpha',
    github: {
      repo: 'compusophy/vanish',
      branch: gitInfo.branch || 'main',
      clean: gitInfo.clean,
      modified_count: gitInfo.modified_files ? gitInfo.modified_files.length : 0
    },
    vercel: vercelInfo
  });
});

// 2. autonomous agent loop endpoint with live sse streaming
app.post('/api/agent/run', async (req, res) => {
  const apiKey = process.env.OPENROUTER_API_KEY || process.env.API_KEY;
  if (!apiKey) {
    return res.status(401).json({
      error: 'no openrouter api key found in .env'
    });
  }

  const {
    prompt,
    history = [],
    model = 'stealth/ox-alpha',
    reasoningEffort = 'high',
    maxSteps = 10
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

  const sendEvent = (eventData) => {
    if (!res.writableEnded) {
      res.write(`data: ${JSON.stringify(eventData)}\n\n`);
    }
  };

  try {
    await runAgentLoop({
      prompt,
      history,
      apiKey,
      model,
      reasoningEffort,
      maxSteps: Math.min(Number(maxSteps) || 10, 25),
      signal: abortController.signal,
      onEvent: sendEvent
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
app.get('/api/files/tree', (req, res) => {
  try {
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

    const tree = buildTree(WORKSPACE_ROOT);
    res.json({ success: true, tree });
  } catch (err) {
    res.status(500).json({ error: err.message });
  }
});

// 4. file content read endpoint
app.get('/api/files/read', (req, res) => {
  try {
    const relPath = req.query.path;
    if (!relPath) return res.status(400).json({ error: 'path parameter required' });

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
    res.status(500).json({ error: err.message });
  }
});

// 5. file content write endpoint
app.post('/api/files/write', (req, res) => {
  try {
    const { path: relPath, content } = req.body;
    if (!relPath || typeof content !== 'string') {
      return res.status(400).json({ error: 'path and content required' });
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
app.get('/api/git/status', async (req, res) => {
  const status = await getGitStatus();
  res.json(status);
});

app.get('/api/git/diff', async (req, res) => {
  const staged = req.query.staged === 'true';
  const diffData = await getGitDiff(staged);
  res.json(diffData);
});

app.post('/api/git/commit', async (req, res) => {
  const { message } = req.body;
  const result = await gitCommit(message || 'update vanish harness');
  res.json(result);
});

app.post('/api/git/push', async (req, res) => {
  const { branch = 'main' } = req.body;
  const result = await gitPush(branch);
  res.json(result);
});

// 7. vercel deployment endpoint
app.post('/api/deploy/vercel', async (req, res) => {
  const { prod = false } = req.body;
  const result = await deployToVercel(prod);
  res.json(result);
});

// 8. chat completions sse proxy fallback
app.post('/api/chat', async (req, res) => {
  const apiKey = process.env.OPENROUTER_API_KEY || process.env.API_KEY;
  if (!apiKey) {
    return res.status(401).json({ error: 'no openrouter api key found in .env' });
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
        'HTTP-Referer': 'http://localhost:3001',
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

function startServer(port) {
  const server = app.listen(port, () => {
    console.log(`\n========================================`);
    console.log(`vanish harness running on http://localhost:${port}`);
    console.log(`model: stealth/ox-alpha`);
    console.log(`github repo: https://github.com/compusophy/vanish`);
    console.log(`========================================\n`);
  });

  server.on('error', (err) => {
    if (err.code === 'EADDRINUSE') {
      console.log(`port ${port} is in use, trying port ${port + 1}...`);
      startServer(port + 1);
    } else {
      console.error('server error:', err);
    }
  });
}

startServer(PORT);
