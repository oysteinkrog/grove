//! Integration coverage for `grove gc` against real git fixtures.
//!
//! Each test builds a throwaway work_dir laid out the way the real one is —
//! a bare origin outside it, a main clone inside it, `.grove`, `.scratch` and
//! `.archive` beside the projects — and drives the scanner and the apply layer
//! directly. The guards that matter (dirty, unpushed, expired-but-dirty,
//! in-use) each get their own case, because they are the only thing standing
//! between gc and somebody's unfinished work.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;
use time::{Duration, OffsetDateTime};

use grove::gc::guards::ProbeContext;
use grove::gc::scan::Scanner;
use grove::gc::{Category, Finding, GcOptions, GcPlan, Layout, Remedy, apply};
use grove::registry::{Project, Registry};

// ── fixture ──────────────────────────────────────────────────────────────────

struct Fixture {
    _root: TempDir,
    work_dir: PathBuf,
    main_repo: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = TempDir::new().unwrap();
        let origin = root.path().join("origin.git");
        let work_dir = root.path().join("work");
        let main_repo = work_dir.join("master");
        std::fs::create_dir_all(&work_dir).unwrap();

        run_git(&["init", "--bare", "-b", "main", origin.to_str().unwrap()]);

        let seed = root.path().join("seed");
        run_git(&["init", "-b", "main", seed.to_str().unwrap()]);
        git(&seed, &["config", "user.email", "t@t.com"]);
        git(&seed, &["config", "user.name", "T"]);
        std::fs::write(seed.join("README.md"), b"grove gc fixture").unwrap();
        git(&seed, &["add", "."]);
        git(&seed, &["commit", "-m", "init"]);
        git(
            &seed,
            &["remote", "add", "origin", origin.to_str().unwrap()],
        );
        git(&seed, &["push", "-u", "origin", "main"]);

        run_git(&[
            "clone",
            origin.to_str().unwrap(),
            main_repo.to_str().unwrap(),
        ]);
        git(&main_repo, &["config", "user.email", "t@t.com"]);
        git(&main_repo, &["config", "user.name", "T"]);

        std::fs::create_dir_all(work_dir.join(".grove")).unwrap();

        Self {
            _root: root,
            work_dir,
            main_repo,
        }
    }

    fn layout(&self) -> Layout {
        Layout {
            work_dir: self.work_dir.clone(),
            main_repo: self.main_repo.clone(),
        }
    }

    fn grove_dir(&self) -> PathBuf {
        self.work_dir.join(".grove")
    }

    /// Add a worktree at `<work_dir>/<name>` on a new branch.
    fn worktree(&self, name: &str, branch: &str) -> PathBuf {
        self.worktree_at(&self.work_dir.join(name), branch)
    }

    fn worktree_at(&self, path: &Path, branch: &str) -> PathBuf {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        git(
            &self.main_repo,
            &[
                "worktree",
                "add",
                "-b",
                branch,
                path.to_str().unwrap(),
                "HEAD",
            ],
        );
        path.to_path_buf()
    }

    fn save_registry(&self, projects: BTreeMap<String, Project>) {
        Registry {
            schema_version: 1,
            projects,
        }
        .save(&self.grove_dir())
        .unwrap();
    }

    fn registry(&self) -> Registry {
        Registry::load(&self.grove_dir()).unwrap()
    }

    fn plan(&self, opts: &GcOptions) -> GcPlan {
        self.plan_with_probes(opts, &ProbeContext::empty())
    }

    fn plan_with_probes(&self, opts: &GcOptions, probes: &ProbeContext) -> GcPlan {
        let registry = self.registry();
        let layout = self.layout();
        Scanner {
            layout: &layout,
            registry: &registry,
            opts,
            probes,
        }
        .scan()
    }

    /// Apply everything `--yes` would apply, returning the messages.
    fn apply_unattended(&self, plan: &GcPlan) -> Vec<Result<String, String>> {
        plan.auto_applicable()
            .map(|f| apply::apply(f, &self.layout(), &self.grove_dir()))
            .collect()
    }
}

fn run_git(args: &[&str]) {
    let out = Command::new("git").args(args).output().unwrap();
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(["-C", dir.to_str().unwrap()])
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?} in {} failed: {}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn commit(dir: &Path, file: &str, message: &str) {
    std::fs::write(dir.join(file), message.as_bytes()).unwrap();
    git(dir, &["add", "."]);
    git(dir, &["commit", "-m", message]);
}

fn project(path: &Path, branch: &str, expires_at: Option<OffsetDateTime>) -> Project {
    Project {
        path: path.to_path_buf(),
        branch: branch.to_string(),
        base: "origin/main".to_string(),
        created: OffsetDateTime::now_utc(),
        issue: None,
        frozen: false,
        expires_at,
    }
}

fn labels(plan: &GcPlan, category: Category) -> Vec<String> {
    plan.in_category(category)
        .map(|f| f.label.clone())
        .collect()
}

fn find<'a>(plan: &'a GcPlan, category: Category, label: &str) -> &'a Finding {
    plan.in_category(category)
        .find(|f| f.label == label)
        .unwrap_or_else(|| {
            panic!(
                "no '{label}' in category {}; found {:?}",
                category.number(),
                labels(plan, category)
            )
        })
}

// ── category 1: stale registry entries ───────────────────────────────────────

#[test]
fn registry_entry_whose_directory_vanished_is_found_and_dropped() {
    let fx = Fixture::new();
    let wt = fx.worktree("wt-gone", "feature/gone");
    let mut projects = BTreeMap::new();
    projects.insert("wt-gone".to_string(), project(&wt, "feature/gone", None));
    projects.insert(
        "wt-kept".to_string(),
        project(
            &fx.worktree("wt-kept", "feature/kept"),
            "feature/kept",
            None,
        ),
    );
    fx.save_registry(projects);

    std::fs::remove_dir_all(&wt).unwrap();

    let plan = fx.plan(&GcOptions::for_tests());
    assert_eq!(labels(&plan, Category::StaleRegistryEntry), vec!["wt-gone"]);
    assert!(find(&plan, Category::StaleRegistryEntry, "wt-gone").is_auto_applicable());

    let results = fx.apply_unattended(&plan);
    assert!(results.iter().all(Result::is_ok), "{results:?}");

    let registry = fx.registry();
    assert!(!registry.projects.contains_key("wt-gone"));
    assert!(
        registry.projects.contains_key("wt-kept"),
        "a live project must survive the sweep"
    );
}

// ── category 2: unregistered worktrees ───────────────────────────────────────

#[test]
fn unregistered_worktree_is_reported_and_never_auto_applied() {
    let fx = Fixture::new();
    fx.worktree("wt-orphan", "feature/orphan");
    fx.save_registry(BTreeMap::new());

    let plan = fx.plan(&GcOptions::for_tests());
    let finding = find(&plan, Category::UnregisteredWorktree, "wt-orphan");
    assert!(matches!(finding.remedy, Remedy::AdoptOrDone { .. }));
    assert!(
        !finding.is_auto_applicable(),
        "--yes must never remove an unregistered worktree"
    );
    assert!(
        finding.details.iter().any(|d| d.contains("feature/orphan")),
        "{:?}",
        finding.details
    );

    let results = fx.apply_unattended(&plan);
    assert!(results.is_empty(), "nothing here is auto-applicable");
    assert!(fx.work_dir.join("wt-orphan").exists());
}

#[test]
fn a_registered_worktree_is_not_reported_as_unregistered() {
    let fx = Fixture::new();
    let wt = fx.worktree("wt-known", "feature/known");
    let mut projects = BTreeMap::new();
    projects.insert("wt-known".to_string(), project(&wt, "feature/known", None));
    fx.save_registry(projects);

    let plan = fx.plan(&GcOptions::for_tests());
    assert!(labels(&plan, Category::UnregisteredWorktree).is_empty());
}

#[test]
fn dirty_unregistered_worktree_carries_its_blocker() {
    let fx = Fixture::new();
    let wt = fx.worktree("wt-dirty", "feature/dirty");
    std::fs::write(wt.join("notes.md"), b"unsaved thinking").unwrap();
    fx.save_registry(BTreeMap::new());

    let plan = fx.plan(&GcOptions::for_tests());
    let finding = find(&plan, Category::UnregisteredWorktree, "wt-dirty");
    assert!(
        finding.blockers.iter().any(|b| b.contains("uncommitted")),
        "untracked-only dirt must block: {:?}",
        finding.blockers
    );
}

// ── category 3: foreign directories ──────────────────────────────────────────

#[test]
fn foreign_directories_are_listed_and_never_touched() {
    let fx = Fixture::new();
    fx.worktree("wt-real", "feature/real");
    std::fs::create_dir_all(fx.work_dir.join("support-articles/docs")).unwrap();
    std::fs::write(fx.work_dir.join("support-articles/a.md"), b"notes").unwrap();
    run_git(&[
        "clone",
        fx.main_repo.to_str().unwrap(),
        fx.work_dir.join("shared-clone").to_str().unwrap(),
    ]);
    fx.save_registry(BTreeMap::new());

    let plan = fx.plan(&GcOptions::for_tests());
    let found = labels(&plan, Category::ForeignDirectory);
    assert!(found.contains(&"support-articles".to_string()), "{found:?}");
    assert!(found.contains(&"shared-clone".to_string()), "{found:?}");
    assert!(
        !found.contains(&"master".to_string()),
        "the main repo is not a foreign directory: {found:?}"
    );
    assert!(
        !found.contains(&"wt-real".to_string()),
        "a worktree is not a foreign directory: {found:?}"
    );
    assert!(
        plan.in_category(Category::ForeignDirectory)
            .all(|f| !f.is_actionable()),
        "category 3 is report-only"
    );

    let clone = find(&plan, Category::ForeignDirectory, "shared-clone");
    assert!(
        clone
            .details
            .iter()
            .any(|d| d.contains("standalone git clone")),
        "{:?}",
        clone.details
    );
}

#[test]
fn dot_directories_are_left_out_of_the_foreign_scan() {
    let fx = Fixture::new();
    std::fs::create_dir_all(fx.work_dir.join(".scratch/probe")).unwrap();
    std::fs::create_dir_all(fx.work_dir.join(".archive")).unwrap();
    fx.save_registry(BTreeMap::new());

    let plan = fx.plan(&GcOptions::for_tests());
    let found = labels(&plan, Category::ForeignDirectory);
    for name in [".scratch", ".archive", ".grove"] {
        assert!(
            !found.contains(&name.to_string()),
            "{name} must not be reported as a foreign directory: {found:?}"
        );
    }
}

// ── category 4: expired ephemerals ───────────────────────────────────────────

#[test]
fn expired_clean_ephemeral_is_removed_and_its_branch_logged() {
    let fx = Fixture::new();
    let wt = fx.worktree_at(&fx.work_dir.join(".scratch/probe"), "scratch/probe");
    let head = String::from_utf8(
        Command::new("git")
            .args(["-C", wt.to_str().unwrap(), "rev-parse", "HEAD"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_string();

    let mut projects = BTreeMap::new();
    projects.insert(
        "probe".to_string(),
        project(
            &wt,
            "scratch/probe",
            Some(OffsetDateTime::now_utc() - Duration::days(1)),
        ),
    );
    fx.save_registry(projects);

    let plan = fx.plan(&GcOptions::for_tests());
    let finding = find(&plan, Category::ExpiredEphemeral, "probe");
    assert!(
        finding.is_auto_applicable(),
        "clean, pushed and idle: {:?}",
        finding.blockers
    );

    let results = fx.apply_unattended(&plan);
    assert!(results.iter().all(Result::is_ok), "{results:?}");

    assert!(!wt.exists(), "the worktree directory should be gone");
    assert!(!fx.registry().projects.contains_key("probe"));

    let log = std::fs::read_to_string(fx.layout().deleted_branches_log()).unwrap();
    assert!(
        log.contains(&format!("{} scratch/probe {head}", wt.display())),
        "branch deletion must be logged as DIR BRANCH SHA, got: {log}"
    );
}

#[test]
fn expired_but_dirty_ephemeral_is_reported_and_never_removed() {
    let fx = Fixture::new();
    let wt = fx.worktree_at(&fx.work_dir.join(".scratch/wip"), "scratch/wip");
    std::fs::write(wt.join("half-finished.rs"), b"fn main() {}").unwrap();

    let mut projects = BTreeMap::new();
    projects.insert(
        "wip".to_string(),
        project(
            &wt,
            "scratch/wip",
            Some(OffsetDateTime::now_utc() - Duration::days(30)),
        ),
    );
    fx.save_registry(projects);

    let plan = fx.plan(&GcOptions::for_tests());
    let finding = find(&plan, Category::ExpiredEphemeral, "wip");
    assert!(
        finding.blockers.iter().any(|b| b.contains("uncommitted")),
        "{:?}",
        finding.blockers
    );
    assert!(!finding.is_auto_applicable());

    let results = fx.apply_unattended(&plan);
    assert!(results.is_empty());
    assert!(wt.exists(), "expired-but-dirty must survive --yes");
    assert!(fx.registry().projects.contains_key("wip"));
}

#[test]
fn expired_ephemeral_with_unpushed_commits_is_blocked() {
    let fx = Fixture::new();
    let wt = fx.worktree_at(&fx.work_dir.join(".scratch/ahead"), "scratch/ahead");
    commit(&wt, "new.rs", "work that only exists here");

    let mut projects = BTreeMap::new();
    projects.insert(
        "ahead".to_string(),
        project(
            &wt,
            "scratch/ahead",
            Some(OffsetDateTime::now_utc() - Duration::days(1)),
        ),
    );
    fx.save_registry(projects);

    let plan = fx.plan(&GcOptions::for_tests());
    let finding = find(&plan, Category::ExpiredEphemeral, "ahead");
    assert!(
        finding
            .blockers
            .iter()
            .any(|b| b.contains("not present on any remote")),
        "{:?}",
        finding.blockers
    );
    assert!(wt.exists());
}

#[test]
fn unexpired_ephemeral_is_not_a_finding() {
    let fx = Fixture::new();
    let wt = fx.worktree_at(&fx.work_dir.join(".scratch/fresh"), "scratch/fresh");
    let mut projects = BTreeMap::new();
    projects.insert(
        "fresh".to_string(),
        project(
            &wt,
            "scratch/fresh",
            Some(OffsetDateTime::now_utc() + Duration::days(7)),
        ),
    );
    fx.save_registry(projects);

    let plan = fx.plan(&GcOptions::for_tests());
    assert!(
        labels(&plan, Category::ExpiredEphemeral).is_empty(),
        "a TTL that has not elapsed is not gc's business"
    );
}

#[test]
fn unregistered_scratch_clone_past_the_ttl_is_collectable() {
    let fx = Fixture::new();
    let scratch = fx.work_dir.join(".scratch");
    std::fs::create_dir_all(&scratch).unwrap();
    run_git(&[
        "clone",
        fx.main_repo.to_str().unwrap(),
        scratch.join("raw-clone").to_str().unwrap(),
    ]);
    fx.save_registry(BTreeMap::new());

    let plan = fx.plan(&GcOptions::for_tests());
    let finding = find(&plan, Category::ExpiredEphemeral, "raw-clone");
    assert!(
        finding
            .details
            .iter()
            .any(|d| d.contains("unregistered .scratch directory")),
        "{:?}",
        finding.details
    );
    assert!(finding.is_auto_applicable(), "{:?}", finding.blockers);

    let results = fx.apply_unattended(&plan);
    assert!(results.iter().all(Result::is_ok), "{results:?}");
    assert!(!scratch.join("raw-clone").exists());
}

#[test]
fn non_git_scratch_directory_is_reported_but_not_removed() {
    let fx = Fixture::new();
    let junk = fx.work_dir.join(".scratch/notes");
    std::fs::create_dir_all(&junk).unwrap();
    std::fs::write(junk.join("thoughts.md"), b"nothing to see").unwrap();
    fx.save_registry(BTreeMap::new());

    let plan = fx.plan(&GcOptions::for_tests());
    let finding = find(&plan, Category::ExpiredEphemeral, "notes");
    assert!(
        finding.blockers.iter().any(|b| b.contains("non-git")),
        "{:?}",
        finding.blockers
    );
    assert!(junk.exists());
}

// ── category 5: harness worktrees ────────────────────────────────────────────

#[test]
fn stale_harness_worktree_is_removed_and_not_confused_with_category_two() {
    let fx = Fixture::new();
    let harness = fx.layout().harness_dir().join("agent-a139da4b436e8c126");
    fx.worktree_at(&harness, "agent/a139da4b436e8c126");
    fx.save_registry(BTreeMap::new());

    let plan = fx.plan(&GcOptions::for_tests());
    assert!(
        labels(&plan, Category::UnregisteredWorktree).is_empty(),
        "harness worktrees belong to category 5, not 2: {:?}",
        labels(&plan, Category::UnregisteredWorktree)
    );
    let finding = find(&plan, Category::HarnessWorktree, "agent-a139da4b436e8c126");
    assert!(finding.is_auto_applicable(), "{:?}", finding.blockers);

    let results = fx.apply_unattended(&plan);
    assert!(results.iter().all(Result::is_ok), "{results:?}");
    assert!(!harness.exists());
}

#[test]
fn fresh_harness_worktree_is_left_for_its_live_session() {
    let fx = Fixture::new();
    let harness = fx.layout().harness_dir().join("agent-ad56e26e5abdfbcf0");
    fx.worktree_at(&harness, "agent/ad56e26e5abdfbcf0");
    fx.save_registry(BTreeMap::new());

    let opts = GcOptions {
        harness_stale_after: Duration::hours(48),
        ..GcOptions::for_tests()
    };
    let plan = fx.plan(&opts);
    let finding = find(&plan, Category::HarnessWorktree, "agent-ad56e26e5abdfbcf0");
    assert!(
        finding.blockers.iter().any(|b| b.contains("still fresh")),
        "{:?}",
        finding.blockers
    );
    assert!(!finding.is_auto_applicable());
    assert!(harness.exists());
}

#[test]
fn a_harness_worktree_in_use_survives_even_when_stale() {
    let fx = Fixture::new();
    let harness = fx.layout().harness_dir().join("agent-busy");
    fx.worktree_at(&harness, "agent/busy");
    fx.save_registry(BTreeMap::new());

    let probes = ProbeContext {
        process_cwds: vec![(31337, harness.join("src"))],
        ..Default::default()
    };
    let plan = fx.plan_with_probes(&GcOptions::for_tests(), &probes);
    let finding = find(&plan, Category::HarnessWorktree, "agent-busy");
    assert!(
        finding.blockers.iter().any(|b| b.contains("in use")),
        "a live process cwd must block removal: {:?}",
        finding.blockers
    );

    let results = fx.apply_unattended(&plan);
    assert!(results.is_empty());
    assert!(harness.exists());
}

// ── category 6: prunable metadata ────────────────────────────────────────────

#[test]
fn prunable_worktree_metadata_is_found_and_pruned() {
    let fx = Fixture::new();
    let wt = fx.worktree("wt-vanished", "feature/vanished");
    fx.save_registry(BTreeMap::new());
    std::fs::remove_dir_all(&wt).unwrap();

    let plan = fx.plan(&GcOptions::for_tests());
    let finding = find(&plan, Category::PrunableMetadata, "git worktree prune");
    assert!(finding.is_auto_applicable());
    assert!(
        finding.details.iter().any(|d| d.contains("wt-vanished")),
        "the prune reason should name the entry: {:?}",
        finding.details
    );

    let results = fx.apply_unattended(&plan);
    assert!(results.iter().all(Result::is_ok), "{results:?}");

    let after = fx.plan(&GcOptions::for_tests());
    assert!(
        labels(&after, Category::PrunableMetadata).is_empty(),
        "a second run should find nothing left to prune"
    );
}

// ── category 7: merged projects ──────────────────────────────────────────────

#[test]
fn merged_and_clean_project_is_listed_as_a_done_candidate() {
    let fx = Fixture::new();
    let wt = fx.worktree("wt-landed", "feature/landed");
    let mut projects = BTreeMap::new();
    projects.insert(
        "wt-landed".to_string(),
        project(&wt, "feature/landed", None),
    );
    fx.save_registry(projects);

    let plan = fx.plan(&GcOptions::for_tests());
    let finding = find(&plan, Category::MergedProject, "wt-landed");
    assert!(
        !finding.is_actionable(),
        "category 7 lists candidates; it never removes them"
    );
    assert!(
        finding
            .details
            .iter()
            .any(|d| d.contains("grove done wt-landed")),
        "the report should hand over the exact command: {:?}",
        finding.details
    );
}

#[test]
fn project_with_work_the_base_lacks_is_not_a_done_candidate() {
    let fx = Fixture::new();
    let wt = fx.worktree("wt-ahead", "feature/ahead");
    commit(&wt, "feature.rs", "a change nobody upstream has");
    let mut projects = BTreeMap::new();
    projects.insert("wt-ahead".to_string(), project(&wt, "feature/ahead", None));
    fx.save_registry(projects);

    let plan = fx.plan(&GcOptions::for_tests());
    assert!(
        labels(&plan, Category::MergedProject).is_empty(),
        "unmerged work must not be advertised as removable"
    );
}

#[test]
fn merged_work_that_landed_under_a_new_sha_is_still_recognised() {
    // Mergify rebases, so the landed commit is not an ancestor of anything in
    // the worktree. Patch-id matching is what has to catch this.
    let fx = Fixture::new();
    let wt = fx.worktree("wt-rebased", "feature/rebased");
    commit(&wt, "shared.rs", "the change that landed");

    // Replay the same change onto main and push it, giving it a new SHA.
    git(&fx.main_repo, &["cherry-pick", "feature/rebased"]);
    git(
        &fx.main_repo,
        &["commit", "--amend", "--no-edit", "--date=now"],
    );
    git(&fx.main_repo, &["push", "origin", "main"]);
    git(&fx.main_repo, &["fetch", "origin"]);

    let mut projects = BTreeMap::new();
    projects.insert(
        "wt-rebased".to_string(),
        project(&wt, "feature/rebased", None),
    );
    fx.save_registry(projects);

    let plan = fx.plan(&GcOptions::for_tests());
    let finding = find(&plan, Category::MergedProject, "wt-rebased");
    assert!(
        finding.details.iter().any(|d| d.contains("patch-id")),
        "a rebased-but-identical change is absorbed by patch-id: {:?}",
        finding.details
    );
}

#[test]
fn a_long_rebased_branch_is_still_recognised_as_merged() {
    // The shape that made the audit necessary: a branch whose commits all
    // landed upstream under new SHAs, so `rev-list --count base..HEAD` reads
    // high while every patch-id is in fact already in the base. Gating the
    // patch-id comparison on that count would refuse to answer here, which is
    // exactly the case worth answering.
    let fx = Fixture::new();
    let wt = fx.worktree("wt-long", "feature/long");
    for n in 1..=5 {
        commit(&wt, &format!("change-{n}.rs"), &format!("change {n}"));
    }

    // Diverge main first so the replayed commits cannot reuse their SHAs.
    commit(&fx.main_repo, "unrelated.rs", "an unrelated upstream change");
    git(
        &fx.main_repo,
        &["cherry-pick", "feature/long~5..feature/long"],
    );
    git(&fx.main_repo, &["push", "origin", "main"]);
    git(&fx.main_repo, &["fetch", "origin"]);

    let mut projects = BTreeMap::new();
    projects.insert("wt-long".to_string(), project(&wt, "feature/long", None));
    fx.save_registry(projects);

    let opts = GcOptions {
        // Nothing may fall back to subject matching: patch-id has to carry it.
        max_subject_probe_commits: 0,
        ..GcOptions::for_tests()
    };
    let plan = fx.plan(&opts);
    let finding = find(&plan, Category::MergedProject, "wt-long");
    assert!(
        finding.details.iter().any(|d| d.contains("patch-id")),
        "{:?}",
        finding.details
    );
}

#[test]
fn dirty_project_is_not_a_done_candidate_however_merged() {
    let fx = Fixture::new();
    let wt = fx.worktree("wt-busy", "feature/busy");
    std::fs::write(wt.join("scratch.txt"), b"mid-edit").unwrap();
    let mut projects = BTreeMap::new();
    projects.insert("wt-busy".to_string(), project(&wt, "feature/busy", None));
    fx.save_registry(projects);

    let plan = fx.plan(&GcOptions::for_tests());
    assert!(labels(&plan, Category::MergedProject).is_empty());
}

// ── whole-run behaviour ──────────────────────────────────────────────────────

#[test]
fn yes_touches_only_the_four_bookkeeping_categories() {
    let fx = Fixture::new();

    // One finding in each of the eight categories, as far as a fixture allows.
    let gone = fx.worktree("wt-gone", "feature/gone");
    let orphan = fx.worktree("wt-orphan", "feature/orphan");
    let landed = fx.worktree("wt-landed", "feature/landed");
    let ephemeral = fx.worktree_at(&fx.work_dir.join(".scratch/probe"), "scratch/probe");
    let harness = fx.layout().harness_dir().join("agent-stale");
    fx.worktree_at(&harness, "agent/stale");
    let foreign = fx.work_dir.join("sc-support");
    std::fs::create_dir_all(&foreign).unwrap();
    let archive = fx.work_dir.join(".archive");
    std::fs::create_dir_all(&archive).unwrap();
    std::fs::write(archive.join("old-plan.md"), vec![b'x'; 4096]).unwrap();

    let mut projects = BTreeMap::new();
    projects.insert("wt-gone".to_string(), project(&gone, "feature/gone", None));
    projects.insert(
        "wt-landed".to_string(),
        project(&landed, "feature/landed", None),
    );
    projects.insert(
        "probe".to_string(),
        project(
            &ephemeral,
            "scratch/probe",
            Some(OffsetDateTime::now_utc() - Duration::days(1)),
        ),
    );
    fx.save_registry(projects);
    std::fs::remove_dir_all(&gone).unwrap();

    let plan = fx.plan(&GcOptions::for_tests());
    for category in [
        Category::StaleRegistryEntry,
        Category::UnregisteredWorktree,
        Category::ForeignDirectory,
        Category::ExpiredEphemeral,
        Category::HarnessWorktree,
        Category::PrunableMetadata,
        Category::MergedProject,
        Category::ArchiveContents,
    ] {
        assert!(
            plan.in_category(category).next().is_some(),
            "category {} produced no finding",
            category.number()
        );
    }

    let results = fx.apply_unattended(&plan);
    assert!(results.iter().all(Result::is_ok), "{results:?}");

    assert!(!fx.registry().projects.contains_key("wt-gone"), "1 applied");
    assert!(orphan.exists(), "2 must be left alone by --yes");
    assert!(foreign.exists(), "3 must be left alone");
    assert!(!ephemeral.exists(), "4 applied");
    assert!(!harness.exists(), "5 applied");
    assert!(landed.exists(), "7 must be left alone");
    assert!(archive.join("old-plan.md").exists(), "8 must be left alone");
    assert!(
        fx.registry().projects.contains_key("wt-landed"),
        "a done candidate keeps its registry entry"
    );
}

#[test]
fn a_second_run_after_applying_reports_the_advisory_categories_only() {
    let fx = Fixture::new();
    let gone = fx.worktree("wt-gone", "feature/gone");
    let mut projects = BTreeMap::new();
    projects.insert("wt-gone".to_string(), project(&gone, "feature/gone", None));
    fx.save_registry(projects);
    std::fs::remove_dir_all(&gone).unwrap();
    std::fs::create_dir_all(fx.work_dir.join("sc-support")).unwrap();

    let first = fx.plan(&GcOptions::for_tests());
    let results = fx.apply_unattended(&first);
    assert!(results.iter().all(Result::is_ok), "{results:?}");

    let second = fx.plan(&GcOptions::for_tests());
    assert!(
        second.auto_applicable().next().is_none(),
        "run should settle"
    );
    assert_eq!(
        labels(&second, Category::ForeignDirectory),
        vec!["sc-support"]
    );
}

#[test]
fn inspecting_a_tree_does_not_make_it_look_recently_used() {
    // `git status` normally refreshes the on-disk index, which bumps the very
    // mtime the liveness guard reads. Left unchecked, gc's own inspection would
    // mark every tree it looked at as a live session — and blocked findings look
    // exactly like correct caution, so nothing would ever be collected again.
    let fx = Fixture::new();
    let wt = fx.worktree("wt-quiet", "feature/quiet");

    let before = grove::gc::guards::last_filesystem_activity(&wt)
        .expect("a fresh worktree has a timestamp");
    std::thread::sleep(std::time::Duration::from_millis(1100));
    grove::gc::guards::inspect_tree(&wt).expect("inspection should succeed");
    let after = grove::gc::guards::last_filesystem_activity(&wt).unwrap();

    assert_eq!(
        before, after,
        "inspecting a tree must leave its activity timestamp alone"
    );
}

#[test]
fn liveness_uses_the_timestamp_it_was_given_not_a_fresh_reading() {
    // The other half of the same defence: the scanners sample activity before
    // touching a tree and hand it to the guard, so even a git command that did
    // write could not fake a live session.
    let fx = Fixture::new();
    let wt = fx.worktree("wt-old", "feature/old");
    let long_ago = OffsetDateTime::now_utc() - Duration::days(30);

    let liveness = grove::gc::guards::check_liveness(
        &wt,
        OffsetDateTime::now_utc(),
        Duration::hours(48),
        &ProbeContext::empty(),
        Some(long_ago),
    );
    assert!(
        !liveness.reasons.iter().any(|r| r.starts_with("modified")),
        "a just-created directory reported as 30 days old must not read as modified: {:?}",
        liveness.reasons
    );
}

#[test]
fn scanning_survives_a_worktree_it_cannot_read() {
    let fx = Fixture::new();
    let mut projects = BTreeMap::new();
    projects.insert(
        "broken".to_string(),
        project(&fx.work_dir.join("broken"), "feature/broken", None),
    );
    // A registered path that exists but is not a git tree at all.
    std::fs::create_dir_all(fx.work_dir.join("broken")).unwrap();
    fx.save_registry(projects);

    let plan = fx.plan(&GcOptions::for_tests());
    assert!(
        !plan.warnings.is_empty(),
        "an unreadable project should be warned about, not fatal"
    );
    assert!(
        labels(&plan, Category::MergedProject).is_empty(),
        "and it must not be advertised as removable"
    );
}
