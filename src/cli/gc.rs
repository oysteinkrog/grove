//! `grove gc` — audit the work_dir, and fix what is safe to fix.
//!
//! The command surface is deliberately small: `--dry-run` to look, `--yes` to
//! apply the four bookkeeping categories unattended, and neither to be asked
//! item by item. There is no `--force`; see [`crate::gc`] for the category
//! table and what each one permits.

use std::io::{BufRead, IsTerminal, Write};
use std::path::PathBuf;

use crate::gc::guards::ProbeContext;
use crate::gc::report::{self, Mode};
use crate::gc::scan::Scanner;
use crate::gc::{Category, Finding, GcOptions, GcPlan, Layout, Remedy, apply};
use crate::registry::Registry;
use crate::repo::RepoContext;

pub struct GcArgs {
    /// Report and change nothing.
    pub dry_run: bool,
    /// Apply categories 1, 4, 5 and 6 without asking.
    pub yes: bool,
}

pub fn run(args: &GcArgs, cx: &RepoContext) -> anyhow::Result<()> {
    let layout = Layout {
        work_dir: cx.resolved.work_dir.clone(),
        main_repo: cx.resolved.main_repo.clone(),
    };
    let opts = GcOptions::default();

    let interactive = std::io::stdin().is_terminal();
    let mode = if args.dry_run {
        Mode::DryRun
    } else if args.yes {
        Mode::Unattended
    } else if interactive {
        Mode::Interactive
    } else {
        Mode::NonInteractive
    };

    eprintln!("[gc] scanning {}", layout.work_dir.display());
    let probes = ProbeContext::gather(&layout.work_dir);
    let scanner = Scanner {
        layout: &layout,
        registry: &cx.registry,
        opts: &opts,
        probes: &probes,
    };
    let plan = scanner.scan();

    print!("{}", report::render(&plan, mode, &layout));

    if !mode.applies_changes() {
        return Ok(());
    }

    let outcome = match mode {
        Mode::Unattended => apply_unattended(&plan, &layout, cx),
        Mode::Interactive => apply_interactively(&plan, &layout, cx)?,
        _ => Outcome::default(),
    };
    outcome.print();
    Ok(())
}

#[derive(Default)]
struct Outcome {
    applied: Vec<String>,
    failed: Vec<String>,
    skipped: usize,
}

impl Outcome {
    /// Record one apply attempt. A failure never stops the run: a locked
    /// directory or a worktree somebody re-entered should cost that one item,
    /// not the other twenty.
    fn record(&mut self, label: &str, result: Result<String, String>) {
        match result {
            Ok(message) => self.applied.push(format!("{label}: {message}")),
            Err(e) => self.failed.push(format!("{label}: {e}")),
        }
    }

    fn print(&self) {
        println!();
        if self.applied.is_empty() && self.failed.is_empty() {
            println!("No changes made.");
            return;
        }
        if !self.applied.is_empty() {
            println!("Applied ({})", self.applied.len());
            for line in &self.applied {
                println!("    {line}");
            }
        }
        if !self.failed.is_empty() {
            println!("Failed ({})", self.failed.len());
            for line in &self.failed {
                println!("    {line}");
            }
        }
        if self.skipped > 0 {
            println!("Skipped {} item(s).", self.skipped);
        }
    }
}

fn apply_unattended(plan: &GcPlan, layout: &Layout, cx: &RepoContext) -> Outcome {
    let grove_dir = cx.grove_dir();
    let mut outcome = Outcome::default();
    for finding in plan.auto_applicable() {
        outcome.record(&finding.label, apply::apply(finding, layout, &grove_dir));
    }
    outcome
}

fn apply_interactively(
    plan: &GcPlan,
    layout: &Layout,
    cx: &RepoContext,
) -> anyhow::Result<Outcome> {
    let grove_dir = cx.grove_dir();
    let mut outcome = Outcome::default();
    let stdin = std::io::stdin();
    let mut reader = stdin.lock();

    for finding in plan.findings.iter().filter(|f| f.is_actionable()) {
        match finding.category {
            Category::UnregisteredWorktree => {
                match prompt(&mut reader, &adopt_or_done_question(finding), "s")? {
                    Answer::Adopt => {
                        let default_tag = finding.label.clone();
                        let tag = prompt_line(
                            &mut reader,
                            &format!("      tag [{default_tag}]: "),
                            &default_tag,
                        )?;
                        outcome.record(&finding.label, adopt(cx, &tag, &finding.path));
                    }
                    Answer::Remove => {
                        outcome.record(&finding.label, apply::apply(finding, layout, &grove_dir))
                    }
                    Answer::Skip => outcome.skipped += 1,
                    Answer::Quit => break,
                }
            }
            category if category.auto_applicable() => {
                match prompt(&mut reader, &apply_question(finding), "n")? {
                    Answer::Remove => {
                        outcome.record(&finding.label, apply::apply(finding, layout, &grove_dir))
                    }
                    Answer::Skip => outcome.skipped += 1,
                    Answer::Quit => break,
                    Answer::Adopt => outcome.skipped += 1,
                }
            }
            _ => {}
        }
    }
    Ok(outcome)
}

fn apply_question(finding: &Finding) -> String {
    let what = match &finding.remedy {
        Remedy::DropRegistryEntry { tag } => format!("drop registry entry '{tag}'"),
        Remedy::RemoveEphemeral { .. } => format!("remove {}", finding.path.display()),
        Remedy::RemoveHarnessWorktree { .. } => format!("remove {}", finding.path.display()),
        Remedy::PruneWorktreeMetadata => "prune stale worktree metadata".to_string(),
        Remedy::AdoptOrDone { .. } | Remedy::ReportOnly => finding.label.clone(),
    };
    format!("[{}] {what}? [y/N/q] ", finding.category.number())
}

fn adopt_or_done_question(finding: &Finding) -> String {
    format!(
        "[2] {} is not registered — [a]dopt, [d]elete, [s]kip, [q]uit? [s] ",
        finding.path.display()
    )
}

enum Answer {
    Adopt,
    Remove,
    Skip,
    Quit,
}

fn prompt(reader: &mut impl BufRead, question: &str, default: &str) -> anyhow::Result<Answer> {
    let raw = prompt_line(reader, question, default)?;
    Ok(match raw.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" | "d" | "delete" => Answer::Remove,
        "a" | "adopt" => Answer::Adopt,
        "q" | "quit" => Answer::Quit,
        _ => Answer::Skip,
    })
}

fn prompt_line(reader: &mut impl BufRead, question: &str, default: &str) -> anyhow::Result<String> {
    print!("{question}");
    std::io::stdout().flush()?;
    let mut line = String::new();
    // EOF mid-prompt means the terminal went away; treat it as the default
    // rather than looping on an empty read.
    if reader.read_line(&mut line)? == 0 {
        return Ok(default.to_string());
    }
    let trimmed = line.trim();
    Ok(if trimmed.is_empty() {
        default.to_string()
    } else {
        trimmed.to_string()
    })
}

/// Register an existing worktree, reloading the registry first so the entry
/// lands on top of any changes earlier answers in this same run already made.
fn adopt(cx: &RepoContext, tag: &str, path: &std::path::Path) -> Result<String, String> {
    let registry = Registry::load(&cx.grove_dir()).map_err(|e| e.to_string())?;
    let fresh = RepoContext {
        id: cx.id.clone(),
        global: cx.global.clone(),
        resolved: cx.resolved.clone(),
        registry,
    };
    let args = super::adopt::AdoptArgs {
        tag: tag.to_string(),
        path: PathBuf::from(path),
        issue: None,
        base: None,
        mv: false,
    };
    super::adopt::run(&args, &fresh)
        .map(|()| format!("adopted as '{tag}'"))
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    fn finding(category: Category, remedy: Remedy) -> Finding {
        Finding::new(category, "wt-a", "/c/work/desktop/wt-a").remedy(remedy)
    }

    #[test]
    fn empty_answer_takes_the_default() {
        let mut reader = Cursor::new(b"\n".to_vec());
        let answer = prompt_line(&mut reader, "tag [wt-a]: ", "wt-a").unwrap();
        assert_eq!(answer, "wt-a");
    }

    #[test]
    fn eof_takes_the_default_instead_of_spinning() {
        let mut reader = Cursor::new(Vec::new());
        let answer = prompt_line(&mut reader, "tag: ", "fallback").unwrap();
        assert_eq!(answer, "fallback");
    }

    #[test]
    fn yes_and_delete_both_mean_remove() {
        for input in ["y\n", "yes\n", "d\n", "delete\n", "Y\n"] {
            let mut reader = Cursor::new(input.as_bytes().to_vec());
            assert!(
                matches!(prompt(&mut reader, "?", "n").unwrap(), Answer::Remove),
                "input {input:?} should mean remove"
            );
        }
    }

    #[test]
    fn anything_unrecognised_means_skip() {
        for input in ["\n", "no\n", "wat\n"] {
            let mut reader = Cursor::new(input.as_bytes().to_vec());
            assert!(matches!(
                prompt(&mut reader, "?", "n").unwrap(),
                Answer::Skip
            ));
        }
    }

    #[test]
    fn quit_is_distinct_from_skip() {
        let mut reader = Cursor::new(b"q\n".to_vec());
        assert!(matches!(
            prompt(&mut reader, "?", "n").unwrap(),
            Answer::Quit
        ));
    }

    #[test]
    fn questions_name_the_category_and_the_action() {
        let drop = finding(
            Category::StaleRegistryEntry,
            Remedy::DropRegistryEntry {
                tag: "wt-a".to_string(),
            },
        );
        assert!(apply_question(&drop).starts_with("[1] drop registry entry 'wt-a'?"));

        let prune = finding(Category::PrunableMetadata, Remedy::PruneWorktreeMetadata);
        assert!(apply_question(&prune).contains("prune stale worktree metadata"));

        let unregistered = finding(
            Category::UnregisteredWorktree,
            Remedy::AdoptOrDone {
                branch: None,
                head: None,
            },
        );
        let question = adopt_or_done_question(&unregistered);
        assert!(question.contains("[a]dopt"));
        assert!(question.contains("/c/work/desktop/wt-a"));
    }

    #[test]
    fn outcome_separates_successes_from_failures() {
        let mut outcome = Outcome::default();
        outcome.record("wt-a", Ok("removed".to_string()));
        outcome.record("wt-b", Err("locked".to_string()));
        outcome.skipped = 2;
        assert_eq!(outcome.applied, vec!["wt-a: removed"]);
        assert_eq!(outcome.failed, vec!["wt-b: locked"]);
    }
}
