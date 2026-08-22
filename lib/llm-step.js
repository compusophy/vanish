// single-turn llm relay. this exists because a multi-step agent loop cannot
// survive inside a vercel function: hobby-tier instances are hard-killed at
// ~60s regardless of vercel.json maxDuration, and every death silently
// destroys all staged work mid-edit. the fix is structural: the BROWSER owns
// the loop and calls this relay once per turn. each invocation lives a few
// seconds — nowhere near any platform limit — and all state (conversation,
// staging area, budgets) lives in the client, which has no time limit.
//
// the server here is a dumb pipe: one openrouter round-trip, streamed back
// as sse events, ending with a fully-parsed assistant message.

const RETRYABLE_STATUS = new Set([408, 409, 425, 429, 500, 502, 503, 504]);
const LLM_ATTEMPTS = 3;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

/**
 * run exactly one chat-completions round-trip against openrouter.
 *
 * @param {object} opts
 * @param {string} opts.apiKey        openrouter key
 * @param {Array}  opts.messages      full conversation (system/user/assistant/tool)
 * @param {string} [opts.model]
 * @param {string} [opts.reasoningEffort] high|medium|low|none
 * @param {Array}  [opts.tools]       openrouter tool definitions (may be null)
 * @ {AbortSignal} [opts.signal]
 * @ {(ev)=>void} [opts.onEvent]     receives {type:'reasoning_chunk'|'content_chunk', text}
 * @returns {Promise<{message: object}>} assistant message incl. tool_calls if any
 */
export async function runLLMStep({
  apiKey,
  messages,
  model = 'stealth/ox-alpha',
  reasoningEffort = 'high',
  tools = null,
  signal,
  onEvent = () => {}
}) {
  let response = null;
  let lastHttpError = null;

  for (let attempt = 1; attempt <= LLM_ATTEMPTS; attempt++) {
    if (signal?.aborted) throw Object.assign(new Error('aborted'), { name: 'AbortError' });
    try {
      response = await fetch('https://openrouter.ai/api/v1/chat/completions', {
        method: 'POST',
        headers: {
          'Authorization': `Bearer ${apiKey}`,
          'Content-Type': 'application/json',
          'HTTP-Referer': 'https://vanish.vercel.app',
          'X-Title': 'vanish autonomous harness'
        },
        body: JSON.stringify({
          model,
          messages,
          stream: true,
          ...(tools ? { tools, tool_choice: 'auto' } : {}),
          ...(reasoningEffort === 'none'
            ? {}
            : { reasoning: { effort: reasoningEffort } })
        }),
        signal
      });

      if (response.ok) { lastHttpError = null; break; }

      const errText = await response.text().catch(() => '');
      lastHttpError = new Error(`openrouter error (${response.status}): ${errText.slice(0, 400)}`);
      const retryable = RETRYABLE_STATUS.has(response.status);
      response = null;
      if (!retryable || attempt === LLM_ATTEMPTS) break;
      await sleep(1200 * attempt);
    } catch (fetchErr) {
      if (fetchErr.name === 'AbortError') throw fetchErr;
      lastHttpError = fetchErr;
      response = null;
      if (attempt === LLM_ATTEMPTS) break;
      await sleep(1200 * attempt);
    }
  }

  if (!response) throw lastHttpError || new Error('llm call failed after retries');

  // stream-parse the sse body into a complete assistant message
  const reader = response.body.getReader();
  const decoder = new TextDecoder('utf-8');
  let buffer = '';
  let content = '';
  let reasoning = '';
  const toolCallMap = new Map(); // index -> { id, name, argsStr }

  while (true) {
    if (signal?.aborted) throw Object.assign(new Error('aborted'), { name: 'AbortError' });
    const { done, value } = await reader.read();
    if (done) break;

    buffer += decoder.decode(value, { stream: true });
    const lines = buffer.split('\n');
    buffer = lines.pop() || '';

    for (const line of lines) {
      const trimmed = line.trim();
      if (!trimmed.startsWith('data: ')) continue;
      const raw = trimmed.slice(6);
      if (raw === '[DONE]') continue;

      let parsed;
      try { parsed = JSON.parse(raw); } catch { continue; }

      const delta = parsed.choices?.[0]?.delta;
      if (!delta) continue;

      const reasoningChunk =
        delta.reasoning || delta.reasoning_content || delta.reasoning_details;
      if (reasoningChunk) {
        const text = typeof reasoningChunk === 'string'
          ? reasoningChunk
          : JSON.stringify(reasoningChunk);
        reasoning += text;
        onEvent({ type: 'reasoning_chunk', text });
      }

      if (delta.content) {
        content += delta.content;
        onEvent({ type: 'content_chunk', text: delta.content });
      }

      if (delta.tool_calls) {
        for (const tc of delta.tool_calls) {
          const idx = tc.index ?? 0;
          if (!toolCallMap.has(idx)) {
            toolCallMap.set(idx, {
              id: tc.id || `call_${Date.now()}_${idx}`,
              name: tc.function?.name || '',
              argsStr: tc.function?.arguments || ''
            });
          } else {
            const existing = toolCallMap.get(idx);
            if (tc.id) existing.id = tc.id;
            if (tc.function?.name) existing.name += tc.function.name;
            if (tc.function?.arguments) existing.argsStr += tc.function.arguments;
          }
        }
      }
    }
  }

  const toolCalls = Array.from(toolCallMap.values()).map((tc) => ({
    id: tc.id,
    type: 'function',
    function: { name: tc.name, arguments: tc.argsStr }
  }));

  const message = { role: 'assistant', content: content || null };
  if (toolCalls.length > 0) message.tool_calls = toolCalls;

  return { message, reasoning, usage: null };
}
