import { exec } from 'child_process';
import util from 'util';

const execPromise = util.promisify(exec);
const WORKSPACE_ROOT = process.cwd();

export async function checkVercelStatus() {
  try {
    const { stdout } = await execPromise('vercel whoami', { cwd: WORKSPACE_ROOT });
    return {
      logged_in: true,
      user: stdout.trim().split('\n')[0]
    };
  } catch (err) {
    return {
      logged_in: false,
      error: err.message
    };
  }
}

export async function deployToVercel(prod = false) {
  try {
    const flag = prod ? '--prod' : '';
    const { stdout, stderr } = await execPromise(`vercel ${flag} --yes`, {
      cwd: WORKSPACE_ROOT,
      timeout: 180000
    });

    const output = (stdout + '\n' + stderr).trim();
    // try to extract deployment url (https://*.vercel.app)
    const urlMatch = output.match(/https:\/\/[a-zA-Z0-9-]+\.vercel\.app/);

    return {
      success: true,
      url: urlMatch ? urlMatch[0] : null,
      output
    };
  } catch (err) {
    return {
      success: false,
      error: err.message,
      output: err.stdout || err.stderr || ''
    };
  }
}
