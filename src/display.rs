use std::io::IsTerminal;

use comfy_table::presets::UTF8_FULL_CONDENSED;
use comfy_table::{Cell, ContentArrangement, Table};
use owo_colors::OwoColorize;

pub fn use_color() -> bool {
    std::io::stdout().is_terminal()
        && std::env::var_os("NO_COLOR").is_none()
        && std::env::var("TERM").as_deref() != Ok("dumb")
}

pub fn make_table() -> Table {
    let mut table = Table::new();
    table
        .load_preset(UTF8_FULL_CONDENSED)
        .set_content_arrangement(ContentArrangement::Dynamic);
    table
}

pub fn dim(s: &str) -> String {
    if use_color() {
        s.dimmed().to_string()
    } else {
        s.to_string()
    }
}

pub fn make_header_cell(text: &str) -> Cell {
    Cell::new(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_color_env_disables_color() {
        // When NO_COLOR is set, use_color() must return false regardless of TTY.
        // We cannot force is_terminal() in tests, but we can verify the NO_COLOR branch.
        unsafe { std::env::set_var("NO_COLOR", "1") };
        assert!(!use_color(), "NO_COLOR=1 should disable color");
        unsafe { std::env::remove_var("NO_COLOR") };
    }

    #[test]
    fn dumb_term_disables_color() {
        unsafe { std::env::remove_var("NO_COLOR") };
        unsafe { std::env::set_var("TERM", "dumb") };
        assert!(!use_color(), "TERM=dumb should disable color");
        unsafe { std::env::remove_var("TERM") };
    }
}
