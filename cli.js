import dotenv from 'dotenv';
dotenv.config();

const apiKey = process.env.OPENROUTER_API_KEY || process.env.API_KEY;

if (!apiKey) {
  console.error('error: no api key found in .env (expected OPENROUTER_API_KEY or API_KEY)');
  process.exit(1);
}

const prompt = process.argv.slice(2).join(' ') || "how many r's are in the word 'strawberry'? explain step-by-step.";

console.log(`prompt: "${prompt}"`);
console.log('connecting to openrouter with model stealth/ox-alpha...\n');

async function main() {
  const response = await fetch('https://openrouter.ai/api/v1/chat/completions', {
    method: 'POST',
    headers: {
      'Authorization': `Bearer ${apiKey}`,
      'Content-Type': 'application/json',
      'HTTP-Referer': 'http://localhost:3000',
      'X-Title': '0xalpha client'
    },
    body: JSON.stringify({
      model: 'stealth/ox-alpha',
      stream: true,
      messages: [
        { role: 'user', content: prompt }
      ],
      reasoning: {
        effort: 'high'
      }
    })
  });

  if (!response.ok) {
    const errorText = await response.text();
    console.error(`http error ${response.status}: ${errorText}`);
    process.exit(1);
  }

  const reader = response.body.getReader();
  const decoder = new TextDecoder();
  let buffer = '';
  let reasoningStarted = false;
  let answerStarted = false;
  let totalReasoning = '';
  let totalAnswer = '';

  while (true) {
    const { done, value } = await reader.read();
    if (done) break;

    buffer += decoder.decode(value, { stream: true });
    const lines = buffer.split('\n');
    buffer = lines.pop() || '';

    for (const line of lines) {
      const trimmed = line.trim();
      if (!trimmed || !trimmed.startsWith('data: ')) continue;
      const dataStr = trimmed.slice(6);
      if (dataStr === '[DONE]') continue;

      try {
        const parsed = JSON.parse(dataStr);
        const delta = parsed.choices?.[0]?.delta;
        
        // check reasoning
        const reasoningChunk = delta?.reasoning || delta?.reasoning_content || delta?.reasoning_details;
        if (reasoningChunk) {
          if (!reasoningStarted) {
            console.log('--- [thinking / reasoning] ---');
            reasoningStarted = true;
          }
          const text = typeof reasoningChunk === 'string' ? reasoningChunk : JSON.stringify(reasoningChunk);
          process.stdout.write(text);
          totalReasoning += text;
        }

        // check content
        const contentChunk = delta?.content;
        if (contentChunk) {
          if (!answerStarted) {
            if (reasoningStarted) console.log('\n--- [response] ---');
            answerStarted = true;
          }
          process.stdout.write(contentChunk);
          totalAnswer += contentChunk;
        }

        if (parsed.usage) {
          const reasoningTokens = parsed.usage.completionTokensDetails?.reasoningTokens || 
                                  parsed.usage.completion_tokens_details?.reasoning_tokens ||
                                  parsed.usage.reasoning_tokens;
          console.log(`\n\n[usage]: prompt tokens: ${parsed.usage.prompt_tokens}, completion tokens: ${parsed.usage.completion_tokens}, reasoning tokens: ${reasoningTokens || 'n/a'}`);
        }
      } catch (err) {
        // ignore parse error on partial lines
      }
    }
  }
}

main().catch(err => {
  console.error('fatal error:', err);
});
