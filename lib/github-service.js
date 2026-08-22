// github rest api backend. this is what lets vanish edit its own source
// when it is running on vercel, where there is no writable filesystem,
// no git binary, and no .git directory.

const GITHUB_API = 'https://api.github.com';

function authHeaders(token) {
  return {
    Authorization: `Bearer ${token}`,
    Accept: 'application/vnd.github+json',
    'X-GitHub-Api-Version': '2022-11-28',
    'User-Agent': 'vanish-harness'
  };
}

async function gh(token, path, options = {}) {
  const res = await fetch(`${GITHUB_API}${path}`, {
    ...options,
    headers: { ...authHeaders(token), ...(options.headers || {}) }
  });

  const text = await res.text();
  let body = null;
  try {
    body = text ? JSON.parse(text) : null;
  } catch {
    body = text;
  }

  if (!res.ok) {
    const message = body?.message || text || `http ${res.status}`;
    const err = new Error(`github api ${res.status}: ${message}`);
    err.status = res.status;
    err.body = body;
    throw err;
  }
  return body;
}

function encodePath(p) {
  return String(p)
    .replace(/^\.\/?/, '')
    .replace(/^\/+/, '')
    .split('/')
    .map(encodeURIComponent)
    .join('/');
}

export function parseRepo(fullName) {
  const [owner, repo] = String(fullName || '').split('/');
  if (!owner || !repo) {
    throw new Error(`invalid repo '${fullName}', expected 'owner/name'`);
  }
  return { owner, repo };
}

export async function getViewer(token) {
  return gh(token, '/user');
}

export async function getRepo(token, fullName) {
  const { owner, repo } = parseRepo(fullName);
  return gh(token, `/repos/${owner}/${repo}`);
}

// ---- reads -----------------------------------------------------------

export async function readFile(token, fullName, filePath, ref) {
  const { owner, repo } = parseRepo(fullName);
  const query = ref ? `?ref=${encodeURIComponent(ref)}` : '';
  const data = await gh(
    token,
    `/repos/${owner}/${repo}/contents/${encodePath(filePath)}${query}`
  );

  if (Array.isArray(data)) {
    throw new Error(`'${filePath}' is a directory, not a file`);
  }
  if (data.encoding !== 'base64' || typeof data.content !== 'string') {
    throw new Error(`'${filePath}' is not a readable text blob`);
  }
  return {
    content: Buffer.from(data.content, 'base64').toString('utf8'),
    sha: data.sha,
    size: data.size
  };
}

export async function listDir(token, fullName, dirPath = '', ref) {
  const { owner, repo } = parseRepo(fullName);
  const clean = (dirPath || '').replace(/^\.\/?/, '').replace(/^\/+|\/+$/g, '');
  const query = ref ? `?ref=${encodeURIComponent(ref)}` : '';
  const data = await gh(
    token,
    `/repos/${owner}/${repo}/contents/${clean ? encodePath(clean) : ''}${query}`
  );

  const items = Array.isArray(data) ? data : [data];
  return items.map((item) => ({
    name: item.name,
    path: item.path,
    type: item.type === 'dir' ? 'directory' : 'file',
    size: item.type === 'dir' ? undefined : item.size
  }));
}

// full recursive tree in a single request
export async function getTree(token, fullName, ref = 'HEAD') {
  const { owner, repo } = parseRepo(fullName);
  const data = await gh(
    token,
    `/repos/${owner}/${repo}/git/trees/${encodeURIComponent(ref)}?recursive=1`
  );
  return (data.tree || [])
    .filter((n) => !n.path.startsWith('node_modules/'))
    .map((n) => ({
      path: n.path,
      type: n.type === 'tree' ? 'directory' : 'file',
      size: n.size
    }));
}

export async function listCommits(token, fullName, branch, perPage = 10) {
  const { owner, repo } = parseRepo(fullName);
  const data = await gh(
    token,
    `/repos/${owner}/${repo}/commits?sha=${encodeURIComponent(branch)}&per_page=${perPage}`
  );
  return data.map((c) => ({
    sha: c.sha.slice(0, 7),
    message: c.commit.message.split('\n')[0],
    author: c.commit.author?.name,
    date: c.commit.author?.date
  }));
}

// patch text for a single commit, used by the ide diff viewer in cloud mode
export async function getCommitDiff(token, fullName, sha) {
  const { owner, repo } = parseRepo(fullName);
  const data = await gh(token, `/repos/${owner}/${repo}/commits/${encodeURIComponent(sha)}`);
  const files = data.files || [];
  return {
    sha: data.sha,
    short_sha: data.sha.slice(0, 7),
    message: data.commit?.message?.split('\n')[0],
    additions: data.stats?.additions ?? 0,
    deletions: data.stats?.deletions ?? 0,
    diff: files
      .map((f) => `--- a/${f.previous_filename || f.filename}\n+++ b/${f.filename}\n${f.patch || '(binary or too large to display)'}`)
      .join('\n\n')
  };
}

// ---- writes ----------------------------------------------------------

// commits a batch of file changes as ONE atomic commit using the git data
// api. files: [{ path, content }] where content === null means delete.
export async function commitFiles(token, fullName, branch, message, files) {
  const { owner, repo } = parseRepo(fullName);
  const base = `/repos/${owner}/${repo}`;

  if (!files.length) {
    return { success: false, error: 'no staged changes to commit' };
  }

  // 1. current head of the branch
  const ref = await gh(token, `${base}/git/ref/heads/${encodeURIComponent(branch)}`);
  const headSha = ref.object.sha;

  // 2. tree the head commit points at
  const headCommit = await gh(token, `${base}/git/commits/${headSha}`);
  const baseTreeSha = headCommit.tree.sha;

  // 3. upload each changed file as a blob
  const treeEntries = [];
  for (const file of files) {
    if (file.content === null) {
      // a null sha in a tree entry deletes the path
      treeEntries.push({ path: file.path, mode: '100644', type: 'blob', sha: null });
      continue;
    }
    const blob = await gh(token, `${base}/git/blobs`, {
      method: 'POST',
      body: JSON.stringify({
        content: Buffer.from(file.content, 'utf8').toString('base64'),
        encoding: 'base64'
      })
    });
    treeEntries.push({ path: file.path, mode: '100644', type: 'blob', sha: blob.sha });
  }

  // 4. new tree layered on top of the existing one
  const tree = await gh(token, `${base}/git/trees`, {
    method: 'POST',
    body: JSON.stringify({ base_tree: baseTreeSha, tree: treeEntries })
  });

  // 5. commit object
  const commit = await gh(token, `${base}/git/commits`, {
    method: 'POST',
    body: JSON.stringify({ message, tree: tree.sha, parents: [headSha] })
  });

  // 6. move the branch ref forward
  await gh(token, `${base}/git/refs/heads/${encodeURIComponent(branch)}`, {
    method: 'PATCH',
    body: JSON.stringify({ sha: commit.sha, force: false })
  });

  return {
    success: true,
    sha: commit.sha,
    short_sha: commit.sha.slice(0, 7),
    branch,
    files_changed: files.map((f) => f.path),
    url: `https://github.com/${owner}/${repo}/commit/${commit.sha}`
  };
}

// ---- deployment status ----------------------------------------------
//
// vercel's github integration posts a commit status for every build it runs.
// that means the deployment result is readable with the github token this
// harness already holds — no vercel credentials required. the status carries
// the state, the deployment id, and the inspector url; a vercel token adds
// the build log on top of it (see vercel-service.js).

function parseDeploymentId(description = '') {
  const m = String(description).match(/dpl_[A-Za-z0-9]+/);
  return m ? m[0] : null;
}

export async function getCommitDeployStatus(token, fullName, sha, { project = null } = {}) {
  const { owner, repo } = parseRepo(fullName);
  const data = await gh(token, `/repos/${owner}/${repo}/commits/${encodeURIComponent(sha)}/status`);

  const vercelStatuses = (data.statuses || []).filter((s) => /vercel/i.test(s.context || ''));

  // a repo can be (or have been) wired to more than one vercel project, and
  // github keeps the status of every one of them — including projects that no
  // longer exist. an unrelated project's red build must not be reported as
  // this harness being down.
  const scoped = project
    ? vercelStatuses.filter((s) => String(s.context || '').includes(project))
    : vercelStatuses;
  const relevant = scoped.length ? scoped : vercelStatuses;

  const checks = relevant
    .map((s) => ({
      context: s.context,
      state: s.state, // success | failure | error | pending
      description: s.description || null,
      url: s.target_url || null,
      deployment_id: parseDeploymentId(s.description)
    }));

  // combined state across non-vercel checks too, but vercel is what decides
  // whether the harness is actually live
  const vercelState = checks.length
    ? (checks.some((c) => c.state === 'failure' || c.state === 'error')
        ? 'failure'
        : checks.every((c) => c.state === 'success')
          ? 'success'
          : 'pending')
    : null;

  return {
    sha,
    found: checks.length > 0,
    state: vercelState || data.state || 'pending',
    checks,
    project: project || null,
    other_projects: project ? vercelStatuses.length - scoped.length : 0,
    source: 'github commit status'
  };
}

export async function getHeadSha(token, fullName, branch) {
  const { owner, repo } = parseRepo(fullName);
  const ref = await gh(token, `/repos/${owner}/${repo}/commits/${encodeURIComponent(branch)}`);
  return ref?.sha || null;
}

// ---- diffing ---------------------------------------------------------

// compact lcs line diff so the ide diff viewer still works in the cloud,
// where `git diff` is unavailable.
export function unifiedDiff(filePath, before, after) {
  const a = before === null ? [] : before.split('\n');
  const b = after === null ? [] : after.split('\n');

  const m = a.length;
  const n = b.length;
  const lcs = Array.from({ length: m + 1 }, () => new Uint32Array(n + 1));
  for (let i = m - 1; i >= 0; i--) {
    for (let j = n - 1; j >= 0; j--) {
      lcs[i][j] = a[i] === b[j]
        ? lcs[i + 1][j + 1] + 1
        : Math.max(lcs[i + 1][j], lcs[i][j + 1]);
    }
  }

  const lines = [];
  let i = 0;
  let j = 0;
  let added = 0;
  let removed = 0;
  while (i < m && j < n) {
    if (a[i] === b[j]) {
      lines.push(` ${a[i]}`);
      i++;
      j++;
    } else if (lcs[i + 1][j] >= lcs[i][j + 1]) {
      lines.push(`-${a[i++]}`);
      removed++;
    } else {
      lines.push(`+${b[j++]}`);
      added++;
    }
  }
  while (i < m) {
    lines.push(`-${a[i++]}`);
    removed++;
  }
  while (j < n) {
    lines.push(`+${b[j++]}`);
    added++;
  }

  const header = before === null
    ? `--- /dev/null\n+++ b/${filePath}`
    : after === null
      ? `--- a/${filePath}\n+++ /dev/null`
      : `--- a/${filePath}\n+++ b/${filePath}`;

  return { diff: `${header}\n${lines.join('\n')}`, added, removed };
}
