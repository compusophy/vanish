import { exec } from 'child_process';
import util from 'util';

const execPromise = util.promisify(exec);
const WORKSPACE_ROOT = process.cwd();

export async function getGitStatus() {
  try {
    const { stdout: branchOut } = await execPromise('git branch --show-current', { cwd: WORKSPACE_ROOT });
    const { stdout: statusOut } = await execPromise('git status --short', { cwd: WORKSPACE_ROOT });
    const { stdout: logOut } = await execPromise('git log -n 5 --oneline', { cwd: WORKSPACE_ROOT }).catch(() => ({ stdout: '' }));

    const lines = statusOut.trim().split('\n').filter(Boolean);
    const files = lines.map(line => {
      const code = line.slice(0, 2).trim();
      const name = line.slice(3).trim();
      return { code, name };
    });

    return {
      branch: branchOut.trim() || 'main',
      clean: files.length === 0,
      modified_files: files,
      recent_commits: logOut.trim().split('\n').filter(Boolean)
    };
  } catch (err) {
    return { error: err.message };
  }
}

export async function getGitDiff(staged = false) {
  try {
    const flag = staged ? '--staged' : '';
    const { stdout } = await execPromise(`git diff ${flag}`, { cwd: WORKSPACE_ROOT });
    return { diff: stdout };
  } catch (err) {
    return { error: err.message, diff: '' };
  }
}

export async function gitCommit(message) {
  try {
    await execPromise('git add -A', { cwd: WORKSPACE_ROOT });
    const safeMsg = (message || 'update').replace(/"/g, '\\"');
    const { stdout } = await execPromise(`git commit -m "${safeMsg}"`, { cwd: WORKSPACE_ROOT });
    return { success: true, output: stdout.trim() };
  } catch (err) {
    return { success: false, error: err.message };
  }
}

export async function gitPush(branch = 'main') {
  try {
    const { stdout, stderr } = await execPromise(`git push origin ${branch}`, { cwd: WORKSPACE_ROOT });
    return { success: true, output: stdout || stderr || 'pushed' };
  } catch (err) {
    return { success: false, error: err.message };
  }
}
