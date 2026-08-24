//! branch-policy evals: the rules that keep production main promoted, not
//! pushed blind (STACKED_PRS_PLAN §4; D10's sibling for refs).
//!
//! these are the decisions a hostile or confused run must not be able to
//! talk its way past, so each carries negative controls: the refusal paths
//! are asserted as directly as the allow paths.

use vanish::agent::github::{Github, PrStatus};
use vanish::agent::tools::{
    branch_for_conversation, commit_allowed_on, pr_gate, PROTECTED_BRANCH,
};

// ---- ref naming ------------------------------------------------------------

#[test]
fn only_agent_refs_are_creatable() {
    assert!(Github::is_agent_ref("agent/conv-1"));
    assert!(Github::is_agent_ref("agent/fix-d10-guard"));
    // everything a confused or hostile run might reach for must refuse:
    assert!(!Github::is_agent_ref(PROTECTED_BRANCH));
    assert!(!Github::is_agent_ref("main"));
    assert!(!Github::is_agent_ref("master"));
    assert!(!Github::is_agent_ref("release/1.0"));
    assert!(!Github::is_agent_ref("Agent/x"), "case matters");
    assert!(!Github::is_agent_ref("agentx/y"));
    assert!(!Github::is_agent_ref(""));
}

// ---- branch assignment ------------------------------------------------------

#[test]
fn conversations_default_to_isolated_agent_branches() {
    // no branch yet → isolated default keyed by conversation id.
    assert_eq!(
        branch_for_conversation(None, "conv-9"),
        "agent/conv-9"
    );
    // an existing non-agent branch is NOT kept: it would be main.
    assert_eq!(
        branch_for_conversation(Some("main"), "conv-9"),
        "agent/conv-9",
        "main must never become a conversation's working branch by inheritance"
    );
}

#[test]
fn claimed_agent_branches_are_sticky() {
    // a conversation already working on an agent/ branch keeps it across
    // runs — re-deriving from the conversation id would scatter work.
    assert_eq!(
        branch_for_conversation(Some("agent/fix-d10-guard"), "conv-9"),
        "agent/fix-d10-guard"
    );
}

// ---- commit gate ------------------------------------------------------------

#[test]
fn direct_commits_to_main_are_refused_with_the_escape_hatch() {
    let verdict = commit_allowed_on("main");
    let msg = match &verdict {
        Err(e) => e.clone(),
        Ok(()) => panic!("main must refuse direct commits"),
    };
    // D9 for refusals: the error must name the way out.
    assert!(msg.contains("agent/"), "refusal must name the escape hatch: {msg}");
    assert!(msg.to_lowercase().contains("pr") || msg.contains("open_pr"), "refusal must name the pr path: {msg}");
}

#[test]
fn agent_branches_accept_direct_commits() {
    assert!(commit_allowed_on("agent/conv-1").is_ok());
    assert!(commit_allowed_on("agent/anything-goes").is_ok());
}

// ---- merge gate ---------------------------------------------------------------

fn pr(number: u64, mergeable: Option<bool>, verdict: &str) -> PrStatus {
    PrStatus {
        number,
        head_sha: "abc1234def5678".to_string(),
        mergeable,
        deploy_verdict: verdict.to_string(),
    }
}

#[test]
fn green_and_mergeable_is_the_only_open_gate() {
    assert!(pr_gate(&pr(7, Some(true), "success")).is_ok());
}

#[test]
fn every_other_combination_is_refused_with_reasons() {
    // conflicts
    let e = pr_gate(&pr(7, Some(false), "success")).unwrap_err();
    assert!(e.contains("not mergeable"), "{e}");
    // github still counting — merging now is a coin flip
    let e = pr_gate(&pr(7, None, "success")).unwrap_err();
    assert!(e.contains("mergeable=None") || e.contains("resolve"), "{e}");
    // red build discovered before merge, not after
    let e = pr_gate(&pr(7, Some(true), "failure")).unwrap_err();
    assert!(e.contains("FAILED"), "{e}");
    // build still running: waiting is free (D1), guessing is expensive
    let e = pr_gate(&pr(7, Some(true), "pending")).unwrap_err();
    assert!(e.contains("still running") || e.contains("settles"), "{e}");
    // no signal at all is NOT a pass — it is an unread one (D4)
    let e = pr_gate(&pr(7, Some(true), "none")).unwrap_err();
    assert!(e.contains("no settled check verdict"), "{e}");
}
