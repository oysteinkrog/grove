use std::path::{Path, PathBuf};

/// Convert a WSL-style path to a Windows path string.
///
/// Handles both short form (`/c/work/foo`) and long form (`/mnt/c/work/foo`).
/// Paths that don't match either WSL drive prefix are returned verbatim.
/// Already-Windows paths (`C:\...`) and UNC paths (`//wsl$/...`) pass through
/// unchanged.
pub fn to_windows_path(p: &Path) -> String {
    let s = p.to_string_lossy();

    // Already a Windows path: drive letter followed by colon
    // e.g. "C:\work\foo" or "C:/work/foo"
    if looks_like_windows_path(&s) {
        return s.into_owned();
    }

    // UNC-style paths pass through unchanged (//wsl$/ etc.)
    if s.starts_with("//") || s.starts_with(r"\\") {
        return s.into_owned();
    }

    // Short WSL form: /c/... or /C/...
    if let Some(rest) = strip_wsl_short_prefix(&s) {
        let drive_char = s.chars().nth(1).unwrap().to_ascii_uppercase();
        return format!("{}:\\{}", drive_char, rest.replace('/', "\\"));
    }

    // Long WSL form: /mnt/c/... or /mnt/C/...
    if let Some((drive_char, rest)) = strip_wsl_mnt_prefix(&s) {
        return format!(
            "{}:\\{}",
            drive_char.to_ascii_uppercase(),
            rest.replace('/', "\\")
        );
    }

    s.into_owned()
}

/// Convert a Windows path string to a WSL path (short `/c/` form).
///
/// Returns the path unchanged if it doesn't look like a Windows path.
pub fn to_wsl_path(s: &str) -> PathBuf {
    // Already a WSL path — starts with / and does not look like a Windows path
    if s.starts_with('/') && !looks_like_windows_path(s) {
        return PathBuf::from(s);
    }

    // Windows drive path: "C:\work\foo" or "C:/work/foo"
    if let Some((drive, rest)) = split_windows_drive(s) {
        let unix_rest = rest.replace('\\', "/");
        // Trim any leading slash from rest to avoid double-slash
        let unix_rest = unix_rest.trim_start_matches('/');
        let drive_lower = drive.to_ascii_lowercase();
        if unix_rest.is_empty() {
            return PathBuf::from(format!("/{}", drive_lower));
        }
        return PathBuf::from(format!("/{}/{}", drive_lower, unix_rest));
    }

    PathBuf::from(s)
}

// ── helpers ──────────────────────────────────────────────────────────────────

fn looks_like_windows_path(s: &str) -> bool {
    // Matches "X:" at the start where X is a letter
    let mut chars = s.chars();
    if let (Some(c), Some(':')) = (chars.next(), chars.next()) {
        return c.is_ascii_alphabetic();
    }
    false
}

/// If `s` matches `/x/` or `/x` (single drive letter), return the part after the drive slash.
fn strip_wsl_short_prefix(s: &str) -> Option<&str> {
    let bytes = s.as_bytes();
    if bytes.len() < 2 || bytes[0] != b'/' {
        return None;
    }
    let drive = bytes[1] as char;
    if !drive.is_ascii_alphabetic() {
        return None;
    }
    // Must be followed by '/' or end of string (root: "/c")
    match bytes.get(2) {
        None => Some(""),             // "/c" → root
        Some(&b'/') => Some(&s[3..]), // "/c/..." → "..."
        _ => None,
    }
}

/// If `s` matches `/mnt/x/...`, return `(drive_char, rest_after_drive_slash)`.
fn strip_wsl_mnt_prefix(s: &str) -> Option<(char, &str)> {
    let s = s.strip_prefix("/mnt/")?;
    let mut chars = s.chars();
    let drive = chars.next()?;
    if !drive.is_ascii_alphabetic() {
        return None;
    }
    match s.as_bytes().get(1) {
        None => Some((drive, "")),             // "/mnt/c" → root
        Some(&b'/') => Some((drive, &s[2..])), // "/mnt/c/..." → "..."
        _ => None,
    }
}

/// Split a Windows path like "C:\work\foo" into ('C', r"\work\foo").
fn split_windows_drive(s: &str) -> Option<(char, &str)> {
    let mut chars = s.chars();
    let drive = chars.next()?;
    if !drive.is_ascii_alphabetic() {
        return None;
    }
    if chars.next()? != ':' {
        return None;
    }
    Some((drive, &s[2..]))
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── to_windows_path ───────────────────────────────────────────────────────

    #[test]
    fn short_wsl_path_becomes_windows() {
        // AC 1: /c/work/foo → C:\work\foo
        assert_eq!(to_windows_path(Path::new("/c/work/foo")), r"C:\work\foo");
    }

    #[test]
    fn mnt_wsl_path_becomes_windows() {
        // AC 2: /mnt/c/work/foo → C:\work\foo
        assert_eq!(
            to_windows_path(Path::new("/mnt/c/work/foo")),
            r"C:\work\foo"
        );
    }

    #[test]
    fn already_windows_path_unchanged() {
        // AC 3: C:\work\foo → C:\work\foo
        assert_eq!(to_windows_path(Path::new(r"C:\work\foo")), r"C:\work\foo");
    }

    #[test]
    fn non_mounted_path_unchanged() {
        // AC 4: /home/oystein → /home/oystein (no /c/ or /mnt/<drive>/ prefix)
        assert_eq!(to_windows_path(Path::new("/home/oystein")), "/home/oystein");
    }

    #[test]
    fn unc_wsl_path_unchanged() {
        // AC 5: //wsl$/Ubuntu/... passes through
        assert_eq!(
            to_windows_path(Path::new("//wsl$/Ubuntu/home")),
            "//wsl$/Ubuntu/home"
        );
    }

    #[test]
    fn wsl_drive_root_becomes_windows_root() {
        // Edge: /c/ → C:\
        assert_eq!(to_windows_path(Path::new("/c")), r"C:\");
        assert_eq!(to_windows_path(Path::new("/c/")), r"C:\");
    }

    #[test]
    fn mnt_drive_root_becomes_windows_root() {
        // Edge: /mnt/c → C:\
        assert_eq!(to_windows_path(Path::new("/mnt/c")), r"C:\");
    }

    // ── to_wsl_path ───────────────────────────────────────────────────────────

    #[test]
    fn windows_path_becomes_wsl() {
        // AC 6: C:\work\foo → /c/work/foo
        assert_eq!(to_wsl_path(r"C:\work\foo"), PathBuf::from("/c/work/foo"));
    }

    #[test]
    fn already_wsl_path_unchanged() {
        // AC 7: /c/work/foo → /c/work/foo
        assert_eq!(to_wsl_path("/c/work/foo"), PathBuf::from("/c/work/foo"));
    }

    #[test]
    fn windows_root_becomes_wsl_root() {
        // Edge: C:\ → /c
        assert_eq!(to_wsl_path(r"C:\"), PathBuf::from("/c"));
    }

    #[test]
    fn windows_forward_slash_becomes_wsl() {
        // Windows paths sometimes use forward slashes
        assert_eq!(to_wsl_path("C:/work/foo"), PathBuf::from("/c/work/foo"));
    }

    #[test]
    fn lowercase_drive_letter_normalized() {
        // Drive letters should be normalized: c:\ → /c/
        assert_eq!(to_wsl_path(r"c:\work\foo"), PathBuf::from("/c/work/foo"));
    }

    #[test]
    fn round_trip_wsl_to_windows_to_wsl() {
        let original = "/c/work/grove/src/paths.rs";
        let win = to_windows_path(Path::new(original));
        let back = to_wsl_path(&win);
        assert_eq!(back, PathBuf::from(original));
    }

    #[test]
    fn round_trip_mnt_to_windows_to_wsl() {
        let mnt = "/mnt/c/work/grove";
        let short = "/c/work/grove";
        let win = to_windows_path(Path::new(mnt));
        let back = to_wsl_path(&win);
        // Round-trip lands on the short form
        assert_eq!(back, PathBuf::from(short));
    }
}
