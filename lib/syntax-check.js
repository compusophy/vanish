import { spawn } from 'child_process';
import fs from 'fs/promises';
import os from 'os';
import path from 'path';
import crypto from 'crypto';

// why this exists:
//
// the agent commits straight to github and vercel builds every commit. a
// single unbalanced brace therefore takes production down and — worse — the
// agent never finds out, because the failure happens on a machine it cannot
// see. fifteen consecutive red deploys were caused by one `} catch` written
// without its `try {`.
//
// this is the cheap half of the fix: refuse to commit source that does not
// parse. the expensive half (reading the real build log back) lives in
// vercel-service.js.

const JS_EXT = new Set(['.js', '.mjs', '.cjs']);
const CHECK_TIMEOUT_MS = 10000;

export function isCheckable(relPath) {
  const ext = path.extname(String(relPath || '')).toLowerCase();
  return JS_EXT.has(ext) || ext === '.json';
}

function cleanError(stderr, tmpFile) {
  return String(stderr)
    .split('\n')
    .filter((l) => !/^\s*at\s/.test(l) && !/^Node\.js v/.test(l))
    .join('\n')
    .split(tmpFile)
    .join('')
    .trim()
    .slice(0, 1200);
}

// node --check is the only parser guaranteed to agree with the runtime that
// will actually import the file. spawning it costs ~60ms, which is nothing
// next to a failed deploy.
async function nodeCheck(content, ext) {
  const file = path.join(
    os.tmpdir(),
    `vanish-check-${crypto.randomBytes(8).toString('hex')}${ext}`
  );

  try {
    await fs.writeFile(file, content, 'utf8');
  } catch (err) {
    // read-only tmp would be unusual, but a failing checker must never block
    // a commit on its own account.
    return { ok: true, skipped: true, reason: err.message };
  }

  try {
    return await new Promise((resolve) => {
      let child;
      try {
        child = spawn(process.execPath, ['--check', file], {
          stdio: ['ignore', 'ignore', 'pipe']
        });
      } catch (err) {
        return resolve({ ok: true, skipped: true, reason: err.message });
      }

      let stderr = '';
      const timer = setTimeout(() => child.kill('SIGKILL'), CHECK_TIMEOUT_MS);

      child.stderr.on('data', (d) => {
        stderr += d;
      });
      child.on('error', (err) => {
        clearTimeout(timer);
        resolve({ ok: true, skipped: true, reason: err.message });
      });
      child.on('close', (code) => {
        clearTimeout(timer);
        if (code === 0) return resolve({ ok: true });
        resolve({ ok: false, error: cleanError(stderr, file) });
      });
    });
  } finally {
    fs.unlink(file).catch(() => {});
  }
}

/**
 * parse-check one file. `.js` is ambiguous (esm or cjs depending on the
 * nearest package.json), so it is tried both ways and only reported broken
 * when neither parses.
 */
export async function checkSource(relPath, content) {
  const ext = path.extname(String(relPath || '')).toLowerCase();

  if (ext === '.json') {
    try {
      JSON.parse(content);
      return { path: relPath, ok: true };
    } catch (err) {
      return { path: relPath, ok: false, error: err.message };
    }
  }

  if (!JS_EXT.has(ext)) return { path: relPath, ok: true, skipped: true };
  if (typeof content !== 'string') return { path: relPath, ok: true, skipped: true };

  const asModule = await nodeCheck(content, ext === '.cjs' ? '.cjs' : '.mjs');
  if (asModule.ok) return { path: relPath, ...asModule };

  // browser scripts and commonjs files are classic scripts, not modules
  const asScript = await nodeCheck(content, '.cjs');
  if (asScript.ok) return { path: relPath, ...asScript };

  return { path: relPath, ok: false, error: asModule.error };
}

/**
 * check a batch of files. returns only the broken ones, which is what the
 * caller wants to put in front of the model.
 */
export async function checkSources(files = []) {
  const results = await Promise.all(
    files
      .filter((f) => f && typeof f.content === 'string' && isCheckable(f.path))
      .map((f) => checkSource(f.path, f.content))
  );
  return results.filter((r) => !r.ok);
}

export function describeFailures(failures = []) {
  return failures
    .map((f) => `${f.path}:\n${f.error}`)
    .join('\n\n')
    .slice(0, 4000);
}
