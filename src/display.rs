use std::io::IsTerminal;

use comfy_table::presets::UTF8_FULL_CONDENSED;
use comfy_table::{Cell, ContentArrangement, Table};
use owo_colors::OwoColorize;

/// Returns true only when color output is appropriate: stdout is a TTY,
/// NO_COLOR is unset, and TERM is not "dumb".
pub fn should_use_color() -> bool {
    std::io::stdout().is_terminal()
        && std::env::var_os("NO_COLOR").is_none()
        && std::env::var("TERM").as_deref() != Ok("dumb")
}

/// Alias kept for callers in list.rs that were written before the rename.
#[inline]
pub fn use_color() -> bool {
    should_use_color()
}

/// Best-effort terminal width; falls back to 120 when detection fails.
pub fn terminal_width() -> u16 {
    comfy_table::Table::new().width().unwrap_or(120)
}

pub fn make_table() -> Table {
    let mut table = Table::new();
    table
        .load_preset(UTF8_FULL_CONDENSED)
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_width(terminal_width());
    table
}

pub fn dim<S: AsRef<str>>(s: S) -> String {
    if should_use_color() {
        s.as_ref().dimmed().to_string()
    } else {
        s.as_ref().to_string()
    }
}

pub fn make_header_cell(text: &str) -> Cell {
    Cell::new(text)
}

#[cfg(test)]
mod tests {
    use serial_test::serial;

    use super::*;

    #[test]
    #[serial]
    fn no_color_env_disables_color() {
        unsafe { std::env::set_var("NO_COLOR", "1") };
        let result = should_use_color();
        unsafe { std::env::remove_var("NO_COLOR") };
        assert!(!result, "NO_COLOR=1 should disable color");
    }

    #[test]
    #[serial]
    fn non_tty_disables_color() {
        // In a test environment stdout is not a TTY, so should_use_color() returns false
        // regardless of NO_COLOR. We verify the is-terminal branch works here.
        unsafe { std::env::remove_var("NO_COLOR") };
        unsafe { std::env::remove_var("TERM") };
        // Tests run with non-TTY stdout (piped), so this must be false.
        let result = should_use_color();
        assert!(!result, "non-TTY stdout should disable color");
    }

    #[test]
    #[serial]
    fn color_helpers_passthrough_when_disabled() {
        unsafe { std::env::set_var("NO_COLOR", "1") };
        let input = "hello world";
        let result = dim(input);
        unsafe { std::env::remove_var("NO_COLOR") };
        assert_eq!(
            result, input,
            "dim() must pass through text verbatim when color disabled"
        );
    }

    #[test]
    #[serial]
    fn dim_preserves_text_content() {
        // Regardless of ANSI wrapping, the plain text must be preserved.
        let input = "frozen-project";
        let result = dim(input);
        assert!(
            result.contains(input),
            "dim() output must contain the original text"
        );
    }
}
