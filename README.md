# vanish

> **vanish** is an autonomous, self-editing, and self-improving coding agent harness powered by `stealth/ox-alpha` via openrouter.

---

## core features

- **autonomous self-editing loop**: `ox-alpha` can inspect, modify, test, and evolve its own source code in multi-turn reasoning loops.
- **integrated tool engine**:
  - `read_file`: view workspace files with line indexing
  - `write_file`: create or overwrite files
  - `edit_file`: precise contextual substring replacements
  - `list_dir`: workspace exploration
  - `run_command`: execute local terminal commands, test runners, linters
  - `git_status` & `git_diff`: real-time working tree change inspection
  - `git_commit` & `git_push`: instant synchronizations to github
  - `deploy_vercel`: automated vercel deployments
- **web ide & live harness monitor**:
  - live thought process inspection drawer with elapsed duration
  - real-time tool execution tracking
  - in-browser code editor with save / reload capabilities
  - interactive git diff viewer with colorized additions & deletions
  - 100% strict lowercase ui aesthetic
- **github repository & vercel sync**:
  - connected to `compusophy/vanish` on github
  - vercel continuous deployment integration

---

## architecture

```
vanish/
├── .env                  # OPENROUTER_API_KEY / API_KEY
├── .gitignore            # secrets, node_modules, and cache protection
├── package.json          # dependencies & scripts
├── server.js             # agent loop engine, tool api, and file endpoints
├── lib/
│   ├── agent.js          # multi-turn autonomous tool execution loop
│   ├── tools.js          # secure workspace tools & execution handlers
│   ├── git-service.js    # git status, diff, commit, and push helpers
│   └── vercel-service.js # vercel deploy and status helpers
└── public/
    ├── index.html        # vanish harness web interface
    ├── style.css         # dark glassmorphism styling
    └── app.js            # client state, sse stream receiver, and ide logic
```

---

## quick start

### 1. configure credentials
set your openrouter api key in `.env`:
```env
OPENROUTER_API_KEY=sk-or-v1-...
```

### 2. install dependencies & run server
```bash
npm install
npm start
```
open `http://localhost:3000` (or `http://localhost:3001`).

### 3. run via terminal cli
```bash
npm run cli -- "your prompt here"
```

### 4. deploy to vercel
```bash
npm run deploy
```
