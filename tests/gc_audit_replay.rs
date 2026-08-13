//! Replay of the 2026-08 `/c/work/desktop` audit against gc's classifier.
//!
//! `tests/fixtures/desktop-audit-2026-08.json` is a vendored copy of the audit
//! that motivated `grove gc`: 65 unregistered worktrees found in one work_dir,
//! each hand-checked by a read-only shard and given a disposition. It is
//! vendored rather than read from `/c/work/desktop` so `cargo test` stays
//! self-contained on any checkout, and so the data outlives the mess.
//!
//! What it pins is the invariant the audit exists to protect: 34 of those trees
//! were judged safely removable *by a human reading PR records*, and gc must
//! still refuse to remove any of them on its own. The classifier is exercised
//! with the real paths, and the dirt counts the shards recorded are replayed
//! through the real blocker logic.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use grove::gc::guards::TreeSafety;
use grove::gc::{ALL_CATEGORIES, Category, Layout, classify_worktree_path};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct AuditEntry {
    path: PathBuf,
    branch: String,
    /// Count of `git status --porcelain` entries the shard saw.
    dirty: usize,
    /// `remove`, `adopt` or `human-review`.
    disposition: String,
}

fn audit() -> Vec<AuditEntry> {
    let raw = include_str!("fixtures/desktop-audit-2026-08.json");
    serde_json::from_str(raw).expect("vendored audit snapshot should parse")
}

fn layout() -> Layout {
    Layout {
        work_dir: PathBuf::from("/c/work/desktop"),
        main_repo: PathBuf::from("/c/work/desktop/master"),
    }
}

/// The audit covered *unregistered* worktrees, so the registry set is empty by
/// construction — that is what made every one of them a category 2 candidate.
fn unregistered() -> BTreeSet<PathBuf> {
    BTreeSet::new()
}

#[test]
fn the_vendored_snapshot_is_the_one_the_audit_produced() {
    let entries = audit();
    assert_eq!(entries.len(), 65, "vendored copy looks truncated");

    let count = |name: &str| entries.iter().filter(|e| e.disposition == name).count();
    assert_eq!(count("remove"), 34);
    assert_eq!(count("adopt"), 5);
    assert_eq!(count("human-review"), 26);
}

#[test]
fn every_audited_tree_lands_in_a_category_gc_will_not_delete() {
    for entry in audit() {
        let category = classify_worktree_path(&entry.path, &layout(), &unregistered());
        match category {
            Some(category) => assert!(
                !category.auto_applicable(),
                "{} classified as category {}, which --yes would delete unattended",
                entry.path.display(),
                category.number()
            ),
            None => {
                // Outside the work_dir entirely; gc does not touch it at all.
                assert!(
                    !entry.path.starts_with("/c/work/desktop/"),
                    "{} is inside the work_dir and should have been classified",
                    entry.path.display()
                );
            }
        }
    }
}

#[test]
fn the_thirty_four_removable_trees_are_category_two_candidates() {
    let mut checked = 0;
    for entry in audit().iter().filter(|e| e.disposition == "remove") {
        if !entry.path.starts_with("/c/work/desktop/") {
            continue;
        }
        assert_eq!(
            classify_worktree_path(&entry.path, &layout(), &unregistered()),
            Some(Category::UnregisteredWorktree),
            "{} should be offered as an adopt-or-remove prompt",
            entry.path.display()
        );
        checked += 1;
    }
    assert_eq!(checked, 34, "all 34 remove candidates should be in scope");
    assert!(
        !Category::UnregisteredWorktree.auto_applicable(),
        "a human said these are removable; gc still has to ask"
    );
}

#[test]
fn the_live_work_the_audit_flagged_would_be_offered_not_deleted() {
    let adopt: Vec<_> = audit()
        .into_iter()
        .filter(|e| e.disposition == "adopt")
        .collect();
    assert_eq!(adopt.len(), 5);
    for entry in adopt {
        assert_eq!(
            classify_worktree_path(&entry.path, &layout(), &unregistered()),
            Some(Category::UnregisteredWorktree),
            "{} carried live work ({}) and must reach a prompt, never a sweep",
            entry.path.display(),
            entry.branch
        );
    }
}

#[test]
fn a_worktree_outside_the_work_dir_is_out_of_scope() {
    let outside: Vec<_> = audit()
        .into_iter()
        .filter(|e| !e.path.starts_with("/c/work/desktop/"))
        .collect();
    assert_eq!(
        outside.len(),
        1,
        "the audit found exactly one tree outside the work_dir"
    );
    assert_eq!(
        classify_worktree_path(&outside[0].path, &layout(), &unregistered()),
        None,
        "gc classifies nothing outside the work_dir it was pointed at"
    );
}

#[test]
fn the_dirt_the_shards_recorded_would_block_removal() {
    let mut dirty_seen = 0;
    for entry in audit() {
        let safety = TreeSafety {
            dirty: entry.dirty > 0,
            dirty_entries: entry.dirty,
            unpushed: false,
            head: None,
            branch: Some(entry.branch.clone()),
        };
        if entry.dirty > 0 {
            dirty_seen += 1;
            assert!(
                safety.blockers().iter().any(|b| b.contains("uncommitted")),
                "{} had {} uncommitted entries and must be blocked",
                entry.path.display(),
                entry.dirty
            );
        } else {
            assert!(
                safety.blockers().is_empty(),
                "{} was clean; dirt is not the reason to hold it back",
                entry.path.display()
            );
        }
    }
    assert_eq!(dirty_seen, 23, "the audit recorded 23 dirty trees");
}

#[test]
fn no_audited_path_reaches_an_unattended_category() {
    let reachable: BTreeSet<u8> = audit()
        .iter()
        .filter_map(|e| classify_worktree_path(&e.path, &layout(), &unregistered()))
        .map(|c| c.number())
        .collect();
    assert_eq!(
        reachable,
        [2].into_iter().collect::<BTreeSet<u8>>(),
        "the whole audit is category 2; anything else is a precedence regression"
    );

    let unattended: BTreeSet<u8> = ALL_CATEGORIES
        .iter()
        .filter(|c| c.auto_applicable())
        .map(|c| c.number())
        .collect();
    assert!(
        reachable.is_disjoint(&unattended),
        "audited paths must never overlap the categories --yes applies"
    );
}

#[test]
fn the_harness_precedence_rule_holds_for_real_paths() {
    // Not in the audit — the shards were pointed at the work_dir's top level —
    // but the same classifier decides it, and getting it wrong would sweep a
    // live agent session into the adopt-or-remove prompt.
    let harness = Path::new("/c/work/desktop/master/.claude/worktrees/agent-a139da4b436e8c126");
    assert_eq!(
        classify_worktree_path(harness, &layout(), &unregistered()),
        Some(Category::HarnessWorktree)
    );
}
