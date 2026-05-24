use clap::CommandFactory;
use clap_complete::{Shell, generate};

use crate::cli::Cli;

pub fn run(shell: Shell) {
    let mut cmd = Cli::command();
    generate(shell, &mut cmd, "grove", &mut std::io::stdout());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn completion_output(shell: Shell) -> String {
        let mut cmd = Cli::command();
        let mut buf = Vec::new();
        generate(shell, &mut cmd, "grove", &mut buf);
        String::from_utf8(buf).expect("completion output is valid UTF-8")
    }

    #[test]
    fn fish_completion_nonempty_and_contains_grove() {
        let out = completion_output(Shell::Fish);
        assert!(!out.is_empty(), "fish completion must be non-empty");
        assert!(
            out.contains("grove"),
            "fish completion must mention 'grove'"
        );
    }

    #[test]
    fn bash_completion_nonempty_and_contains_grove() {
        let out = completion_output(Shell::Bash);
        assert!(!out.is_empty(), "bash completion must be non-empty");
        assert!(
            out.contains("grove"),
            "bash completion must mention 'grove'"
        );
    }

    #[test]
    fn zsh_completion_nonempty_and_contains_grove() {
        let out = completion_output(Shell::Zsh);
        assert!(!out.is_empty(), "zsh completion must be non-empty");
        assert!(out.contains("grove"), "zsh completion must mention 'grove'");
    }

    #[test]
    fn powershell_completion_nonempty_and_contains_grove() {
        let out = completion_output(Shell::PowerShell);
        assert!(!out.is_empty(), "powershell completion must be non-empty");
        assert!(
            out.contains("grove"),
            "powershell completion must mention 'grove'"
        );
    }
}
