mod common;

use grove::cli::adopt::{AdoptArgs, run as adopt};
use grove::cli::done::{DoneArgs, run as done};
use grove::cli::freeze::{FreezeArgs, run_freeze as freeze, run_thaw as thaw};
use grove::cli::new::{NewArgs, run as new_project};
use grove::cli::rename::{RenameArgs, run as rename};
use grove::registry::Registry;

use common::GitFixture;

/// Full Phase 3 lifecycle: new → adopt → rename → freeze → done
///
/// Each step verifies registry state and on-disk presence.
#[test]
fn phase3_full_lifecycle() {
    let fx = GitFixture::new().with_grove_registry();
    let work_dir = fx.temp_dir.path().to_path_buf();
    let grove_dir = fx.grove_dir.clone();

    // ── Step 1: grove new "alpha" ─────────────────────────────────────────
    {
        let cx = fx.make_context();
        let args = NewArgs {
            tag: "alpha".to_string(),
            issue: None,
            branch: None,
            base: None,
            no_fetch: true,
        };
        new_project(&args, &cx).expect("grove new alpha should succeed");

        let reg = Registry::load(&grove_dir).unwrap();
        assert!(
            reg.projects.contains_key("alpha"),
            "alpha should be in registry after new"
        );
        let proj = &reg.projects["alpha"];
        assert_eq!(proj.branch, "alpha");
        assert!(work_dir.join("alpha").exists(), "worktree dir should exist");
    }

    // ── Step 2: grove adopt an external worktree as "external" ────────────
    let external_wt = fx.add_worktree("external", "external-branch");
    {
        let cx = fx.make_context();
        let args = AdoptArgs {
            tag: "external".to_string(),
            path: external_wt.clone(),
            issue: None,
            base: None,
            mv: false,
        };
        adopt(&args, &cx).expect("grove adopt external should succeed");

        let reg = Registry::load(&grove_dir).unwrap();
        assert!(
            reg.projects.contains_key("external"),
            "external should be in registry after adopt"
        );
        let proj = &reg.projects["external"];
        assert_eq!(proj.branch, "external-branch");
    }

    // ── Step 3: grove rename "external" → "ext-renamed" ──────────────────
    {
        let cx = fx.make_context();
        let args = RenameArgs {
            old_tag: "external".to_string(),
            new_tag: "ext-renamed".to_string(),
            no_move: false,
        };
        rename(&args, &cx).expect("grove rename should succeed");

        let reg = Registry::load(&grove_dir).unwrap();
        assert!(
            !reg.projects.contains_key("external"),
            "old tag should be gone after rename"
        );
        assert!(
            reg.projects.contains_key("ext-renamed"),
            "new tag should be in registry after rename"
        );
        // Directory should have moved
        assert!(
            work_dir.join("ext-renamed").exists(),
            "directory should exist at new path after rename"
        );
    }

    // ── Step 4: grove freeze "alpha" ─────────────────────────────────────
    {
        let cx = fx.make_context();
        let args = FreezeArgs {
            tag: Some("alpha".to_string()),
        };
        freeze(&args, &cx).expect("grove freeze alpha should succeed");

        let reg = Registry::load(&grove_dir).unwrap();
        assert!(reg.projects["alpha"].frozen, "alpha should be frozen");
    }

    // ── Step 5: grove thaw "alpha" ────────────────────────────────────────
    {
        let cx = fx.make_context();
        let args = FreezeArgs {
            tag: Some("alpha".to_string()),
        };
        thaw(&args, &cx).expect("grove thaw alpha should succeed");

        let reg = Registry::load(&grove_dir).unwrap();
        assert!(!reg.projects["alpha"].frozen, "alpha should be thawed");
    }

    // ── Step 6: grove done "alpha" (force — no remote configured) ─────────
    {
        let cx = fx.make_context();
        let args = DoneArgs {
            tag: Some("alpha".to_string()),
            force: true,
            keep_local: false,
            keep_remote: true, // no real remote push was done; skip remote delete
        };
        done(&args, &cx).expect("grove done alpha should succeed");

        let reg = Registry::load(&grove_dir).unwrap();
        assert!(
            !reg.projects.contains_key("alpha"),
            "alpha should be removed from registry after done"
        );
        assert!(
            !work_dir.join("alpha").exists(),
            "alpha worktree directory should be removed after done"
        );
    }

    // ── Final state: only ext-renamed remains ─────────────────────────────
    {
        let reg = Registry::load(&grove_dir).unwrap();
        assert_eq!(
            reg.projects.len(),
            1,
            "only 1 project should remain, got: {:?}",
            reg.projects.keys().collect::<Vec<_>>()
        );
        assert!(
            reg.projects.contains_key("ext-renamed"),
            "ext-renamed should still be in registry"
        );
    }
}
