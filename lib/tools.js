import fs from 'fs';
import path from 'path';
import { exec } from 'child_process';
import util from 'util';
import * as gh from './github-service.js';
import { checkSources, describeFailures } from './syntax-check.js';
import {
  getDeploymentSignal,
  waitForDeploymentSignal,
  deploymentSource
} from './deploy-feedback.js';

const execPromise = util.promisify(exec);

// workspace root path
const WORKSPACE_ROOT = process.cwd();

// vanish runs in two very different worlds:
//
//   local  - a real checkout. writable fs, a git binary, the vercel cli.
//   github - a vercel function. read-only fs, no git, no cli. every file
//            operation is routed through the github rest api instead, and
//            writes accumulate in an in-memory staging area that git_commit
//            flushes as a single atomic commit.
//
// the tool surface the model sees is deliberately identical in both modes so
// the same prompts and reasoning transfer over.
export function detectMode() {
  return process.env.VERCEL ? 'github' : 'local';
}

// helper to ensure paths stay within workspace
function resolveSafePath(userPath) {
  const resolved = path.resolve(WORKSPACE_ROOT, userPath || '.');
  if (!resolved.startsWith(WORKSPACE_ROOT)) {
    throw new Error(`path '${userPath}' is outside workspace boundaries`);
  }
  return resolved;
}

function normalizeRepoPath(p) {
  return String(p || '')
    .replace(/\\/g, '/')
    .replace(/^\.\/?/, '')
    .replace(/^\/+/, '');
}

function sliceWithLineNumbers(raw, startLine, endLine) {
  const lines = raw.split('\n');
  const totalLines = lines.length;
  const start = Math.max(1, startLine || 1);
  const end = Math.min(totalLines, endLine || totalLines);
  const content = lines
    .slice(start - 1, end)
    .map((line, idx) => `${start + idx}: ${line}`)
    .join('\n');
  return { totalLines, start, end, content };
}

// creates the per-run staging area used by github mode
export function createToolContext({ mode, token, repo, branch }) {
  return {
    mode: mode || detectMode(),
    token,
    repo,
    branch: branch || 'main',
    staged: new Map(),   // path -> new content (null means delete)
    baseline: new Map()  // path -> content at first touch (null means new file)
  };
}

// ---- tool definitions -------------------------------------------------

const READ_TOOLS = [
  {
    type: 'function',
    function: {
      name: 'read_file',
      description: 'read the content of a file in the workspace, optionally specifying line ranges.',
      parameters: {
        type: 'object',
        properties: {
          path: { type: 'string', description: 'relative path to the file' },
          start_line: { type: 'number', description: 'optional 1-indexed start line' },
          end_line: { type: 'number', description: 'optional 1-indexed end line' }
        },
        required: ['path']
      }
    }
  },
  {
    type: 'function',
    function: {
      name: 'write_file',
      description: 'create or completely overwrite a file with new content.',
      parameters: {
        type: 'object',
        properties: {
          path: { type: 'string', description: 'relative path to the file to write' },
          content: { type: 'string', description: 'full content to write into the file' }
        },
        required: ['path', 'content']
      }
    }
  },
  {
    type: 'function',
    function: {
      name: 'edit_file',
      description: 'replace exact target text with replacement text in a specified file.',
      parameters: {
        type: 'object',
        properties: {
          path: { type: 'string', description: 'relative path to the file' },
          target: { type: 'string', description: 'exact character sequence to find and replace' },
          replacement: { type: 'string', description: 'new character sequence to replace target with' }
        },
        required: ['path', 'target', 'replacement']
      }
    }
  },
  {
    type: 'function',
    function: {
      name: 'list_dir',
      description: 'list files and directories in a given workspace directory.',
      parameters: {
        type: 'object',
        properties: {
          path: { type: 'string', description: 'relative path to directory (defaults to .)' },
          recursive: { type: 'boolean', description: 'whether to list recursively (defaults to false)' }
        }
      }
    }
  }
];

const RUN_COMMAND_TOOL = {
  type: 'function',
  function: {
    name: 'run_command',
    description: 'run a shell/terminal command locally in the workspace (e.g. npm test, npm install, git, node, vercel).',
    parameters: {
      type: 'object',
      properties: {
        command: { type: 'string', description: 'shell command string to execute' },
        timeout_ms: { type: 'number', description: 'execution timeout in ms (default 30000)' }
      },
      required: ['command']
    }
  }
};

function gitTools(mode) {
  const commitDescription = mode === 'github'
    ? 'commit all staged file changes directly to github as a single atomic commit on the connected branch. this immediately triggers a vercel deployment.'
    : 'stage all changes and create a git commit with a descriptive message.';

  const pushDescription = mode === 'github'
    ? 'confirm the push state. in cloud mode git_commit already writes straight to github, so this only reports the current branch and deployment status.'
    : 'push committed changes to github (origin main).';

  return [
    {
      type: 'function',
      function: {
        name: 'git_status',
        description: 'get current git status and list modified, added, or untracked files.',
        parameters: { type: 'object', properties: {} }
      }
    },
    {
      type: 'function',
      function: {
        name: 'git_diff',
        description: 'get git diff of unstaged or staged changes across the repository.',
        parameters: {
          type: 'object',
          properties: {
            staged: { type: 'boolean', description: 'whether to show staged changes diff only' }
          }
        }
      }
    },
    {
      type: 'function',
      function: {
        name: 'git_commit',
        description: commitDescription,
        parameters: {
          type: 'object',
          properties: { message: { type: 'string', description: 'git commit message' } },
          required: ['message']
        }
      }
    },
    {
      type: 'function',
      function: {
        name: 'git_push',
        description: pushDescription,
        parameters: {
          type: 'object',
          properties: { branch: { type: 'string', description: 'branch name (defaults to main)' } }
        }
      }
    },
    {
      type: 'function',
      function: {
        name: 'deploy_vercel',
        description: mode === 'github'
          ? 'report deployment status. vercel auto-deploys every commit pushed to the connected branch, so committing is deploying.'
          : 'trigger a vercel deployment for the vanish harness and return the deployment url.',
        parameters: {
          type: 'object',
          properties: { prod: { type: 'boolean', description: 'deploy to production (default false)' } }
        }
      }
    },
    {
      type: 'function',
      function: {
        name: 'check_deployment',
        description:
          'read the real result of a vercel build. call this after git_commit to find out whether the commit actually deployed, and when it failed to get the build error and the tail of the build log. this is the only way to see failures that happen after the code leaves this process.',
        parameters: {
          type: 'object',
          properties: {
            sha: {
              type: 'string',
              description: 'commit sha to look up. defaults to the most recent deployment.'
            },
            wait: {
              type: 'boolean',
              description: 'block until the build finishes (bounded, ~45s) instead of reporting the current in-progress state.'
            },
            log_lines: {
              type: 'number',
              description: 'how many trailing build-log lines to return on failure (default 40).'
            }
          }
        }
      }
    }
  ];
}

export function getToolDefinitions(mode = detectMode()) {
  // run_command is intentionally withheld in cloud mode: the harness is
  // reachable at a public url, and handing a model arbitrary shell execution
  // there would be remote code execution on the deployment.
  return mode === 'github'
    ? [...READ_TOOLS, ...gitTools(mode)]
    : [...READ_TOOLS, RUN_COMMAND_TOOL, ...gitTools(mode)];
}

// preserved for callers that expect the original local tool surface
export const toolDefinitions = getToolDefinitions('local');

// ---- github-mode helpers ---------------------------------------------

async function ghCurrentContent(ctx, relPath) {
  if (ctx.staged.has(relPath)) return ctx.staged.get(relPath);
  try {
    const file = await gh.readFile(ctx.token, ctx.repo, relPath, ctx.branch);
    return file.content;
  } catch (err) {
    if (err.status === 404) return null;
    throw err;
  }
}

async function ghStage(ctx, relPath, content) {
  if (!ctx.baseline.has(relPath)) {
    // remember the pre-edit state so git_diff can render a real diff later
    let original = null;
    try {
      const file = await gh.readFile(ctx.token, ctx.repo, relPath, ctx.branch);
      original = file.content;
    } catch (err) {
      if (err.status !== 404) throw err;
    }
    ctx.baseline.set(relPath, original);
  }
  ctx.staged.set(relPath, content);
}

// ---- guard rails ------------------------------------------------------

// nothing unparseable gets to leave this process. a broken commit is not a
// local error the model can see — it is a red deploy on a machine it cannot
// reach, and historically it kept committing on top of it for hours.
async function blockOnSyntaxErrors(files) {
  const failures = await checkSources(files);
  if (!failures.length) return null;
  return {
    success: false,
    error:
      `commit blocked: ${failures.length} file(s) failed to parse. fix them and commit again. ` +
      'committing this would fail the vercel build and take the live app down.',
    files: failures.map((f) => f.path),
    syntax_errors: describeFailures(failures)
  };
}

// ---- deployment feedback ---------------------------------------------

async function runCheckDeployment(args = {}, ctx = {}) {
  const source = deploymentSource(ctx);
  if (source === 'none') {
    return {
      error:
        'no way to read deployment results from here. in cloud mode a github ' +
        'session provides the build state for free; VERCEL_TOKEN adds the build log.'
    };
  }

  const logLines = Number(args.log_lines) > 0 ? Number(args.log_lines) : 40;
  const sha = args.sha || ctx.lastCommit?.sha || null;

  if (args.wait) {
    return await waitForDeploymentSignal({ ctx, sha, timeoutMs: 45000, pollMs: 3000 });
  }
  return await getDeploymentSignal({ ctx, sha, logLines });
}

// ---- tool execution ---------------------------------------------------

export async function executeTool(name, args = {}, ctx = {}) {
  const mode = ctx.mode || detectMode();

  try {
    if (mode === 'github') {
      if (!ctx.token) {
        return { error: 'no github token on this session. sign in with github to give vanish write access.' };
      }
      if (!ctx.repo) {
        return { error: 'GITHUB_REPO is not configured for this deployment.' };
      }
      return await executeGithubTool(name, args, ctx);
    }
    return await executeLocalTool(name, args, ctx);
  } catch (err) {
    return { error: err.message || String(err) };
  }
}

async function executeGithubTool(name, args, ctx) {
  switch (name) {
    case 'read_file': {
      const relPath = normalizeRepoPath(args.path);
      const raw = await ghCurrentContent(ctx, relPath);
      if (raw === null) return { error: `file not found: ${args.path}` };

      const { totalLines, start, end, content } = sliceWithLineNumbers(
        raw,
        args.start_line,
        args.end_line
      );
      return {
        path: relPath,
        source: ctx.staged.has(relPath) ? 'staged (uncommitted)' : `github:${ctx.branch}`,
        total_lines: totalLines,
        start_line: start,
        end_line: end,
        content
      };
    }

    case 'write_file': {
      const relPath = normalizeRepoPath(args.path);
      await ghStage(ctx, relPath, args.content);
      return {
        success: true,
        path: relPath,
        bytes_written: Buffer.byteLength(args.content, 'utf8'),
        staged: true,
        note: 'staged in memory. call git_commit to write it to github and trigger a deploy.'
      };
    }

    case 'edit_file': {
      const relPath = normalizeRepoPath(args.path);
      const content = await ghCurrentContent(ctx, relPath);
      if (content === null) return { error: `file not found: ${args.path}` };

      if (!content.includes(args.target)) {
        return {
          error: `target substring not found in ${relPath}. make sure exact whitespace and character matches exist.`
        };
      }
      const occurrences = content.split(args.target).length - 1;
      if (occurrences > 1) {
        return {
          error: `target substring found ${occurrences} times in ${relPath}. please provide more context to uniquely identify the replacement chunk.`
        };
      }

      await ghStage(ctx, relPath, content.replace(args.target, args.replacement));
      return {
        success: true,
        path: relPath,
        occurrences_replaced: 1,
        staged: true,
        note: 'staged in memory. call git_commit to write it to github and trigger a deploy.'
      };
    }

    case 'list_dir': {
      const dirPath = normalizeRepoPath(args.path || '');
      let entries;

      if (args.recursive) {
        const tree = await gh.getTree(ctx.token, ctx.repo, ctx.branch);
        entries = tree
          .filter((n) => (dirPath ? n.path.startsWith(`${dirPath}/`) || n.path === dirPath : true))
          .map((n) => ({ name: n.path.split('/').pop(), path: n.path, type: n.type, size: n.size }));
      } else {
        entries = await gh.listDir(ctx.token, ctx.repo, dirPath, ctx.branch);
      }

      // surface files the agent created this run that github does not know about yet
      const known = new Set(entries.map((e) => e.path));
      for (const [stagedPath, stagedContent] of ctx.staged) {
        if (known.has(stagedPath) || stagedContent === null) continue;
        const inScope = dirPath
          ? stagedPath.startsWith(`${dirPath}/`)
          : !stagedPath.includes('/') || args.recursive;
        if (!inScope) continue;
        entries.push({
          name: stagedPath.split('/').pop(),
          path: stagedPath,
          type: 'file',
          size: Buffer.byteLength(stagedContent, 'utf8'),
          staged: true
        });
      }

      return { path: dirPath || '.', ref: ctx.branch, entries };
    }

    case 'run_command':
      return {
        error: 'run_command is disabled in cloud mode. this harness is deployed as a vercel function on a public url, so arbitrary shell execution is not available. use read_file / write_file / edit_file and git_commit instead, which apply changes through the github api.'
      };

    case 'git_status': {
      if (!ctx.staged.size) {
        return {
          mode: 'github',
          branch: ctx.branch,
          status_summary: 'clean working tree (no staged modifications)'
        };
      }
      const summary = [...ctx.staged.entries()]
        .map(([p, content]) => {
          if (content === null) return ` D ${p}`;
          return ctx.baseline.get(p) === null ? ` A ${p}` : ` M ${p}`;
        })
        .join('\n');
      return { mode: 'github', branch: ctx.branch, status_summary: summary };
    }

    case 'git_diff': {
      if (!ctx.staged.size) return { mode: 'github', diff: 'no diff' };

      let added = 0;
      let removed = 0;
      const chunks = [];
      for (const [p, after] of ctx.staged) {
        const before = ctx.baseline.get(p) ?? null;
        const d = gh.unifiedDiff(p, before, after);
        chunks.push(d.diff);
        added += d.added;
        removed += d.removed;
      }
      return {
        mode: 'github',
        branch: ctx.branch,
        files_changed: ctx.staged.size,
        additions: added,
        deletions: removed,
        diff: chunks.join('\n\n')
      };
    }

    case 'git_commit': {
      if (!ctx.staged.size) {
        return { success: false, error: 'nothing staged to commit. write or edit a file first.' };
      }
      const files = [...ctx.staged.entries()].map(([p, content]) => ({ path: p, content }));

      const blocked = await blockOnSyntaxErrors(files);
      if (blocked) return blocked;

      const result = await gh.commitFiles(
        ctx.token,
        ctx.repo,
        ctx.branch,
        args.message || 'update vanish harness',
        files
      );

      if (result.success) {
        ctx.staged.clear();
        ctx.baseline.clear();
        ctx.lastCommit = result;
      }
      return {
        ...result,
        note: 'committed to github. vercel will auto-deploy this commit.'
      };
    }

    case 'git_push': {
      const branch = args.branch || ctx.branch;
      return {
        success: true,
        branch,
        note: `in cloud mode git_commit writes directly to origin/${branch} via the github api, so there is nothing left to push.`,
        last_commit: ctx.lastCommit || null
      };
    }

    case 'deploy_vercel': {
      const hook = process.env.VERCEL_DEPLOY_HOOK_URL;
      if (hook) {
        const res = await fetch(hook, { method: 'POST' });
        return {
          success: res.ok,
          triggered: 'deploy hook',
          status: res.status
        };
      }
      return {
        success: true,
        note: `this project is connected to github with continuous deployment. every commit to ${ctx.branch} deploys automatically, so git_commit is the deploy step. call check_deployment to see whether the build actually passed.`,
        last_commit: ctx.lastCommit || null,
        latest_deployment: await runCheckDeployment({ sha: ctx.lastCommit?.sha }, ctx)
      };
    }

    case 'check_deployment':
      return await runCheckDeployment(args, ctx);

    default:
      return { error: `unknown tool '${name}'` };
  }
}

async function executeLocalTool(name, args, ctx = {}) {
  switch (name) {
    case 'read_file': {
      const targetPath = resolveSafePath(args.path);
      if (!fs.existsSync(targetPath)) {
        return { error: `file not found: ${args.path}` };
      }
      const raw = fs.readFileSync(targetPath, 'utf8');
      const { totalLines, start, end, content } = sliceWithLineNumbers(
        raw,
        args.start_line,
        args.end_line
      );
      return {
        path: args.path,
        total_lines: totalLines,
        start_line: start,
        end_line: end,
        content
      };
    }

    case 'write_file': {
      const targetPath = resolveSafePath(args.path);
      const parentDir = path.dirname(targetPath);
      if (!fs.existsSync(parentDir)) {
        fs.mkdirSync(parentDir, { recursive: true });
      }
      fs.writeFileSync(targetPath, args.content, 'utf8');
      return {
        success: true,
        path: args.path,
        bytes_written: Buffer.byteLength(args.content, 'utf8')
      };
    }

    case 'edit_file': {
      const targetPath = resolveSafePath(args.path);
      if (!fs.existsSync(targetPath)) {
        return { error: `file not found: ${args.path}` };
      }
      const content = fs.readFileSync(targetPath, 'utf8');
      if (!content.includes(args.target)) {
        return {
          error: `target substring not found in ${args.path}. make sure exact whitespace and character matches exist.`
        };
      }
      const occurrences = content.split(args.target).length - 1;
      if (occurrences > 1) {
        return {
          error: `target substring found ${occurrences} times in ${args.path}. please provide more context to uniquely identify the replacement chunk.`
        };
      }
      fs.writeFileSync(targetPath, content.replace(args.target, args.replacement), 'utf8');
      return { success: true, path: args.path, occurrences_replaced: 1 };
    }

    case 'list_dir': {
      const dirPath = resolveSafePath(args.path || '.');
      if (!fs.existsSync(dirPath)) {
        return { error: `directory not found: ${args.path}` };
      }

      const entries = [];
      function scan(curDir, relBase, depth) {
        if (depth > 4) return;
        const items = fs.readdirSync(curDir, { withFileTypes: true });
        for (const item of items) {
          if (item.name === 'node_modules' || item.name === '.git' || item.name === '.vercel') continue;
          const relItem = path.join(relBase, item.name).replace(/\\/g, '/');
          entries.push({
            name: item.name,
            path: relItem,
            type: item.isDirectory() ? 'directory' : 'file',
            size: item.isFile() ? fs.statSync(path.join(curDir, item.name)).size : undefined
          });
          if (args.recursive && item.isDirectory()) {
            scan(path.join(curDir, item.name), relItem, depth + 1);
          }
        }
      }
      scan(dirPath, args.path || '', 1);
      return { path: args.path || '.', entries };
    }

    case 'run_command': {
      const timeout = args.timeout_ms || 30000;
      try {
        const { stdout, stderr } = await execPromise(args.command, {
          cwd: WORKSPACE_ROOT,
          timeout,
          maxBuffer: 1024 * 1024 * 5
        });
        return { command: args.command, exit_code: 0, stdout: stdout || '', stderr: stderr || '' };
      } catch (execErr) {
        return {
          command: args.command,
          exit_code: execErr.code || 1,
          stdout: execErr.stdout || '',
          stderr: execErr.stderr || execErr.message || ''
        };
      }
    }

    case 'git_status': {
      const { stdout } = await execPromise('git status --short', { cwd: WORKSPACE_ROOT });
      return { status_summary: stdout.trim() || 'clean working tree (no modifications)' };
    }

    case 'git_diff': {
      const flag = args.staged ? '--staged' : '';
      const { stdout } = await execPromise(`git diff ${flag}`, { cwd: WORKSPACE_ROOT });
      return { diff: stdout.trim() || 'no diff' };
    }

    case 'git_commit': {
      // same gate as cloud mode: read back every file about to be committed
      // and refuse if any of them fails to parse.
      const { stdout: statusOut } = await execPromise('git status --porcelain', {
        cwd: WORKSPACE_ROOT
      });
      const changed = statusOut
        .split('\n')
        .map((line) => line.slice(3).trim())
        .filter(Boolean)
        .map((p) => (p.includes(' -> ') ? p.split(' -> ').pop() : p))
        .map((p) => p.replace(/^"|"$/g, ''));

      const files = [];
      for (const relPath of changed) {
        const abs = path.resolve(WORKSPACE_ROOT, relPath);
        if (!abs.startsWith(WORKSPACE_ROOT) || !fs.existsSync(abs)) continue;
        if (fs.statSync(abs).isDirectory()) continue;
        files.push({ path: relPath, content: fs.readFileSync(abs, 'utf8') });
      }

      const blocked = await blockOnSyntaxErrors(files);
      if (blocked) return blocked;

      await execPromise('git add -A', { cwd: WORKSPACE_ROOT });
      const safeMsg = (args.message || 'update harness via vanish').replace(/"/g, '\\"');
      const { stdout } = await execPromise(`git commit -m "${safeMsg}"`, { cwd: WORKSPACE_ROOT });
      return { success: true, commit_output: stdout.trim() };
    }

    case 'git_push': {
      const branch = args.branch || 'main';
      const { stdout, stderr } = await execPromise(`git push origin ${branch}`, { cwd: WORKSPACE_ROOT });
      return { success: true, branch, output: stdout || stderr || 'pushed to github' };
    }

    case 'deploy_vercel': {
      const flag = args.prod ? '--prod' : '';
      const { stdout, stderr } = await execPromise(`vercel ${flag} --yes`, {
        cwd: WORKSPACE_ROOT,
        timeout: 120000
      });
      return { success: true, output: stdout || stderr || 'vercel deploy initiated' };
    }

    case 'check_deployment':
      return await runCheckDeployment(args, ctx);

    default:
      return { error: `unknown tool '${name}'` };
  }
}
