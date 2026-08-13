//! Category 8: what is sitting in `.archive`.
//!
//! `.archive` is where retired patches, plans and logs go. It is reported —
//! never touched — so it stays visible instead of quietly becoming the next
//! sprawl the work_dir has to be rescued from.

use std::path::Path;

use super::guards;
use super::scan::dir_label;
use super::{Category, Finding, GcOptions, GcPlan, Layout};

/// Stop walking a directory tree after this many entries. `.archive` should be
/// small; a runaway tree must not turn a report into a filesystem crawl.
const MAX_WALK_ENTRIES: usize = 50_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DirSize {
    pub bytes: u64,
    pub entries: usize,
    /// The walk hit [`MAX_WALK_ENTRIES`] and the numbers are a lower bound.
    pub truncated: bool,
}

/// Total size of a file, or of a directory tree, without following symlinks.
pub fn measure(path: &Path) -> DirSize {
    let mut size = DirSize {
        bytes: 0,
        entries: 0,
        truncated: false,
    };
    let mut stack = vec![path.to_path_buf()];

    while let Some(current) = stack.pop() {
        let Ok(meta) = std::fs::symlink_metadata(&current) else {
            continue;
        };
        if meta.is_file() {
            size.bytes += meta.len();
            size.entries += 1;
        } else if meta.is_dir() {
            size.entries += 1;
            let Ok(entries) = std::fs::read_dir(&current) else {
                continue;
            };
            for entry in entries.flatten() {
                if size.entries + stack.len() >= MAX_WALK_ENTRIES {
                    size.truncated = true;
                    break;
                }
                stack.push(entry.path());
            }
        }
    }
    size
}

/// Render a byte count the way a human reads it: 3 significant-ish digits and
/// a binary unit.
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else if value < 10.0 {
        format!("{value:.1} {}", UNITS[unit])
    } else {
        format!("{value:.0} {}", UNITS[unit])
    }
}

pub(super) fn scan(layout: &Layout, opts: &GcOptions, plan: &mut GcPlan) {
    let archive = layout.archive_dir();
    if !archive.is_dir() {
        return;
    }
    let entries = match std::fs::read_dir(&archive) {
        Ok(entries) => entries,
        Err(e) => {
            plan.warn(format!(
                "could not read {} ({e}); category 8 is empty",
                archive.display()
            ));
            return;
        }
    };

    let mut paths: Vec<std::path::PathBuf> = entries.flatten().map(|e| e.path()).collect();
    paths.sort();

    let mut total_bytes = 0u64;
    let mut count = 0usize;
    for path in paths {
        // gc writes the branch-deletion log itself; listing it as archive
        // clutter would be reporting its own bookkeeping back at the operator.
        if path == layout.deleted_branches_log() {
            continue;
        }
        let size = measure(&path);
        total_bytes += size.bytes;
        count += 1;

        let mut finding = Finding::new(Category::ArchiveContents, dir_label(&path), &path)
            .detail(human_bytes(size.bytes));
        if let Some(touched) = guards::last_filesystem_activity(&path) {
            finding = finding.detail(format!(
                "last touched {}",
                guards::humanize_age(opts.now - touched)
            ));
        }
        if size.truncated {
            finding = finding.detail("size is a lower bound; tree too large to walk fully");
        }
        plan.findings.push(finding);
    }

    if count > 0 {
        plan.findings.push(
            Finding::new(Category::ArchiveContents, "(total)", &archive)
                .detail(format!("{count} entries, {}", human_bytes(total_bytes))),
        );
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn measures_a_single_file() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, vec![b'x'; 1234]).unwrap();
        let size = measure(&file);
        assert_eq!(size.bytes, 1234);
        assert_eq!(size.entries, 1);
        assert!(!size.truncated);
    }

    #[test]
    fn measures_a_nested_tree() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("a/b")).unwrap();
        std::fs::write(dir.path().join("a/one.txt"), vec![b'x'; 100]).unwrap();
        std::fs::write(dir.path().join("a/b/two.txt"), vec![b'x'; 250]).unwrap();
        let size = measure(&dir.path().join("a"));
        assert_eq!(size.bytes, 350);
    }

    #[test]
    fn missing_path_measures_as_empty() {
        let size = measure(Path::new("/tmp/grove-gc-no-such-path-xyz"));
        assert_eq!(size.bytes, 0);
        assert_eq!(size.entries, 0);
    }

    #[test]
    fn byte_counts_read_naturally() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(2048), "2.0 KB");
        assert_eq!(human_bytes(45_606), "45 KB");
        assert_eq!(human_bytes(5 * 1024 * 1024), "5.0 MB");
    }

    #[test]
    fn archive_scan_reports_entries_and_a_total() {
        let dir = TempDir::new().unwrap();
        let layout = Layout {
            work_dir: dir.path().to_path_buf(),
            main_repo: dir.path().join("master"),
        };
        let archive = layout.archive_dir();
        std::fs::create_dir_all(&archive).unwrap();
        std::fs::write(archive.join("old-plan.md"), vec![b'x'; 2048]).unwrap();
        std::fs::write(archive.join("patch.diff"), vec![b'x'; 1024]).unwrap();

        let mut plan = GcPlan::default();
        scan(&layout, &GcOptions::for_tests(), &mut plan);

        let labels: Vec<&str> = plan
            .in_category(Category::ArchiveContents)
            .map(|f| f.label.as_str())
            .collect();
        assert_eq!(labels, vec!["old-plan.md", "patch.diff", "(total)"]);

        let total = plan
            .in_category(Category::ArchiveContents)
            .find(|f| f.label == "(total)")
            .unwrap();
        assert!(
            total.details[0].contains("2 entries"),
            "{:?}",
            total.details
        );
        assert!(total.details[0].contains("3.0 KB"), "{:?}", total.details);
        assert!(
            plan.in_category(Category::ArchiveContents)
                .all(|f| !f.is_actionable()),
            ".archive contents are report-only"
        );
    }

    #[test]
    fn the_branch_deletion_log_is_not_reported_as_clutter() {
        let dir = TempDir::new().unwrap();
        let layout = Layout {
            work_dir: dir.path().to_path_buf(),
            main_repo: dir.path().join("master"),
        };
        std::fs::create_dir_all(layout.archive_dir()).unwrap();
        std::fs::write(layout.deleted_branches_log(), b"/x branch sha\n").unwrap();

        let mut plan = GcPlan::default();
        scan(&layout, &GcOptions::for_tests(), &mut plan);
        assert!(
            plan.in_category(Category::ArchiveContents).next().is_none(),
            "only gc's own log was present, so there is nothing to report"
        );
    }
}
