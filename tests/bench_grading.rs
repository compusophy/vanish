//! benchmark grading evals.
//!
//! the checkers are the scoring function of the whole self-benchmark; if
//! they can be satisfied by anything less than the real edit, every score
//! the harness ever reports is inflated. each scenario therefore carries a
//! negative control: a plausible-but-wrong working tree must FAIL.

use std::collections::BTreeMap;

use vanish::agent::bench::{grade_all, checked_paths, BenchSnapshot, Checker};

fn snap(pairs: &[(&str, &str)]) -> BenchSnapshot {
    BenchSnapshot {
        files: pairs
            .iter()
            .map(|(p, c)| (p.to_string(), c.to_string()))
            .collect::<BTreeMap<_, _>>(),
        test_count: 0,
        has_commit: false,
    }
}

// ---- file contains --------------------------------------------------------

#[test]
fn file_contains_passes_on_real_edit_and_fails_on_near_miss() {
    let c = Checker::FileContains {
        path: "vanish-bench/hello.txt".into(),
        contains: vec!["hello vanish".into()],
    };

    let good = snap(&[("vanish-bench/hello.txt", "hello vanish\n")]);
    assert!(c.check(&good));

    // near-miss 1: right file, wrong text.
    let wrong_text = snap(&[("vanish-bench/hello.txt", "goodbye world\n")]);
    assert!(!c.check(&wrong_text), "wrong content must not pass");

    // near-miss 2: right text, wrong file.
    let wrong_file = snap(&[("notes/hello.txt", "hello vanish")]);
    assert!(!c.check(&wrong_file), "content in another file must not pass");

    // near-miss 3: file absent entirely.
    assert!(!c.check(&snap(&[])), "missing file must not pass");
}

#[test]
fn multi_needle_checker_demands_every_fragment() {
    // bench-edit-precise requires BOTH lines: appending only one of them is
    // a half-done edit and must score as a failure.
    let c = Checker::FileContains {
        path: "vanish-bench/hello.txt".into(),
        contains: vec!["hello vanish".into(), "line two".into()],
    };
    assert!(c.check(&snap(&[(
        "vanish-bench/hello.txt",
        "hello vanish\nline two"
    )])));
    let half_done = snap(&[("vanish-bench/hello.txt", "line two only")]);
    assert!(!c.check(&half_done), "one of two required fragments must not pass");
}

#[test]
fn substring_matching_is_literal_not_regex() {
    // a checker must not be satisfiable by regex trickery in the target
    // file: "." matches any char in a regex engine but here demands a dot.
    let c = Checker::FileContains {
        path: "f.txt".into(),
        contains: vec!["a.c".into()],
    };
    assert!(c.check(&snap(&[("f.txt", "a.c")])));
    assert!(
        !c.check(&snap(&[("f.txt", "abc")])),
        "regex-style near match must not satisfy a literal needle"
    );
}

// ---- file excludes --------------------------------------------------------

#[test]
fn file_excludes_fails_when_token_survives_anywhere() {
    let c = Checker::FileExcludes {
        path: "vanish-bench/todo.md".into(),
        excludes: vec!["REMOVE_ME".into()],
    };
    assert!(c.check(&snap(&[("vanish-bench/todo.md", "- keep\n- also keep")])));
    // the deletion was partial: the token survives on another line.
    let partial = snap(&[("vanish-bench/todo.md", "- keep\n- REMOVE_ME later")]);
    assert!(!c.check(&partial), "surviving token must fail the checker");

    // an ABSENT file fails too, deliberately: FileExcludes demands the file
    // EXIST and be clean. deleting the entire todo.md is not completing
    // "delete the line" — it is destroying the file — and grading it as
    // success would make every removal task gameable by rm. this strictness
    // is also what makes "an empty tree scores zero" true at the suite
    // level (pinned by grade_all_reports_every_pinned_task_in_order).
    assert!(
        !c.check(&snap(&[])),
        "a removed file must not satisfy a removal checker"
    );
}

// ---- exists / count / commit ----------------------------------------------

#[test]
fn file_exists_needs_only_presence() {
    let c = Checker::FileExists { path: "x/y.md".into() };
    assert!(c.check(&snap(&[("x/y.md", "")])));
    assert!(!c.check(&snap(&[])));
}

#[test]
fn test_count_and_commit_checkers_read_the_snapshot() {
    let mut s = snap(&[]);
    assert!(!Checker::TestCountAtLeast { minimum: 10 }.check(&s));
    s.test_count = 10;
    assert!(Checker::TestCountAtLeast { minimum: 10 }.check(&s));
    // the snapshot starts with has_commit=false (snap() pins that); a commit
    // landing is what flips it, and CommitExists reads exactly that fact.
    s.has_commit = true;
    assert!(Checker::CommitExists.check(&s.clone()));
    s.has_commit = false;
    assert!(!Checker::CommitExists.check(&s));
}

// ---- suite-level behavior --------------------------------------------------

#[test]
fn grade_all_reports_every_pinned_task_in_order() {
    // an empty snapshot fails everything that touches files; that is the
    // honest baseline and proves no checker can pass on absence.
    let report = grade_all(&snap(&[]));
    assert_eq!(report.total(), 5);
    assert_eq!(report.passed(), 0, "empty tree must score zero");
    // submission order preserved: ids appear exactly once, in pinned order.
    let ids: Vec<&str> = report.entries.iter().map(|e| e.id.as_str()).collect();
    assert_eq!(
        ids,
        vec![
            "bench-read-and-report",
            "bench-create-file",
            "bench-edit-precise",
            "bench-rust-fn",
            "bench-remove-token",
        ]
    );
}

#[test]
fn checked_paths_covers_every_file_backed_checker() {
    let paths = checked_paths();
    // every file-named checker's path is present...
    assert!(paths.contains(&"src/lib.rs"));
    assert!(paths.contains(&"vanish-bench/todo.md"));
    // ...and there are no duplicates: the worker reads each path once.
    let unique: std::collections::BTreeSet<_> = paths.iter().collect();
    assert_eq!(unique.len(), paths.len());
}

#[test]
fn scorecard_is_human_readable_and_honest_about_failures() {
    let report = grade_all(&snap(&[("vanish-bench/hello.txt", "hello vanish")]));
    let card = report.scorecard();
    assert!(card.contains("benchmark: 1/5 passed"));
    assert!(card.contains("✗ bench-remove-token — fail"));
    assert!(card.contains("✓ bench-create-file — pass"));
}
