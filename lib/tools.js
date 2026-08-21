import fs from 'fs';
import path from 'path';
import { exec } from 'child_process';
import util from 'util';

const execPromise = util.promisify(exec);

// workspace root path
const WORKSPACE_ROOT = process.cwd();

// helper to ensure paths stay within workspace
function resolveSafePath(userPath) {
  const resolved = path.resolve(WORKSPACE_ROOT, userPath || '.');
  if (!resolved.startsWith(WORKSPACE_ROOT)) {
    throw new Error(`path '${userPath}' is outside workspace boundaries`);
  }
  return resolved;
}

// tool definitions for openai/openrouter schema
export const toolDefinitions = [
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
  },
  {
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
  },
  {
    type: 'function',
    function: {
      name: 'git_status',
      description: 'get current git status and list modified, added, or untracked files.',
      parameters: {
        type: 'object',
        properties: {}
      }
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
      description: 'stage all changes and create a git commit with a descriptive message.',
      parameters: {
        type: 'object',
        properties: {
          message: { type: 'string', description: 'git commit message' }
        },
        required: ['message']
      }
    }
  },
  {
    type: 'function',
    function: {
      name: 'git_push',
      description: 'push committed changes to github (origin main).',
      parameters: {
        type: 'object',
        properties: {
          branch: { type: 'string', description: 'branch name (defaults to main)' }
        }
      }
    }
  },
  {
    type: 'function',
    function: {
      name: 'deploy_vercel',
      description: 'trigger a vercel deployment for the vanish harness and return the deployment url.',
      parameters: {
        type: 'object',
        properties: {
          prod: { type: 'boolean', description: 'deploy to production (default false)' }
        }
      }
    }
  }
];

// tool execution handlers
export async function executeTool(name, args = {}) {
  try {
    switch (name) {
      case 'read_file': {
        const targetPath = resolveSafePath(args.path);
        if (!fs.existsSync(targetPath)) {
          return { error: `file not found: ${args.path}` };
        }
        const raw = fs.readFileSync(targetPath, 'utf8');
        const lines = raw.split('\n');
        const totalLines = lines.length;

        const start = Math.max(1, args.start_line || 1);
        const end = Math.min(totalLines, args.end_line || totalLines);

        const sliced = lines.slice(start - 1, end).map((line, idx) => {
          return `${start + idx}: ${line}`;
        }).join('\n');

        return {
          path: args.path,
          total_lines: totalLines,
          start_line: start,
          end_line: end,
          content: sliced
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
        const updated = content.replace(args.target, args.replacement);
        fs.writeFileSync(targetPath, updated, 'utf8');
        return {
          success: true,
          path: args.path,
          occurrences_replaced: 1
        };
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
          return {
            command: args.command,
            exit_code: 0,
            stdout: stdout || '',
            stderr: stderr || ''
          };
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
        return {
          status_summary: stdout.trim() || 'clean working tree (no modifications)'
        };
      }

      case 'git_diff': {
        const flag = args.staged ? '--staged' : '';
        const { stdout } = await execPromise(`git diff ${flag}`, { cwd: WORKSPACE_ROOT });
        return {
          diff: stdout.trim() || 'no diff'
        };
      }

      case 'git_commit': {
        await execPromise('git add -A', { cwd: WORKSPACE_ROOT });
        const safeMsg = (args.message || 'update harness via vanish').replace(/"/g, '\\"');
        const { stdout } = await execPromise(`git commit -m "${safeMsg}"`, { cwd: WORKSPACE_ROOT });
        return {
          success: true,
          commit_output: stdout.trim()
        };
      }

      case 'git_push': {
        const branch = args.branch || 'main';
        const { stdout, stderr } = await execPromise(`git push origin ${branch}`, { cwd: WORKSPACE_ROOT });
        return {
          success: true,
          branch,
          output: stdout || stderr || 'pushed to github'
        };
      }

      case 'deploy_vercel': {
        const flag = args.prod ? '--prod' : '';
        const { stdout, stderr } = await execPromise(`vercel ${flag} --yes`, {
          cwd: WORKSPACE_ROOT,
          timeout: 120000
        });
        return {
          success: true,
          output: stdout || stderr || 'vercel deploy initiated'
        };
      }

      default:
        return { error: `unknown tool '${name}'` };
    }
  } catch (err) {
    return { error: err.message || String(err) };
  }
}
