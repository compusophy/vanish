import { getToolDefinitions, executeTool, createToolContext, detectMode } from './tools.js';
import { getGitStatus, getGitDiff } from './git-service.js';

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

guidelines for self-editing & autonomous coding:
- always read relevant files first before making modifications.
- keep changes clean, robust, and well-structured.
- after editing, verify your changes by checking git_diff or running tests/commands.
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

how the deploy loop works here:
- write_file and edit_file only stage changes in memory for this run.
- git_commit flushes all staged files to github in a single commit.
- the repository has continuous deployment wired to vercel, so that commit
  automatically rebuilds and redeploys this harness. committing IS deploying.
- there is no run_command in cloud mode, so you cannot run tests or builds.
  compensate by reading carefully and re-checking your work with git_diff
  before you commit.

guidelines for self-editing & autonomous coding:
- always read relevant files first before making modifications.
- keep changes clean, robust, and well-structured.
- you are editing the code that is running you. a broken commit takes the
  deployment down, so review git_diff before every git_commit.
- explain your thought process clearly in step-by-step reasoning.
- when your task is complete, summarize the changes you made.`;
}

// context compaction: once real chat history accumulates across turns,
// giant tool outputs would eat the whole context window. cap each one.
const MAX_TOOL_CHARS = 8000;

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
  maxSteps = 8,
  keepGoing = false,
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
  const startTime = Date.now();

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

  while (step < maxSteps) {
    if (signal?.aborted) {
      onEvent({ type: 'agent_stopped', reason: 'aborted by user', step });
      break;
    }

    step++;
    onEvent({ type: 'step_start', step, maxSteps });

    let stepReasoning = '';
    let stepContent = '';
    const toolCallsMap = new Map(); // index -> { id, name, argsStr }

    try {
      const response = await fetch('https://openrouter.ai/api/v1/chat/completions', {
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

      if (!response.ok) {
        const errText = await response.text();
        throw new Error(`openrouter error (${response.status}): ${errText}`);
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
      // with keepGoing enabled, refuse the early exit while budget remains:
      // nudge it to keep verifying/polishing until the step count is spent.
      if (toolCalls.length === 0) {
        onEvent({
          type: 'step_end',
          step,
          hasToolCalls: false,
          content: stepContent,
          reasoning: stepReasoning
        });

        if (keepGoing && step < maxSteps && !signal?.aborted) {
          onEvent({ type: 'continue_nudge', step, remainingSteps: maxSteps - step });
          conversationMessages.push({
            role: 'user',
            content:
              `keep going. you have ${maxSteps - step} steps left in your budget — ` +
              'do not stop yet. use the remaining steps productively: verify your changes ' +
              '(read files back, check git_diff), run further self-checks, refine or harden ' +
              'what you built, then continue working until the full step budget is used. ' +
              'only produce a final summary once no steps remain.'
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
      }

      onEvent({
        type: 'step_end',
        step,
        hasToolCalls: true,
        content: stepContent,
        reasoning: stepReasoning
      });

    } catch (stepErr) {
      if (signal?.aborted) break;
      onEvent({ type: 'step_error', step, error: stepErr.message });
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
