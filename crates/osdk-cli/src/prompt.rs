use std::io::{self, IsTerminal, Write};

use anyhow::{anyhow, Result};
use osdk_core::{i18n, t};

pub trait Prompt: Send + Sync {
    fn confirm(&self, question: &str) -> Result<bool>;

    /// Whether this prompt can safely ask a question right now.
    fn is_interactive(&self) -> bool {
        false
    }

    /// Whether `--yes` requested deterministic recommended defaults.
    fn assume_yes(&self) -> bool {
        false
    }

    /// Select one or more zero-based option indexes. `defaults` marks the
    /// options that start checked; a shorter slice leaves the rest unchecked.
    fn select_many(
        &self,
        _question: &str,
        _options: &[String],
        _defaults: &[bool],
    ) -> Result<Vec<usize>> {
        Err(anyhow!("interactive selection is unavailable"))
    }

    /// Select one zero-based option index. `default` is used for an empty answer
    /// and for `--yes`, whose unattended meaning is to accept the recommended
    /// choice rather than block for input.
    fn select_one(&self, _question: &str, _options: &[String], default: usize) -> Result<usize> {
        Ok(default)
    }
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

        ensure_interactive(question)?;
        eprint!("{question} {}", i18n::tr("prompt.yes_no"));
        io::stderr().flush()?;
        let mut answer = String::new();
        io::stdin().read_line(&mut answer)?;
        Ok(is_affirmative(&answer))
    }

    fn is_interactive(&self) -> bool {
        io::stdin().is_terminal()
    }

    fn assume_yes(&self) -> bool {
        self.assume_yes
    }

    /// A checklist driven by the keyboard rather than typed numbers.
    ///
    /// Typing indexes made the choice hard to see and easy to get wrong, and
    /// there was no way to take everything at once. Arrow keys move, Space
    /// toggles, `a` toggles all, Enter confirms; Esc cancels.
    fn select_many(
        &self,
        question: &str,
        options: &[String],
        defaults: &[bool],
    ) -> Result<Vec<usize>> {
        ensure_interactive(question)?;
        if options.is_empty() {
            return Err(anyhow!("cannot select from an empty option list"));
        }
        let mut checked: Vec<bool> = (0..options.len())
            .map(|index| defaults.get(index).copied().unwrap_or(false))
            .collect();
        let term = dialoguer::console::Term::stderr();
        loop {
            let picked =
                dialoguer::MultiSelect::with_theme(&dialoguer::theme::ColorfulTheme::default())
                    .with_prompt(format!(
                        "{question} (\u{2191}/\u{2193} move, Space toggle, a all, Enter confirm)"
                    ))
                    .items(options)
                    .defaults(&checked)
                    .interact_on_opt(&term)?;
            match picked {
                None => return Err(anyhow!("selection cancelled")),
                Some(picked) if picked.is_empty() => {
                    eprintln!("Select at least one option (Space toggles, a selects all).");
                    checked = vec![false; options.len()];
                }
                Some(picked) => return Ok(picked),
            }
        }
    }

    fn select_one(&self, question: &str, options: &[String], default: usize) -> Result<usize> {
        if self.assume_yes {
            return Ok(default);
        }
        ensure_interactive(question)?;
        loop {
            print_options(question, options)?;
            match parse_single_selection(&read_answer()?, options.len(), default) {
                Ok(selected) => return Ok(selected),
                Err(error) => eprintln!("{error}"),
            }
        }
    }
}

fn ensure_interactive(question: &str) -> Result<()> {
    if io::stdin().is_terminal() {
        Ok(())
    } else {
        Err(anyhow!(t!(
            "err.confirmation_non_interactive",
            question = question
        )))
    }
}

fn print_options(question: &str, options: &[String]) -> Result<()> {
    if options.is_empty() {
        return Err(anyhow!("cannot select from an empty option list"));
    }
    eprintln!("{question}");
    for (index, option) in options.iter().enumerate() {
        eprintln!("  {}) {option}", index + 1);
    }
    eprint!("Choose one number (Enter accepts the recommended default): ");
    io::stderr().flush()?;
    Ok(())
}

fn read_answer() -> Result<String> {
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    Ok(answer)
}

fn parse_single_selection(answer: &str, count: usize, default: usize) -> Result<usize> {
    if count == 0 || default >= count {
        return Err(anyhow!("invalid selection options"));
    }
    let trimmed = answer.trim();
    if trimmed.is_empty() {
        return Ok(default);
    }
    let selected = trimmed
        .parse::<usize>()
        .map_err(|_| anyhow!("invalid selection `{trimmed}`: enter a number from 1 to {count}"))?;
    if !(1..=count).contains(&selected) {
        return Err(anyhow!(
            "selection {selected} is out of range; choose 1 through {count}"
        ));
    }
    Ok(selected - 1)
}

fn is_affirmative(answer: &str) -> bool {
    matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes" | "是" | "好"
    )
}

#[cfg(test)]
mod tests {
    use super::{is_affirmative, parse_single_selection};

    #[test]
    fn accepts_explicit_affirmative_answers_only() {
        for answer in ["y", "Y", "yes", "YES", "是", "好"] {
            assert!(is_affirmative(answer), "{answer}");
        }
        for answer in ["", "n", "no", "true", "1"] {
            assert!(!is_affirmative(answer), "{answer}");
        }
    }

    #[test]
    fn parses_single_selection_with_default_and_range_checks() {
        assert_eq!(parse_single_selection("", 2, 0).unwrap(), 0);
        assert_eq!(parse_single_selection("2", 2, 0).unwrap(), 1);
        assert!(parse_single_selection("0", 2, 0).is_err());
        assert!(parse_single_selection("3", 2, 0).is_err());
        assert!(parse_single_selection("many", 2, 0).is_err());
    }
}
