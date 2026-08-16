use std::io::{self, IsTerminal, Write};

use anyhow::{anyhow, Result};
use osdk_core::{i18n, t};

pub trait Prompt: Send + Sync {
    fn confirm(&self, question: &str) -> Result<bool>;
}

pub struct TerminalPrompt {
    assume_yes: bool,
}

impl TerminalPrompt {
    pub fn new(assume_yes: bool) -> Self {
        Self { assume_yes }
    }
}

impl Prompt for TerminalPrompt {
    fn confirm(&self, question: &str) -> Result<bool> {
        if self.assume_yes {
            return Ok(true);
        }

        let stdin = io::stdin();
        if !stdin.is_terminal() {
            return Err(anyhow!(t!(
                "err.confirmation_non_interactive",
                question = question
            )));
        }

        eprint!("{question} {}", i18n::tr("prompt.yes_no"));
        io::stderr().flush()?;
        let mut answer = String::new();
        stdin.read_line(&mut answer)?;
        Ok(is_affirmative(&answer))
    }
}

fn is_affirmative(answer: &str) -> bool {
    matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes" | "是" | "好"
    )
}

#[cfg(test)]
mod tests {
    use super::is_affirmative;

    #[test]
    fn accepts_explicit_affirmative_answers_only() {
        for answer in ["y", "Y", "yes", "YES", "是", "好"] {
            assert!(is_affirmative(answer), "{answer}");
        }
        for answer in ["", "n", "no", "true", "1"] {
            assert!(!is_affirmative(answer), "{answer}");
        }
    }
}
