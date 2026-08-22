import { getToolDefinitions, executeTool, createToolContext, detectMode } from './tools.js';
import { getGitStatus, getGitDiff } from './git-service.js';
import {
  getDeploymentSignal,
  waitForDeploymentSignal,
  deploymentSource,
  isSettled
} from './deploy-feedback.js';

const LOCAL_SYSTEM_PROMPT = `you are vanish, an elite autonomous self-editing and self-improving coding agent harness powered by stealth/ox-alpha.
you have direct, real-time control over your own source code, workspace files, local terminal, git repository, and vercel deployments.

your tool capabilities:
1. read_file: inspect file contents with line numbers.
2. write_file: create new files or overwrite existing files.
3. edit_file: perform exact substring replacements.
4. list_dir: explore files and directories.
5. run_command: execute terminal commands (tests, builds, node scripts, git).
6. git_status: check working tree modifications.
7. git_diff: inspect exact diffs of code changes.
8. git_commit: stage and commit changes.
9. git_push: push commits to origin main on github.
10. deploy_vercel: deploy the harness to vercel.
11. check_deployment: read the real result of a vercel build — state, error,
    and the build log tail when a vercel token is configured.

guidelines for self-editing & autonomous coding:
- always read relevant files first before making modifications.
- keep changes clean, robust, and well-structured.
- after editing, verify your changes by checking git_diff or running tests/commands.
- git_commit refuses to run if any changed .js or .json file fails to parse.
  when it reports syntax_errors, fix the named file before retrying — do not
  try to work around the gate.
- a commit is only done when it is deployed. after pushing, use
  check_deployment to confirm the build went green.
- explain your thought process clearly in step-by-step reasoning.
- when your task is complete, summarize the changes you made.`;

function buildCloudSystemPrompt(ctx) {
  return `you are vanish, an elite autonomous self-editing and self-improving coding agent harness powered by stealth/ox-alpha.

you are currently running in cloud mode: deployed as a vercel function, editing your own source repository \`${ctx.repo}\` on branch \`${ctx.branch}\` through the github api. there is no writable filesystem and no shell here.

your tool capabilities:
1. read_file: inspect file contents from github with line numbers.
2. write_file: create or overwrite a file (staged in memory, not yet on github).
3. edit_file: perform exact substring replacements (also staged).
4. list_dir: explore the repository tree.
5. git_status: list the files you have staged this run.
6. git_diff: inspect the exact diff of your staged changes.
7. git_commit: write every staged file to github as one atomic commit.
8. git_push: reports push state (git_commit already writes to origin).
9. deploy_vercel: reports deployment status.
10. check_deployment: read the real result of the vercel build for a commit.
    this is your only window onto what happens to your code after it leaves
    this process. it always reports whether the build passed or failed; the
    full build log is included when a vercel token is configured.

how the deploy loop works here:
- write_file and edit_file only stage changes in memory for this run.
- git_commit flushes all staged files to github in a single commit.
- the repository has continuous deployment wired to vercel, so that commit
  automatically rebuilds and redeploys this harness. committing IS deploying.
- git_commit parse-checks every staged .js and .json file and REFUSES to
  commit if any of them is broken. if it returns syntax_errors, read the
  named file around the reported line, fix the real cause, and commit again.
- after a successful commit the build result is reported back to you
  automatically. if it says the deployment failed, fixing it is now the most
  important thing in the run — the live app is down until you do.
- there is no run_command in cloud mode, so you cannot run tests or builds.
  compensate by reading carefully and re-checking your work with git_diff
  before you commit.

guidelines for self-editing & autonomous coding:
- always read relevant files first before making modifications.
- keep changes clean, robust, and well-structured.
- you are editing the code that is running you. a broken commit takes the
  deployment down, so review git_diff before every git_commit.
- never declare work finished on the strength of a commit alone. finished
  means check_deployment reports state READY for that commit.
- explain your thought process clearly in step-by-step reasoning.
- when your task is complete, summarize the changes you made.`;
}

// context compaction: once real chat history accumulates across turns,
// giant tool outputs would eat the whole context window. cap each one.
const MAX_TOOL_CHARS = 8000;

// transient http statuses worth retrying (rate limits, provider hiccups).
// one flaky openrouter response must never kill an entire run.
const RETRYABLE_STATUS = new Set([408, 409, 425, 429, 500, 502, 503, 504]);
const LLM_ATTEMPTS = 3;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

/**
 * death log: persist a structured post-mortem of a fatal failure to
 * memory/deaths.md and COMMIT it immediately. staged work dies with the
 * run; committed markdown survives it, so the next run (and the human)
 * can see exactly what killed this one. best-effort only — a failing
 * logger must never mask the original error.
 */
async function logDeath(ctx, { reason, detail, step, maxSteps, model }) {
  try {
    if (ctx.mode !== 'github') return null;
    let previous = '';
    try {
      const raw = await executeTool('read_file', { path: 'memory/deaths.md' }, ctx);
      const parsed = typeof raw === 'string' ? JSON.parse(raw) : raw;
      if (parsed && typeof parsed.content === 'string') previous = parsed.content;
    } catch (e) {
      /* first death — file does not exist yet */
    }

    const entry =
      `\n---\n## death — ${new Date().toISOString()}\n` +
      `- reason: ${reason}\n` +
      `- step: ${step}/${maxSteps}\n` +
      `- model: ${model}\n` +
      `- error: ${String(detail).slice(0, 1200).replace(/\r/g, '')}\n`;

    await executeTool(
      'write_file',
      { path: 'memory/deaths.md', content: (previous + entry).slice(-48000) },
      ctx
    );
    await executeTool('git_commit', { message: `death log: ${reason} at step ${step}` }, ctx);
    return true;
  } catch (e) {
    return null; // logger failure is non-fatal by design
  }
}

// ---- deployment feedback ---------------------------------------------
//
// this harness deploys itself. before vercel credentials existed, that made
// the build a one-way door: the agent committed, vercel failed, and nothing
// about the failure ever came back — so it kept stacking commits on top of a
// dead deployment. these two helpers close the loop.

const DEPLOY_LOG_LINES = 30;

function formatDeployFailure(report) {
  const parts = [`state: ${report.state}`];
  if (report.sha) parts.push(`commit: ${report.sha}`);
  if (report.commit_message) parts.push(`commit message: ${report.commit_message}`);
  if (report.error_step) parts.push(`failed at: ${report.error_step}`);
  if (report.error_message) parts.push(`vercel error: ${report.error_message}`);
  if (report.build_log_tail) parts.push(`build log tail:\n${report.build_log_tail}`);
  return parts.join('\n');
}

// called once per run, before the first llm call
async function injectDeploymentHealth({ ctx, conversationMessages, onEvent }) {
  if (deploymentSource(ctx) === 'none') return null;

  try {
    const report = await getDeploymentSignal({ ctx, logLines: DEPLOY_LOG_LINES });
    onEvent({ type: 'deployment_state', report });

    if (!report.found || report.succeeded) return report;
    if (!isSettled(report)) return report; // still building

    conversationMessages.push({
      role: 'user',
      content:
        'DEPLOYMENT IS RED. the live vercel build at the head of this branch failed, ' +
        'which means the deployed harness is stale or down. fixing this takes priority ' +
        'over everything else in this run: read the error below, find the cause in the ' +
        'repo, fix it, and commit.\n\n' +
        formatDeployFailure(report)
    });
    return report;
  } catch (err) {
    onEvent({ type: 'deployment_check_failed', error: err.message });
    return null;
  }
}

// called immediately after a successful git_commit, if the run has time
async function verifyCommitDeployment({ ctx, commit, conversationMessages, onEvent, msLeft }) {
  if (deploymentSource(ctx) === 'none' || !commit?.success || !commit?.sha) return null;

  // never eat the wrap-up budget: a missed check is recoverable on the next
  // run, a hard kill mid-write is not.
  const budget = Math.min(35000, msLeft - 12000);
  if (budget < 8000) {
    onEvent({
      type: 'deployment_check_skipped',
      sha: commit.short_sha || null,
      reason: 'not enough time left in this run — check_deployment on the next run will report it'
    });
    return null;
  }

  onEvent({ type: 'deployment_watch', sha: commit.short_sha || null, budgetMs: budget });

  try {
    const report = await waitForDeploymentSignal({
      ctx,
      sha: commit.sha,
      timeoutMs: budget,
      pollMs: 3000
    });
    onEvent({ type: 'deployment_result', sha: commit.short_sha || null, report });

    if (report.timed_out) {
      conversationMessages.push({
        role: 'user',
        content: `deployment check: the build for ${commit.short_sha} was still running after ${Math.round(budget / 1000)}s. call check_deployment before you finish to confirm it went green.`
      });
    } else if (report.succeeded) {
      conversationMessages.push({
        role: 'user',
        content: `deployment check: commit ${report.sha} built and deployed successfully${report.url ? ` (${report.url})` : ''}.`
      });
    } else {
      conversationMessages.push({
        role: 'user',
        content:
          `DEPLOYMENT FAILED for the commit you just made (${report.sha}). the live app is now broken. ` +
          'diagnose it from the build output below and push a fix in this run.\n\n' +
          formatDeployFailure(report)
      });
    }
    return report;
  } catch (err) {
    onEvent({ type: 'deployment_check_failed', error: err.message });
    return null;
  }
}

function compactContent(value) {
  const str = typeof value === 'string' ? value : JSON.stringify(value);
  if (str.length <= MAX_TOOL_CHARS) return str;
  return `${str.slice(0, MAX_TOOL_CHARS)}\n...[truncated ${str.length - MAX_TOOL_CHARS} chars]`;
}

export async function runAgentLoop({
  prompt,
  history = [],
  apiKey,
  model = 'stealth/ox-alpha',
  reasoningEffort = 'high',
  maxSteps = 20,
  keepGoing = false,
  loopMode = false,
  signal,
  onEvent,
  toolContext
}) {
  const ctx = toolContext || createToolContext({ mode: detectMode() });
  const systemPrompt = ctx.mode === 'github'
    ? buildCloudSystemPrompt(ctx)
    : LOCAL_SYSTEM_PROMPT;

  const conversationMessages = [
    { role: 'system', content: systemPrompt },
    ...history
  ];

  if (prompt) {
    conversationMessages.push({ role: 'user', content: prompt });
  }

  let step = 0;
  let nudgeCount = 0;
  let wrapUpInjected = false;
  const startTime = Date.now();

  // serverless reality: hobby-tier functions are hard-killed at ~60s
  // regardless of vercel.json maxDuration. dying silently loses all staged
  // work. instead, watch the clock and order a graceful wrap-up before the
  // wall — commit early, summarize, survive.
  const RUN_SOFT_DEADLINE_MS = 42000;
  const RUN_HARD_DEADLINE_MS = 52000;

  onEvent({
    type: 'agent_start',
    prompt,
    model,
    maxSteps,
    mode: ctx.mode,
    repo: ctx.repo || null,
    branch: ctx.branch || null,
    timestamp: new Date().toISOString()
  });

  // the missing half of the feedback loop: start every run knowing whether
  // the live deployment is green. previously a broken commit stayed broken
  // for hours because each new run began with no idea production was red.
  await injectDeploymentHealth({ ctx, conversationMessages, onEvent });

  while (step < maxSteps) {
    if (signal?.aborted) {
      onEvent({ type: 'agent_stopped', reason: 'aborted by user', step });
      break;
    }

    step++;
    onEvent({ type: 'step_start', step, maxSteps });
    try {
  
      const elapsed = Date.now() - startTime;
  
      // hard wall: too late to start another llm round-trip (~10-25s each).
      // stop cleanly now so agent_complete + context handoff still fire.
      if (elapsed > RUN_HARD_DEADLINE_MS) {
        onEvent({
          type: 'time_budget_exhausted',
          step,
          elapsedMs: elapsed,
          note: 'serverless time wall — stopping gracefully to preserve work'
        });
        break;
      }
  
      // soft wall: inject a wrap-up directive once, ~one llm call before the end
      if (!wrapUpInjected && elapsed > RUN_SOFT_DEADLINE_MS) {
        wrapUpInjected = true;
        onEvent({ type: 'time_wrapup', step, elapsedMs: elapsed });
        conversationMessages.push({
          role: 'user',
          content:
            'TIME BUDGET WARNING: this run is about to hit the serverless execution ' +
            'wall. do not start any new multi-step work. immediately: (1) git_commit ' +
            'all staged changes with a clear message so nothing is lost, then ' +
            '(2) write a concise summary of what you completed and what remains. ' +
            'the next run will continue from memory/taskboard.md.'
        });
      }
  
      let stepReasoning = '';
      let stepContent = '';
      const toolCallsMap = new Map(); // index -> { id, name, argsStr }
  
      // llm call with retry: transient 429/5xx failures no longer kill the
      // whole run — back off and try again before declaring death.
      let response = null;
      let lastHttpError = null;
      for (let attempt = 1; attempt <= LLM_ATTEMPTS; attempt++) {
        if (signal?.aborted) break;
        try {
          response = await fetch('https://openrouter.ai/api/v1/chat/completions', {
            method: 'POST',
            headers: {
              'Authorization': `Bearer ${apiKey}`,
              'Content-Type': 'application/json',
              'HTTP-Referer': 'http://localhost:3001',
              'X-Title': 'vanish autonomous harness'
            },
            body: JSON.stringify({
              model: model || 'stealth/ox-alpha',
              messages: conversationMessages,
              stream: true,
              tools: getToolDefinitions(ctx.mode),
              tool_choice: 'auto',
              reasoning: reasoningEffort === 'none' ? undefined : { effort: reasoningEffort }
            }),
            signal
          });
  
          if (response.ok) {
            lastHttpError = null;
            break;
          }
  
          const errText = await response.text().catch(() => '');
          lastHttpError = new Error(`openrouter error (${response.status}): ${errText.slice(0, 400)}`);
          const retryable = RETRYABLE_STATUS.has(response.status);
          response = null;
  
          if (!retryable) break; // non-retryable — fail fast
  
          if (attempt < LLM_ATTEMPTS) {
            onEvent({
              type: 'step_retry',
              step,
              attempt,
              maxAttempts: LLM_ATTEMPTS,
              error: lastHttpError.message.slice(0, 200)
            });
            await sleep(1200 * attempt);
          }
        } catch (fetchErr) {
          if (fetchErr.name === 'AbortError') throw fetchErr;
          lastHttpError = fetchErr;
          if (attempt < LLM_ATTEMPTS) {
            onEvent({
              type: 'step_retry',
              step,
              attempt,
              maxAttempts: LLM_ATTEMPTS,
              error: String(fetchErr.message || '').slice(0, 200)
            });
            await sleep(1200 * attempt);
          }
        }
      }
  
      if (!response) {
        throw lastHttpError || new Error('llm call failed after retries');
      }
  
        const reader = response.body.getReader();
        const decoder = new TextDecoder('utf-8');
        let buffer = '';
  
        while (true) {
          if (signal?.aborted) break;
          const { done, value } = await reader.read();
          if (done) break;
  
          buffer += decoder.decode(value, { stream: true });
          const lines = buffer.split('\n');
          buffer = lines.pop() || '';
  
          for (const line of lines) {
            const trimmed = line.trim();
            if (!trimmed || !trimmed.startsWith('data: ')) continue;
            const raw = trimmed.slice(6);
            if (raw === '[DONE]') continue;
  
            try {
              const parsed = JSON.parse(raw);
              const delta = parsed.choices?.[0]?.delta;
              if (!delta) continue;
  
              // reasoning delta
              const reasoningChunk = delta.reasoning || delta.reasoning_content || delta.reasoning_details;
              if (reasoningChunk) {
                const text = typeof reasoningChunk === 'string' ? reasoningChunk : JSON.stringify(reasoningChunk);
                stepReasoning += text;
                onEvent({ type: 'reasoning_chunk', step, text });
              }
  
              // content delta
              if (delta.content) {
                stepContent += delta.content;
                onEvent({ type: 'content_chunk', step, text: delta.content });
              }
  
              // tool calls delta
              if (delta.tool_calls) {
                for (const tc of delta.tool_calls) {
                  const idx = tc.index ?? 0;
                  if (!toolCallsMap.has(idx)) {
                    toolCallsMap.set(idx, {
                      id: tc.id || `call_${Date.now()}_${idx}`,
                      name: tc.function?.name || '',
                      argsStr: tc.function?.arguments || ''
                    });
                  } else {
                    const existing = toolCallsMap.get(idx);
                    if (tc.id) existing.id = tc.id;
                    if (tc.function?.name) existing.name += tc.function.name;
                    if (tc.function?.arguments) existing.argsStr += tc.function.arguments;
                  }
                }
              }
            } catch (e) {
              // ignore parse error on partial lines
            }
          }
        }
  
        const toolCalls = Array.from(toolCallsMap.values()).map(tc => {
          let parsedArgs = {};
          try {
            parsedArgs = tc.argsStr ? JSON.parse(tc.argsStr) : {};
          } catch (e) {
            parsedArgs = { raw: tc.argsStr };
          }
          return {
            id: tc.id,
            type: 'function',
            function: {
              name: tc.name,
              arguments: tc.argsStr
            },
            parsedArgs
          };
        });
  
        // append assistant response to context
        const assistantMsg = {
          role: 'assistant',
          content: stepContent || null
        };
  
        if (toolCalls.length > 0) {
          assistantMsg.tool_calls = toolCalls.map(tc => ({
            id: tc.id,
            type: 'function',
            function: {
              name: tc.function.name,
              arguments: tc.function.arguments
            }
          }));
        }
  
        conversationMessages.push(assistantMsg);
  
        // if no tool calls were requested, the agent believes it is done.
        //
        // loopMode (d3): never accept a self-termination. the loop runs until
        // human intervention (abort). every early finish is refused with an
        // instruction to pick the next most valuable action.
        //
        // keepGoing (d2): steps are a budget, not a quota. allow at most
        // MAX_NUDGES verification nudges, then let it finish dynamically —
        // simple tasks end in few steps, hard ones use more of the budget.
        if (toolCalls.length === 0) {
          onEvent({
            type: 'step_end',
            step,
            hasToolCalls: false,
            content: stepContent,
            reasoning: stepReasoning
          });
  
          if (signal?.aborted) break;
  
          if (loopMode) {
            onEvent({ type: 'continue_nudge', step, loopMode: true });
            conversationMessages.push({
              role: 'user',
              content:
                'loop mode active: you do not stop until human intervention. ' +
                'do not produce a final summary and do not idle. instead pick the ' +
                'next most valuable action for this task and execute it with tools — ' +
                'e.g. deeper verification (read files back, git_diff), refactoring, ' +
                'hardening edge cases, improving docs, or advancing the next item on ' +
                'memory/taskboard.md. continue working now.'
            });
            continue;
          }
  
          const MAX_NUDGES = 2;
          if (keepGoing && nudgeCount < MAX_NUDGES) {
            nudgeCount++;
            const remaining = Math.max(maxSteps - step, 1);
            onEvent({
              type: 'continue_nudge',
              step,
              remainingSteps: remaining,
              nudgeCount,
              maxNudges: MAX_NUDGES
            });
            conversationMessages.push({
              role: 'user',
              content:
                nudgeCount === 1
                  ? `before finishing, verify your work while budget remains (~${remaining} steps left): ` +
                    'read edited files back, check git_diff for correctness, and fix anything off. ' +
                    'if verification passes cleanly you may finish; otherwise keep working.'
                  : 'final chance to catch issues: one more quick pass over your changes ' +
                    '(git_diff, re-read critical files). if everything checks out, give your ' +
                    'final summary now.'
            });
            continue;
          }
  
          break;
        }
  
        // execute tool calls
        onEvent({
          type: 'step_tool_calls',
          step,
          toolCalls: toolCalls.map(tc => ({ id: tc.id, name: tc.function.name, args: tc.parsedArgs }))
        });
  
        for (const tc of toolCalls) {
          if (signal?.aborted) break;
  
          const toolStartTime = Date.now();
          onEvent({
            type: 'tool_exec_start',
            step,
            toolId: tc.id,
            name: tc.function.name,
            args: tc.parsedArgs
          });
  
          const result = await executeTool(tc.function.name, tc.parsedArgs, ctx);
          const duration = Date.now() - toolStartTime;
  
          onEvent({
            type: 'tool_exec_result',
            step,
            toolId: tc.id,
            name: tc.function.name,
            result,
            duration
          });
  
          conversationMessages.push({
            role: 'tool',
            tool_call_id: tc.id,
            name: tc.function.name,
            content: compactContent(result)
          });

          // a commit is a deploy here. watch the build it just triggered so
          // the failure lands in this conversation instead of in a dashboard
          // nobody is reading.
          if (tc.function.name === 'git_commit' && result?.success) {
            await verifyCommitDeployment({
              ctx,
              commit: result,
              conversationMessages,
              onEvent,
              msLeft: RUN_HARD_DEADLINE_MS - (Date.now() - startTime)
            });
          }
        }
  
        onEvent({
          type: 'step_end',
          step,
          hasToolCalls: true,
          content: stepContent,
          reasoning: stepReasoning
        });
  
    } catch (stepErr) {
      if (signal?.aborted) {
        onEvent({ type: 'agent_stopped', reason: 'aborted by user', step });
        break;
      }

      // fatal: surface it loudly to the client, then persist a post-mortem
      // so the next run (and the human) can see exactly what killed this one.
      onEvent({ type: 'step_error', step, error: stepErr.message });
      onEvent({
        type: 'agent_died',
        step,
        maxSteps,
        reason: String(stepErr.message || '').slice(0, 500),
        timestamp: new Date().toISOString()
      });
      await logDeath(ctx, {
        reason: stepErr.name === 'TypeError' ? 'runtime crash' : 'llm call failed',
        detail: `${stepErr.name}: ${stepErr.message}`,
        step,
        maxSteps,
        model
      });
      break;
    }
  }

  // fetch latest status to report modifications. in cloud mode there is no
  // git binary, so this comes back through the github-backed tool instead.
  const gitStatus = ctx.mode === 'github'
    ? await executeTool('git_status', {}, ctx)
    : await getGitStatus();
  const totalDuration = ((Date.now() - startTime) / 1000).toFixed(1);

  onEvent({
    type: 'agent_complete',
    totalSteps: step,
    duration: `${totalDuration}s`,
    mode: ctx.mode,
    lastCommit: ctx.lastCommit || null,
    gitStatus,
    timestamp: new Date().toISOString()
  });

  // hand the final conversation (minus system prompt) back to the client so
  // it can persist it and send it back as history on the next run. this is
  // what turns the one-shot agent into a real continuous chat session.
  onEvent({
    type: 'agent_context',
    messages: conversationMessages.filter(m => m.role !== 'system')
  });

  return {
    success: true,
    totalSteps: step,
    duration: totalDuration,
    messages: conversationMessages
  };
}
