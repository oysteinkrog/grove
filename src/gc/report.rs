//! Rendering for `grove gc`.
//!
//! The report is plain text rather than a table: paths and evidence lines are
//! long and variable, and the output is meant to be readable in a scrollback
//! and pasteable into a bead or a PR.

use std::fmt::Write as _;

use crate::display::dim;

use super::{ALL_CATEGORIES, Category, GcPlan, Layout};

/// What the run is authorised to do, as told to the reader up front.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// `--dry-run`: report and change nothing.
    DryRun,
    /// No terminal on stdin and no `--yes`: report and change nothing.
    NonInteractive,
    /// `--yes`: apply categories 1, 4, 5 and 6 without asking.
    Unattended,
    /// A terminal is present: ask before each change.
    Interactive,
}

impl Mode {
    pub fn banner(self) -> &'static str {
        match self {
            Self::DryRun => "dry run — nothing will be changed",
            Self::NonInteractive => {
                "report only — stdin is not a terminal, so nothing will be changed"
            }
            Self::Unattended => "--yes — categories 1, 4, 5 and 6 will be applied without asking",
            Self::Interactive => "interactive — you will be asked before each change",
        }
    }

    pub fn applies_changes(self) -> bool {
        matches!(self, Self::Unattended | Self::Interactive)
    }
}

/// The whole report as one string.
///
/// Composed from the same pieces the streaming path prints one at a time, so
/// the two cannot drift: a category rendered mid-scan reads exactly as it would
/// at the end.
pub fn render(plan: &GcPlan, mode: Mode, layout: &Layout) -> String {
    let mut out = render_header(mode, layout);
    for category in ALL_CATEGORIES {
        out.push_str(&render_category(plan, category));
    }
    out.push_str(&render_warnings(plan));
    let _ = writeln!(out, "{}", summary(plan, mode));
    out
}

/// The two lines that name the work_dir and say what the run may change.
pub fn render_header(mode: Mode, layout: &Layout) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "grove gc — {}", layout.work_dir.display());
    let _ = writeln!(out, "{}", dim(mode.banner()));
    let _ = writeln!(out);
    out
}

/// One category's heading, policy line and findings.
///
/// Printed as soon as its scan finishes rather than held to the end. A scan
/// over a real work_dir spends minutes per category, and a report that arrives
/// only once every category is done is indistinguishable from a hang: the run
/// this was written for was killed at five minutes having shown nothing.
pub fn render_category(plan: &GcPlan, category: Category) -> String {
    let mut out = String::new();
    let findings: Vec<_> = plan.in_category(category).collect();
    let _ = writeln!(
        out,
        "[{}] {} ({})",
        category.number(),
        category.title(),
        findings.len()
    );
    let _ = writeln!(out, "    {}", dim(category.policy()));
    if findings.is_empty() {
        let _ = writeln!(out, "    {}", dim("nothing found"));
        let _ = writeln!(out);
        return out;
    }
    for finding in findings {
        let _ = writeln!(out, "    {}", finding.label);
        let _ = writeln!(out, "      {}", dim(finding.path.display().to_string()));
        for detail in &finding.details {
            let _ = writeln!(out, "      {}", dim(detail));
        }
        for blocker in &finding.blockers {
            let _ = writeln!(out, "      blocked: {blocker}");
        }
    }
    let _ = writeln!(out);
    out
}

/// Whatever probes could not answer, or the empty string when all of them did.
pub fn render_warnings(plan: &GcPlan) -> String {
    let mut out = String::new();
    if plan.warnings.is_empty() {
        return out;
    }
    let _ = writeln!(out, "Warnings ({})", plan.warnings.len());
    for warning in &plan.warnings {
        let _ = writeln!(out, "    {warning}");
    }
    let _ = writeln!(out);
    out
}

pub fn summary(plan: &GcPlan, mode: Mode) -> String {
    let total = plan.findings.len();
    let auto = plan.auto_applicable().count();
    let blocked = plan
        .findings
        .iter()
        .filter(|f| !f.blockers.is_empty())
        .count();

    if total == 0 {
        return "Nothing to report: the work_dir is clean.".to_string();
    }

    let verb = if mode.applies_changes() {
        "can be applied"
    } else {
        "would be applied with --yes"
    };
    format!("{total} findings · {auto} {verb} · {blocked} blocked by a guard")
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::super::{Finding, Remedy};
    use super::*;

    fn layout() -> Layout {
        Layout {
            work_dir: PathBuf::from("/c/work/desktop"),
            main_repo: PathBuf::from("/c/work/desktop/master"),
        }
    }

    #[test]
    fn empty_plan_lists_all_eight_categories_as_clean() {
        let plan = GcPlan::default();
        let text = render(&plan, Mode::DryRun, &layout());
        for category in ALL_CATEGORIES {
            assert!(
                text.contains(&format!("[{}] {}", category.number(), category.title())),
                "category {} missing from the report",
                category.number()
            );
        }
        assert!(text.contains("Nothing to report"));
    }

    #[test]
    fn streaming_the_pieces_reproduces_the_whole_report() {
        // The streaming path in `cli::gc` prints header, then one category at a
        // time, then warnings and summary. Concatenated that must equal what
        // `render` produces, or a category read mid-scan differs from the same
        // category read at the end.
        let mut plan = GcPlan::default();
        plan.findings.push(
            Finding::new(Category::StaleRegistryEntry, "gone", "/c/work/desktop/gone")
                .detail("registered path does not exist")
                .remedy(Remedy::DropRegistryEntry {
                    tag: "gone".to_string(),
                }),
        );
        plan.warn("agent-mail reservations unavailable");

        let whole = render(&plan, Mode::DryRun, &layout());

        let mut streamed = render_header(Mode::DryRun, &layout());
        for category in ALL_CATEGORIES {
            streamed.push_str(&render_category(&plan, category));
        }
        streamed.push_str(&render_warnings(&plan));
        streamed.push_str(&format!("{}\n", summary(&plan, Mode::DryRun)));

        assert_eq!(whole, streamed);
    }

    #[test]
    fn a_category_renders_only_its_own_findings() {
        let mut plan = GcPlan::default();
        plan.findings.push(Finding::new(
            Category::StaleRegistryEntry,
            "in-category-1",
            "/c/work/desktop/one",
        ));
        plan.findings.push(Finding::new(
            Category::ArchiveContents,
            "in-category-8",
            "/c/work/desktop/.archive/eight",
        ));

        let first = render_category(&plan, Category::StaleRegistryEntry);
        assert!(first.contains("in-category-1"));
        assert!(
            !first.contains("in-category-8"),
            "category 1 leaked a category 8 finding: {first}"
        );
    }

    #[test]
    fn warnings_render_to_nothing_when_there_are_none() {
        assert_eq!(render_warnings(&GcPlan::default()), "");
    }

    #[test]
    fn findings_render_label_path_details_and_blockers() {
        let mut plan = GcPlan::default();
        plan.findings.push(
            Finding::new(
                Category::ExpiredEphemeral,
                "probe",
                "/c/work/desktop/.scratch/probe",
            )
            .detail("TTL elapsed 3d ago")
            .remedy(Remedy::RemoveEphemeral {
                tag: Some("probe".to_string()),
                branch: None,
                head: None,
            })
            .block(["uncommitted changes (2 entries)".to_string()]),
        );
        let text = render(&plan, Mode::DryRun, &layout());
        assert!(text.contains("probe"));
        assert!(text.contains("/c/work/desktop/.scratch/probe"));
        assert!(text.contains("TTL elapsed 3d ago"));
        assert!(text.contains("blocked: uncommitted changes (2 entries)"));
    }

    #[test]
    fn warnings_are_reported_separately() {
        let mut plan = GcPlan::default();
        plan.warn("agent-mail reservations unavailable");
        let text = render(&plan, Mode::NonInteractive, &layout());
        assert!(text.contains("Warnings (1)"));
        assert!(text.contains("agent-mail reservations unavailable"));
    }

    #[test]
    fn summary_counts_auto_applicable_and_blocked() {
        let mut plan = GcPlan::default();
        plan.findings.push(
            Finding::new(Category::StaleRegistryEntry, "gone", "/c/work/desktop/gone").remedy(
                Remedy::DropRegistryEntry {
                    tag: "gone".to_string(),
                },
            ),
        );
        plan.findings.push(
            Finding::new(
                Category::HarnessWorktree,
                "agent-x",
                "/c/work/desktop/master/.claude/worktrees/agent-x",
            )
            .remedy(Remedy::RemoveHarnessWorktree {
                branch: None,
                head: None,
            })
            .block(["in use: process 12 has its cwd inside the tree".to_string()]),
        );
        plan.findings.push(Finding::new(
            Category::ForeignDirectory,
            "sc-support",
            "/c/work/desktop/sc-support",
        ));

        let text = summary(&plan, Mode::DryRun);
        assert!(text.starts_with("3 findings"), "{text}");
        assert!(text.contains("1 would be applied with --yes"), "{text}");
        assert!(text.contains("1 blocked"), "{text}");
    }

    #[test]
    fn each_mode_states_what_it_will_do() {
        assert!(Mode::DryRun.banner().contains("nothing will be changed"));
        assert!(Mode::NonInteractive.banner().contains("not a terminal"));
        assert!(Mode::Unattended.banner().contains("1, 4, 5 and 6"));
        assert!(Mode::Interactive.banner().contains("asked"));
        assert!(!Mode::DryRun.applies_changes());
        assert!(!Mode::NonInteractive.applies_changes());
        assert!(Mode::Unattended.applies_changes());
        assert!(Mode::Interactive.applies_changes());
    }
}
