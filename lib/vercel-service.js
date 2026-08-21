import { exec } from 'child_process';
import util from 'util';

const execPromise = util.promisify(exec);
const WORKSPACE_ROOT = process.cwd();

function isCloud() {
  return Boolean(process.env.VERCEL);
}

export async function checkVercelStatus() {
  // running on vercel there is no cli to shell out to, but the platform
  // injects everything worth reporting as environment variables.
  if (isCloud()) {
    return {
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
    };
  }

  try {
    const { stdout } = await execPromise('vercel whoami', { cwd: WORKSPACE_ROOT });
    return {
      logged_in: true,
      running_on_vercel: false,
      user: stdout.trim().split('\n').pop()
    };
  } catch (err) {
    return {
      logged_in: false,
      running_on_vercel: false,
      error: err.message
    };
  }
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
