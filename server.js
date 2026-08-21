// local launcher. the express app itself lives in lib/app.js so that the
// vercel function entry (api/index.js) can mount the exact same app.
import app from './lib/app.js';
import { detectMode } from './lib/tools.js';
import { oauthConfigured, repoFullName, repoBranch } from './lib/auth.js';

// coerce to a number: process.env.PORT is a string, and the retry below would
// otherwise concatenate instead of increment ('3000' + 1 === '30001')
const PORT = Number(process.env.PORT) || 3000;

function startServer(port) {
  const server = app.listen(port, () => {
    console.log(`\n========================================`);
    console.log(`vanish harness running on http://localhost:${port}`);
    console.log(`model: stealth/ox-alpha`);
    console.log(`mode: ${detectMode()}`);
    console.log(`repo: https://github.com/${repoFullName()} (${repoBranch()})`);
    console.log(`github oauth: ${oauthConfigured() ? 'configured' : 'not configured (local dev is open)'}`);
    console.log(`========================================\n`);
  });

  server.on('error', (err) => {
    if (err.code === 'EADDRINUSE') {
      console.log(`port ${port} is in use, trying port ${port + 1}...`);
      startServer(port + 1);
    } else {
      console.error('server error:', err);
    }
  });
}

startServer(PORT);
