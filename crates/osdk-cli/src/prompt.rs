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

    /// Select one or more zero-based option indexes.
    fn select_many(&self, _question: &str, _options: &[String]) -> Result<Vec<usize>> {
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

    fn select_many(&self, question: &str, options: &[String]) -> Result<Vec<usize>> {
        ensure_interactive(question)?;
        loop {
            print_options(question, options, true)?;
            match parse_multi_selection(&read_answer()?, options.len()) {
                Ok(selected) => return Ok(selected),
                Err(error) => eprintln!("{error}"),
            }
        }
    }

    fn select_one(&self, question: &str, options: &[String], default: usize) -> Result<usize> {
        if self.assume_yes {
            return Ok(default);
        }
        ensure_interactive(question)?;
        loop {
            print_options(question, options, false)?;
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

fn print_options(question: &str, options: &[String], many: bool) -> Result<()> {
    if options.is_empty() {
        return Err(anyhow!("cannot select from an empty option list"));
    }
    eprintln!("{question}");
    for (index, option) in options.iter().enumerate() {
        eprintln!("  {}) {option}", index + 1);
    }
    if many {
        eprint!("Choose one or more numbers (for example 1,3-5): ");
    } else {
        eprint!("Choose one number (Enter accepts the recommended default): ");
    }
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

fn parse_multi_selection(answer: &str, count: usize) -> Result<Vec<usize>> {
    if count == 0 {
        return Err(anyhow!("invalid selection options"));
    }
    let mut selected = std::collections::BTreeSet::new();
    for token in answer
        .split([',', ' ', '\t'])
        .map(str::trim)
        .filter(|token| !token.is_empty())
    {
        if let Some((start, end)) = token.split_once('-') {
            let start = parse_selection_number(start, count)?;
            let end = parse_selection_number(end, count)?;
            if start > end {
                return Err(anyhow!("invalid descending selection range `{token}`"));
            }
            selected.extend(start..=end);
        } else {
            selected.insert(parse_selection_number(token, count)?);
        }
    }
    if selected.is_empty() {
        return Err(anyhow!("select at least one option"));
    }
    Ok(selected.into_iter().map(|index| index - 1).collect())
}

fn parse_selection_number(value: &str, count: usize) -> Result<usize> {
    let number = value
        .parse::<usize>()
        .map_err(|_| anyhow!("invalid selection `{value}`: enter numbers from 1 to {count}"))?;
    if !(1..=count).contains(&number) {
        return Err(anyhow!(
            "selection {number} is out of range; choose 1 through {count}"
        ));
    }
    Ok(number)
}

fn is_affirmative(answer: &str) -> bool {
    matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes" | "是" | "好"
    )
}

#[cfg(test)]
mod tests {
    use super::{is_affirmative, parse_multi_selection, parse_single_selection};

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
    fn parses_multi_selection_ranges_and_deduplicates() {
        assert_eq!(
            parse_multi_selection("1, 3-5 3", 6).unwrap(),
            vec![0, 2, 3, 4]
        );
    }

    #[test]
    fn rejects_empty_descending_and_out_of_range_multi_selection() {
        assert!(parse_multi_selection("", 3).is_err());
        assert!(parse_multi_selection("3-1", 3).is_err());
        assert!(parse_multi_selection("4", 3).is_err());
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
