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
├── .env                    # OPENROUTER_API_KEY / API_KEY (local only)
├── .env.example            # every supported environment variable
├── .gitignore              # secrets, node_modules, and cache protection
├── package.json            # dependencies & scripts
├── vercel.json             # serverless routing + 300s function duration
├── server.js               # local listener
├── api/
│   └── index.js            # vercel serverless entry (mounts the same app)
├── lib/
│   ├── app.js              # express app: routes, auth gating, ide endpoints
│   ├── agent.js            # multi-turn autonomous tool execution loop
│   ├── tools.js            # mode-aware tool surface (local fs vs github api)
│   ├── github-service.js   # github rest backend: reads, atomic commits, diffs
│   ├── auth.js             # github oauth flow, allowlist, route gating
│   ├── session.js          # aes-256-gcm sealed stateless session cookie
│   ├── git-service.js      # local git status, diff, commit, and push helpers
│   └── vercel-service.js   # vercel deploy and status helpers
└── public/
    ├── index.html          # vanish harness web interface
    ├── style.css           # dark glassmorphism styling
    ├── auth.js             # sign-in gate and session chip
    └── app.js              # client state, sse stream receiver, and ide logic
```

---

## two execution modes

vanish detects where it is running and swaps its tool backend accordingly.
the tool surface the model sees is identical in both, so prompts transfer.

| | **local** | **cloud** (vercel) |
| --- | --- | --- |
| detection | default | `process.env.VERCEL` |
| reads | filesystem | github contents api |
| writes | filesystem | in-memory staging area |
| `git_commit` | `git` binary | atomic multi-file commit via git data api |
| `git_diff` | `git diff` | lcs line diff over staged changes |
| `run_command` | available | **withheld** |
| auth | open when oauth is unconfigured | always required |

a vercel function has a read-only filesystem, no `git` binary, and no vercel
cli, so the local tools cannot work there. in cloud mode `write_file` and
`edit_file` stage changes in memory for the duration of one agent run, and
`git_commit` flushes them all to github as a single atomic commit.

`run_command` is deliberately not offered in cloud mode. the harness sits on a
public url, and handing a model arbitrary shell execution there would be
remote code execution on the deployment.

### the self-deploy loop

```
agent edits its own source
        ↓  git_commit  (github git data api)
   commit lands on main
        ↓  continuous deployment
    vercel rebuilds
        ↓
 the harness redeploys itself
```

committing *is* deploying.

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

---

## deployment & github oauth

the deployed harness is gated behind github oauth. this does double duty: it
keeps the public url from letting anyone drain the openrouter key, and the
token it returns is what gives the agent write access to the repository.

### 1. create a github oauth app

at **github.com → settings → developer settings → oauth apps → new oauth app**:

| field | value |
| --- | --- |
| application name | `vanish` |
| homepage url | your deployment url |
| authorization callback url | `<deployment url>/api/auth/github/callback` |

then generate a client secret.

### 2. set the environment variables

```bash
vercel env add GITHUB_CLIENT_ID production
vercel env add GITHUB_CLIENT_SECRET production
```

repeat for `preview` if you want sign-in on preview deployments. note that
each deployment url must be registered as a callback url on the oauth app.

### 3. redeploy so the new variables are picked up

```bash
vercel deploy --prod
```

### environment variables

| variable | required | purpose |
| --- | --- | --- |
| `OPENROUTER_API_KEY` | yes | model access (`API_KEY` also accepted) |
| `SESSION_SECRET` | yes (cloud) | seals the session cookie |
| `GITHUB_CLIENT_ID` | yes (cloud) | oauth app id |
| `GITHUB_CLIENT_SECRET` | yes (cloud) | oauth app secret |
| `GITHUB_REPO` | no | repo the agent may edit (default `compusophy/vanish`) |
| `GITHUB_BRANCH` | no | branch it commits to (default `main`) |
| `ALLOWED_GITHUB_LOGINS` | no | comma separated allowlist; defaults to the repo owner |
| `PUBLIC_BASE_URL` | no | pins the oauth callback origin |
| `GITHUB_TOKEN` | no | headless fallback for cron runs with no browser |
| `VERCEL_DEPLOY_HOOK_URL` | no | forces a rebuild without a commit |

rotating `SESSION_SECRET` invalidates every existing session.

### continuous deployment

the vercel project is connected to the github repository, so every push to
`main` deploys to production and every push to another branch gets a preview
url. that is what closes the self-editing loop: when the agent commits, the
harness redeploys itself.
