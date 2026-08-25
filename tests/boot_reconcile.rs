//! the recurring boot reconcile error, pinned shut.
//!
//! history: every boot emitted "reconcile error ... Failed to execute
//! 'removeEntry' ... NoModificationAllowedError" because TWO Configure
//! commands arrive at every boot — the worker self-configures from the opfs
//! config mirror AND the ui sends its own after Event::Ready. the latch that
//! was supposed to make one of them skip only closed AFTER the first
//! reconcile finished (check-then-act across an await), so both tasks passed
//! the gate and raced over the same stale cache files; chrome refuses
//! removeEntry while another task holds a file open for writing. worse,
//! reconcile propagated that error with `?`, so ONE locked file aborted the
//! whole D10 arming pass.
//!
//! three fixes, each pinned here:
//! 1. the claim is atomic (checked and set in one borrow) and taken before
//!    any await — the loser skips cleanly instead of racing;
//! 2. a failed pass releases the claim so the next Configure retries;
//! 3. per-file delete failures no longer abort the pass.
//!
//! like tests/ci_gate.rs, some of these are deliberately blunt source-level
//! greps: the failure mode is "a refactor reintroduces check-then-set across
//! an await", which no behavioral test on pure functions can see.

use std::path::Path;

fn source(rel: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(rel);
    // normalize line endings: the shape guards below search for `{\n`
    // patterns, and a windows checkout with core.autocrlf=true serves the
    // same source as `{\r\n` — a false red on a tree ci would pass.
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("could not read {rel}: {e}"))
        .replace("\r\n", "\n")
}

// ---- fix 3: partial reconcile survives --------------------------------------

use vanish::agent::tools::ReconcileReport;

#[test]
fn reconcile_report_defaults_to_a_clean_empty_pass() {
    // Default must exist so callers can build a report without knowing every
    // field; all-empty is the honest zero value.
    let r = ReconcileReport::default();
    assert!(r.refreshed.is_empty());
    assert!(r.uncommitted.is_empty());
    assert!(r.failed.is_empty());
    assert_eq!(r.files_on_branch, 0);
}

#[test]
fn reconcile_report_carries_failed_entries() {
    // negative control for the field's purpose: `failed` records files whose
    // cache could not be dropped THIS pass, so they can be surfaced (D4)
    // instead of silently skipped or treated as refreshed.
    let r = ReconcileReport {
        head: "abc1234".to_string(),
        files_on_branch: 2,
        refreshed: vec!["a.md".to_string()],
        uncommitted: vec![],
        failed: vec!["memory/TASKBOARD.md".to_string()],
    };
    assert_eq!(r.failed.len(), 1);
    assert_ne!(r.failed[0], r.refreshed[0], "a file cannot be both dropped and failed");
}

#[test]
fn reconcile_loop_treats_a_failed_delete_as_non_fatal() {
    // the exact shape of the old bug: `opfs::delete(&item.path).await?` —
    // the `?` aborts the entire pass on the first locked file, leaving the
    // session unreconciled (and re-emitting the scary error every boot).
    let ws = source("src/agent/tools.rs");
    let start = ws.find("pub async fn reconcile_against_branch")
        .unwrap_or_else(|| panic!("reconcile_against_branch not found in tools.rs"));
    let body = &ws[start..];
    assert!(
        !body.contains("opfs::delete(&item.path).await?"),
        "reconcile_against_branch propagates delete failures with '?'. one \
         locked file (another task holding it open) then aborts the WHOLE \
         pass — this was the recurring 'reconcile error ... \
         NoModificationAllowedError' at every boot. collect the failure into \
         ReconcileReport::failed instead."
    );
    assert!(
        body.contains("failed.push(item.path.clone())"),
        "reconcile_against_branch no longer records failed deletions; \
         ReconcileReport::failed exists precisely to carry them."
    );
}

#[test]
fn a_failed_delete_keeps_its_index_entry() {
    // dropping the index record while the file stays on disk would leave
    // stale bytes that read_through serves as trusted local content — the
    // 37-file discovery class again. the entry must be kept so the NEXT
    // reconcile pass sees and retries it.
    let ws = source("src/agent/tools.rs");
    let start = ws.find("Err(_) => {\n                        failed.push")
        .expect("the non-fatal delete-failure arm moved or changed shape");
    let arm = &ws[start..start + 400];
    assert!(
        !arm.contains(".remove(&item.path)"),
        "the delete-failure arm removes the index entry while the file itself \
         remains on disk. that desyncs index from disk and turns stale bytes \
         into trusted content. keep the entry so a later pass retries."
    );
}

// ---- fixes 1+2: the atomic claim gate ---------------------------------------

#[test]
fn the_reconcile_claim_is_checked_and_set_atomically() {
    // check-then-set split across two STATE.with calls (or around an await)
    // is exactly the race that let both boot Configures run reconciles. the
    // fixed site reads the flag and sets it inside ONE borrow, with no
    // await between them. grep-shaped: any future refactor back to two
    // borrows fails here.
    let worker = source("src/worker.rs");
    let start = worker
        .find("let claimed_reconcile")
        .unwrap_or_else(|| panic!("claimed_reconcile not found in worker.rs"));
    let block = &worker[start..start + 700];

    assert!(
        block.contains("should_auto_reconcile"),
        "the claim site bypassed should_auto_reconcile once already (the gate \
         went dead-code when its last caller was rewritten inline). the \
         shared function stays THE definition of who may reconcile."
    );
    let between_borrows = &block[block.find("STATE.with").unwrap()..];
    let borrow_count = between_borrows.matches("s.borrow()").count()
        + between_borrows.matches("s.borrow_mut()").count();
    assert!(
        borrow_count >= 2 && !between_borrows.contains(".await"),
        "the claim must read AND set auto_reconciled inside one STATE.with \
         with NO await between them — splitting them across awaits is the \
         original double-reconcile race."
    );
}

#[test]
fn a_failed_pass_releases_the_claim_for_retry() {
    // if the reconcile errors AFTER claiming, the claim must be released, or
    // one transient failure permanently disarms boot-time D10 reconciliation
    // for the whole session.
    let worker = source("src/worker.rs");
    let start = worker
        .find("if claimed_reconcile {")
        .unwrap_or_else(|| panic!("claimed_reconcile block missing"));
    let end = worker[start..]
        .find("// optional: absent is fine")
        .unwrap_or_else(|| panic!("could not find the end of the reconcile block"));
    let block = &worker[start..start + end];

    assert!(
        block.contains("auto_reconciled = false"),
        "the Err arm of the boot reconcile does not release the claim. a \
         single transient failure would then permanently disarm auto-\
         reconcile for the session — worse than the bug being fixed."
    );
    assert!(
        block.contains("\"reconcile\""),
        "the failure must still surface as an Error event scoped to \
         'reconcile' (D4), never a silent skip."
    );
}

// ---- fix 2's substrate: bounded retry on locked deletes ---------------------

#[test]
fn opfs_delete_retries_locked_files_instead_of_failing() {
    // chrome refuses removeEntry while another task holds the file open.
    // the file WILL become deletable once that writer's stream is collected,
    // so the correct response is a bounded timer-based retry (timer, not
    // resolved-promise: D7). pinned here so a refactor cannot quietly drop
    // the retry loop and resurrect the boot error.
    let opfs = source("src/platform/opfs.rs");
    assert!(
        opfs.contains("NoModificationAllowedError"),
        "opfs::delete no longer recognizes the locked-file exception by name; \
         the retry path is dead without it."
    );
    assert!(
        opfs.contains("MAX_REMOVE_RETRIES") && opfs.contains("REMOVE_RETRY_MS"),
        "opfs::delete lost its bounded retry constants — an unbounded retry \
         would spin forever on a genuinely undeletable file."
    );
    assert!(
        opfs.contains("sleep_ms(REMOVE_RETRY_MS)"),
        "the delete retry sleeps through something other than a real timer. \
         awaiting a resolved promise starves the event loop (D7); these \
         retries fire inside boot tasks that must keep answering messages."
    );
}
