import * as gh from './github-service.js';
import {
  vercelConfigured,
  vercelConfigStatus,
  deploymentReport,
  waitForDeployment,
  projectName
} from './vercel-service.js';

// one question, two possible answers, in order of how much they tell you:
//
//   vercel rest api  - state + vercel's error + the actual build log.
//                      needs VERCEL_TOKEN.
//   github statuses  - state + the inspector url. free: vercel's github
//                      integration already posts a commit status for every
//                      build, and this harness holds a github token anyway.
//
// the second one is the reason a fresh clone still gets a closed loop. the
// first one is the reason a failure is actionable rather than just visible.

const SETTLED = new Set(['READY', 'ERROR', 'CANCELED']);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

export function deploymentSource(ctx = {}) {
  if (vercelConfigured()) return 'vercel';
  if (ctx.token && ctx.repo) return 'github';
  return 'none';
}

export function isSettled(report) {
  return Boolean(report && SETTLED.has(report.state));
}

function fromGithubState(state) {
  if (state === 'success') return 'READY';
  if (state === 'failure' || state === 'error') return 'ERROR';
  return 'BUILDING';
}

async function githubSignal(ctx, sha) {
  const target = sha || (await gh.getHeadSha(ctx.token, ctx.repo, ctx.branch || 'main'));
  if (!target) return { found: false, note: 'could not resolve a commit to check' };

  const status = await gh.getCommitDeployStatus(ctx.token, ctx.repo, target, {
    project: projectName()
  });
  const failing = status.checks.find((c) => c.state === 'failure' || c.state === 'error');
  const state = fromGithubState(status.state);

  return {
    found: status.found,
    source: 'github commit status',
    state,
    succeeded: state === 'READY',
    sha: String(target).slice(0, 7),
    error_message: failing?.description || null,
    inspector_url: failing?.url || status.checks[0]?.url || null,
    deployment_id: failing?.deployment_id || status.checks[0]?.deployment_id || null,
    project: status.project,
    other_projects_ignored: status.other_projects || 0,
    // the log lives behind vercel's api. say so plainly rather than leaving
    // the model to wonder why a failure has no detail attached.
    build_log_tail: null,
    log_note:
      state === 'ERROR'
        ? 'build log is not readable without VERCEL_TOKEN. diagnose from the diff you just committed, or open the inspector url.'
        : null
  };
}

/**
 * current deployment state for a commit (or the branch head when sha is
 * omitted), using the richest source available.
 */
export async function getDeploymentSignal({ ctx = {}, sha = null, logLines = 40 } = {}) {
  const source = deploymentSource(ctx);

  if (source === 'none') {
    return {
      found: false,
      available: false,
      error: 'no way to read deployment results: neither VERCEL_TOKEN nor a github token is available.',
      ...vercelConfigStatus()
    };
  }

  try {
    if (source === 'vercel') {
      const report = await deploymentReport(sha || null, { logLines });
      return { ...report, source: 'vercel api' };
    }
    return await githubSignal(ctx, sha);
  } catch (err) {
    return { found: false, error: `${source} deployment lookup failed: ${err.message}` };
  }
}

/**
 * bounded wait for a commit's build to settle. the caller owns the time
 * budget — this runs inside a serverless function with a hard wall.
 */
export async function waitForDeploymentSignal({
  ctx = {},
  sha,
  timeoutMs = 45000,
  pollMs = 3000
} = {}) {
  const source = deploymentSource(ctx);
  if (source === 'none') return await getDeploymentSignal({ ctx, sha });

  if (source === 'vercel') {
    const report = await waitForDeployment(sha, { timeoutMs, pollMs });
    return { ...report, source: 'vercel api' };
  }

  const started = Date.now();
  let last = null;
  while (Date.now() - started < timeoutMs) {
    last = await githubSignal(ctx, sha);
    if (last.found && isSettled(last)) return last;
    await sleep(pollMs);
  }

  return {
    ...(last || { found: false }),
    timed_out: true,
    note: `build had not reported back within ${Math.round(timeoutMs / 1000)}s. call check_deployment again to see the result.`
  };
}
