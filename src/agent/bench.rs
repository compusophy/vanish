//! the internal eval harness: pinned self-edit tasks, scored mechanically.
//!
//! a benchmark is only honest if its checkers cannot be argued with. every
//! task here has a deterministic checker evaluated against a snapshot of
//! observable facts (which files exist, what they contain, how many tests
//! ran, whether anything was committed) — never against the model's own
//! claim of success. task_complete is data, not evidence.
//!
//! the suite lives IN the repository it tests: the tasks are self-edits, so
//! the benchmark exercises exactly the loop this repo's own history shows
//! (read → edit → verify). grading is pure; only the worker touches io.

/// what a checker demands, expressed as data so it survives serde round
/// trips and can be rendered into the report without a match in the ui.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Checker {
    /// a file must exist AND contain all of these substrings.
    FileContains { path: String, contains: Vec<String> },
    /// a file must exist but NOT contain any of these substrings.
    FileExcludes { path: String, excludes: Vec<String> },
    /// the file must simply be there.
    FileExists { path: String },
    /// cargo test must have found at least this many tests.
    TestCountAtLeast { minimum: usize },
    /// at least one commit landed during the run.
    CommitExists,
}

impl Checker {
    /// evaluate against the observable facts. pure; no io.
    pub fn check(&self, snap: &BenchSnapshot) -> bool {
        match self {
            Checker::FileContains { path, contains } => snap
                .files
                .get(path)
                .map(|body| contains.iter().all(|needle| body.contains(needle)))
                .unwrap_or(false),
            Checker::FileExcludes { path, excludes } => snap
                .files
                .get(path)
                .map(|body| excludes.iter().all(|bad| !body.contains(bad)))
                .unwrap_or(false),
            Checker::FileExists { path } => snap.files.contains_key(path),
            Checker::TestCountAtLeast { minimum } => snap.test_count >= *minimum,
            Checker::CommitExists => snap.has_commit,
        }
    }
}

/// the observable state grading reads. built by the worker from opfs after
/// the whole batch finishes; pure code only ever sees this.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BenchSnapshot {
    /// full contents of files the checkers care about.
    pub files: std::collections::BTreeMap<String, String>,
    /// total tests discovered by the last cargo test run.
    pub test_count: usize,
    /// did any git commit land since the benchmark started.
    pub has_commit: bool,
}

/// one pinned task. the prompts are deliberately concrete and small: a
/// benchmark task that needs judgment produces noisy scores.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BenchTask {
    pub id: &'static str,
    pub prompt: &'static str,
    pub checker: Checker,
}

pub const BENCH_TASKS: &[BenchTask] = &[
    BenchTask {
        id: "bench-read-and-report",
        prompt: "Read README.md and reply with its first heading. Do not edit any file.",
        checker: Checker::FileContains {
            path: "vanish-bench/notes.md",
            contains: vec!["#"],
        },
    },
    BenchTask {
        id: "bench-create-file",
        prompt: "Create the file vanish-bench/hello.txt containing exactly the text: hello vanish",
        checker: Checker::FileContains {
            path: "vanish-bench/hello.txt",
            contains: vec!["hello vanish"],
        },
    },
    BenchTask {
        id: "bench-edit-precise",
        prompt: "In vanish-bench/hello.txt, append a second line reading: line two",
        checker: Checker::FileContains {
            path: "vanish-bench/hello.txt",
            contains: vec!["hello vanish", "line two"],
        },
    },
    BenchTask {
        id: "bench-rust-fn",
        prompt: "Add a public function named bench_marker to src/lib.rs returning the u32 value 42. Do not remove anything else.",
        checker: Checker::FileContains {
            path: "src/lib.rs",
            contains: vec!["fn bench_marker"],
        },
    },
    BenchTask {
        id: "bench-remove-token",
        prompt: "In vanish-bench/todo.md, delete the line containing REMOVE_ME entirely.",
        checker: Checker::FileExcludes {
            path: "vanish-bench/todo.md",
            excludes: vec!["REMOVE_ME"],
        },
    },
];

/// grade one task against the snapshot. pass/fail plus the reason string
/// for the report — the reason names the CHECKER, not a vibe.
pub fn grade_task(task: &BenchTask, snap: &BenchSnapshot) -> (bool, String) {
    let ok = task.checker.check(snap);
    let reason = if ok {
        format!("checker satisfied: {}", describe(&task.checker))
    } else {
        format!("checker NOT satisfied: {}", describe(&task.checker))
    };
    (ok, reason)
}

fn describe(c: &Checker) -> String {
    match c {
        Checker::FileContains { path, .. } => format!("{path} must contain required text"),
        Checker::FileExcludes { path, .. } => format!("{path} must not contain excluded text"),
        Checker::FileExists { path } => format!("{path} must exist"),
        Checker::TestCountAtLeast { minimum } => format!("test count >= {minimum}"),
        Checker::CommitExists => "at least one commit landed".to_string(),
    }
}

/// the aggregate report written to vanish-bench/report.json.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BenchReport {
    pub entries: Vec<BenchEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BenchEntry {
    pub id: String,
    /// "pass" | "fail" | the batch-level reason ("stopped" etc.) when the
    /// task never produced a completed run.
    pub verdict: String,
    pub reason: String,
}

impl BenchReport {
    pub fn passed(&self) -> usize {
        self.entries.iter().filter(|e| e.verdict == "pass").count()
    }
    pub fn total(&self) -> usize {
        self.entries.len()
    }

    /// render the human-readable scorecard emitted into the feed.
    pub fn scorecard(&self) -> String {
        let mut lines = vec![format!(
            "benchmark: {}/{} passed",
            self.passed(),
            self.total()
        )];
        for e in &self.entries {
            let mark = if e.verdict == "pass" { "✓" } else { "✗" };
            lines.push(format!("{mark} {} — {}", e.id, e.verdict));
        }
        lines.join("\n")
    }
}

/// every file path any checker inspects. the worker reads exactly these
/// from opfs to build the snapshot — no guessing, no whole-tree scan.
pub fn checked_paths() -> Vec<&'static str> {
    let mut out: Vec<&'static str> = Vec::new();
    for t in BENCH_TASKS {
        let p = match &t.checker {
            Checker::FileContains { path, .. }
            | Checker::FileExcludes { path, .. }
            | Checker::FileExists { path } => path.as_str(),
            Checker::TestCountAtLeast { .. } | Checker::CommitExists => continue,
        };
        if !out.contains(&p) {
            out.push(p);
        }
    }
    out
}

/// grade the whole suite, in pinned submission order regardless of outcome.
pub fn grade_all(snap: &BenchSnapshot) -> BenchReport {
    BenchReport {
        entries: BENCH_TASKS
            .iter()
            .map(|t| {
                let (ok, reason) = grade_task(t, snap);
                BenchEntry {
                    id: t.id.to_string(),
                    verdict: if ok { "pass" } else { "fail" }.to_string(),
                    reason,
                }
            })
            .collect(),
    }
}
