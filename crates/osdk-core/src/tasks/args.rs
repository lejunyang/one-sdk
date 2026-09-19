//! Task arguments: declaration, validation, and substitution.
//!
//! The design question that shapes everything here is *how a value reaches the
//! command*. mise interpolates into the shell string (`{{arg(name)}}`) and has
//! since deprecated it, citing unpredictable shell escaping among the reasons.
//! That reason is worse for osdk, because the default interpreter on Windows is
//! `cmd`:
//!
//! | shell | can a value be escaped safely? |
//! | --- | --- |
//! | `sh -c` | yes -- single-quote and replace `'` with `'\''` |
//! | `pwsh` | yes -- single-quote and double the `'` |
//! | **`cmd /c`** | **no** -- `%VAR%` expands before quoting is considered, and `^` interacts with quote state |
//!
//! A feature that is injection-safe on two platforms and unsafe on the third is
//! worse than no feature, because it invites the belief that it is safe
//! everywhere. So substitution is offered only where injection is structurally
//! impossible: [`RunStep::Argv`], whose elements become argv entries directly
//! and never pass through a parser. Shell strings keep working; they just do not
//! interpolate.
//!
//! The one exception is appending, which does not interpolate *into* anything:
//! see [`Spec::append_policy`].

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Placeholder expanding to every not-otherwise-consumed argument.
///
/// Expands to *several* argv entries, not one joined string: `-- --nocapture
/// --exact` has to arrive as two arguments, and a joined string would arrive as
/// one containing a space.
pub const REST_PLACEHOLDER: &str = "{{args}}";

/// One positional parameter.
///
/// Declared as an array of tables rather than a map because **order is the
/// meaning** of a positional. A map would be sorted by key on load, silently
/// reordering `<env> <manifest>` into `<env> <manifest>` or worse depending on
/// spelling.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PositionalArg {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub help: Option<String>,
    /// Allowed values. Anything else is rejected before a command runs.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub choices: Vec<String>,
    /// Having a default is what makes a positional optional.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
}

/// A `--name value` option.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OptionArg {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub help: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub choices: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
}

/// A `--name` boolean switch.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FlagArg {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub help: Option<String>,
}

/// A task's full argument declaration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Spec {
    pub args: Vec<PositionalArg>,
    pub options: BTreeMap<String, OptionArg>,
    pub flags: BTreeMap<String, FlagArg>,
}

impl Spec {
    pub fn is_empty(&self) -> bool {
        self.args.is_empty() && self.options.is_empty() && self.flags.is_empty()
    }

    /// Reject declarations that could not work, before anything runs.
    pub fn validate(&self, task: &str) -> Result<()> {
        let mut seen = std::collections::BTreeSet::new();
        for arg in &self.args {
            if arg.name.trim().is_empty() {
                return Err(Error::config(format!(
                    "task `{task}`: an arg has no `name`"
                )));
            }
            if !seen.insert(arg.name.as_str()) {
                return Err(Error::config(format!(
                    "task `{task}`: duplicate arg `{}`",
                    arg.name
                )));
            }
            if let Some(default) = &arg.default {
                if !arg.choices.is_empty() && !arg.choices.contains(default) {
                    return Err(Error::config(format!(
                        "task `{task}`: arg `{}` has default `{default}`, which is not among its choices",
                        arg.name
                    )));
                }
            }
        }
        // An optional positional followed by a required one cannot be filled
        // unambiguously: given one value, there is no way to say which slot it
        // belongs to.
        let mut seen_optional = None;
        for arg in &self.args {
            match (&arg.default, seen_optional) {
                (Some(_), _) => seen_optional = Some(arg.name.as_str()),
                (None, Some(earlier)) => {
                    return Err(Error::config(format!(
                        "task `{task}`: required arg `{}` follows optional arg `{earlier}`; \
                         a single value could fill either slot",
                        arg.name
                    )));
                }
                (None, None) => {}
            }
        }
        for (name, option) in &self.options {
            if let Some(default) = &option.default {
                if !option.choices.is_empty() && !option.choices.contains(default) {
                    return Err(Error::config(format!(
                        "task `{task}`: option `--{name}` has default `{default}`, \
                         which is not among its choices"
                    )));
                }
            }
            if self.flags.contains_key(name) {
                return Err(Error::config(format!(
                    "task `{task}`: `{name}` is declared as both an option and a flag"
                )));
            }
        }
        Ok(())
    }
}

/// Parsed argument values, ready for substitution.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Values {
    /// Every declared name mapped to its resolved value, for `{{name}}`.
    pub named: BTreeMap<String, String>,
    /// Arguments not consumed by the spec, for `{{args}}`.
    pub rest: Vec<String>,
}

impl Values {
    /// Environment variables exposing every value.
    ///
    /// Exported unconditionally because this path has no escaping problem at
    /// all: the value goes into the child's environment block verbatim, never
    /// through a parser. It gives people who know their own shell a way to use
    /// arguments in a `run` string, at their own risk.
    pub fn env_vars(&self) -> BTreeMap<String, String> {
        let mut vars: BTreeMap<String, String> = self
            .named
            .iter()
            .map(|(name, value)| (format!("osdk_arg_{name}"), value.clone()))
            .collect();
        if !self.rest.is_empty() {
            vars.insert("osdk_args".into(), self.rest.join(" "));
        }
        vars
    }
}

/// Parse `input` against `spec`.
///
/// Validation happens here, before the first command runs, for the same reason
/// a typo in `depends` is caught at plan time: discovering it halfway through a
/// pipeline that already wrote files is strictly worse.
pub fn parse(task: &str, spec: &Spec, input: &[String]) -> Result<Values> {
    let mut values = Values::default();
    let mut positional: Vec<String> = Vec::new();
    let mut index = 0;

    while index < input.len() {
        let token = &input[index];
        index += 1;

        // `--` stops option parsing; everything after is positional or rest,
        // so a task can forward things that look like flags.
        if token == "--" {
            positional.extend(input[index..].iter().cloned());
            break;
        }

        let Some(name) = token.strip_prefix("--") else {
            positional.push(token.clone());
            continue;
        };

        // `--name=value` and `--name value` are both accepted.
        let (name, inline) = match name.split_once('=') {
            Some((name, value)) => (name, Some(value.to_string())),
            None => (name, None),
        };

        if let Some(option) = spec.options.get(name) {
            let value = match inline {
                Some(value) => value,
                None => {
                    let value = input.get(index).cloned().ok_or_else(|| {
                        Error::other(format!("task `{task}`: option `--{name}` needs a value"))
                    })?;
                    index += 1;
                    value
                }
            };
            check_choices(task, &format!("--{name}"), &option.choices, &value)?;
            values.named.insert(name.to_string(), value);
            continue;
        }

        if spec.flags.contains_key(name) {
            if inline.is_some() {
                return Err(Error::other(format!(
                    "task `{task}`: flag `--{name}` takes no value"
                )));
            }
            values.named.insert(name.to_string(), "1".into());
            continue;
        }

        // An unknown `--flag` is kept as a rest argument rather than rejected:
        // forwarding `--nocapture` to an inner command is the common case, and
        // a task that declares nothing should still be able to pass things on.
        positional.push(token.clone());
    }

    // Fill positionals in declaration order.
    let mut remaining = positional.into_iter();
    for arg in &spec.args {
        match remaining.next() {
            Some(value) => {
                check_choices(task, &arg.name, &arg.choices, &value)?;
                values.named.insert(arg.name.clone(), value);
            }
            None => match &arg.default {
                Some(default) => {
                    values.named.insert(arg.name.clone(), default.clone());
                }
                None => {
                    return Err(Error::other(format!(
                        "task `{task}`: missing required argument `{}`",
                        arg.name
                    )));
                }
            },
        }
    }
    values.rest = remaining.collect();

    // Defaults for options and flags that were not given.
    for (name, option) in &spec.options {
        if let Some(default) = &option.default {
            values
                .named
                .entry(name.clone())
                .or_insert_with(|| default.clone());
        }
    }
    for name in spec.flags.keys() {
        values.named.entry(name.clone()).or_default();
    }

    Ok(values)
}

fn check_choices(task: &str, label: &str, choices: &[String], value: &str) -> Result<()> {
    if choices.is_empty() || choices.iter().any(|choice| choice == value) {
        return Ok(());
    }
    Err(Error::other(format!(
        "task `{task}`: `{label}` must be one of {}, got `{value}`",
        choices
            .iter()
            .map(|choice| format!("`{choice}`"))
            .collect::<Vec<_>>()
            .join(", ")
    )))
}

/// Substitute `{{name}}` placeholders in one argv element.
///
/// `{{args}}` is handled by the caller because it expands to a variable number
/// of entries; this handles the one-to-one case.
pub fn substitute(task: &str, element: &str, values: &Values) -> Result<String> {
    let mut out = String::with_capacity(element.len());
    let mut rest = element;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            return Err(Error::config(format!(
                "task `{task}`: unclosed `{{{{` in `{element}`"
            )));
        };
        let name = after[..end].trim();
        let Some(value) = values.named.get(name) else {
            return Err(Error::config(format!(
                "task `{task}`: `{{{{{name}}}}}` is not a declared argument"
            )));
        };
        out.push_str(value);
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Expand one argv template into concrete arguments.
pub fn expand_argv(task: &str, template: &[String], values: &Values) -> Result<Vec<String>> {
    let mut out = Vec::with_capacity(template.len());
    for element in template {
        if element.trim() == REST_PLACEHOLDER {
            out.extend(values.rest.iter().cloned());
            continue;
        }
        out.push(substitute(task, element, values)?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec_from(toml: &str) -> Spec {
        toml::from_str(toml).expect("parse spec")
    }

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn positional_order_is_preserved_because_args_is_a_list() {
        let spec = spec_from(
            r#"
[[args]]
name = "env"
[[args]]
name = "manifest"
"#,
        );
        // A map keyed by name would come back sorted: manifest before env.
        assert_eq!(spec.args[0].name, "env");
        assert_eq!(spec.args[1].name, "manifest");

        let values = parse("deploy", &spec, &args(&["prod", "app.yaml"])).unwrap();
        assert_eq!(values.named["env"], "prod");
        assert_eq!(values.named["manifest"], "app.yaml");
    }

    #[test]
    fn options_accept_both_spellings_and_flags_are_booleans() {
        let spec = spec_from(
            r#"
[options.replicas]
default = "3"
[flags.wait]
"#,
        );
        let values = parse("deploy", &spec, &args(&["--replicas", "5", "--wait"])).unwrap();
        assert_eq!(values.named["replicas"], "5");
        assert_eq!(values.named["wait"], "1");

        let values = parse("deploy", &spec, &args(&["--replicas=7"])).unwrap();
        assert_eq!(values.named["replicas"], "7");
        // Unset flag is present but false, so `{{wait}}` never fails to resolve.
        assert_eq!(values.named["wait"], "");
        // Default applies when the option is absent.
        let values = parse("deploy", &spec, &[]).unwrap();
        assert_eq!(values.named["replicas"], "3");
    }

    #[test]
    fn choices_are_rejected_before_anything_runs() {
        let spec = spec_from(
            r#"
[[args]]
name = "env"
choices = ["staging", "prod"]
"#,
        );
        assert!(parse("deploy", &spec, &args(&["prod"])).is_ok());
        let error = parse("deploy", &spec, &args(&["production"]))
            .unwrap_err()
            .to_string();
        assert!(error.contains("must be one of"), "{error}");
        assert!(error.contains("production"), "{error}");
    }

    #[test]
    fn a_missing_required_argument_is_an_error_but_a_default_fills_in() {
        let required = spec_from("[[args]]\nname = \"env\"\n");
        let error = parse("deploy", &required, &[]).unwrap_err().to_string();
        assert!(error.contains("missing required argument"), "{error}");

        let optional = spec_from("[[args]]\nname = \"env\"\ndefault = \"staging\"\n");
        let values = parse("deploy", &optional, &[]).unwrap();
        assert_eq!(values.named["env"], "staging");
    }

    #[test]
    fn extra_arguments_become_rest() {
        let spec = spec_from("[[args]]\nname = \"env\"\n");
        let values = parse("deploy", &spec, &args(&["prod", "extra1", "extra2"])).unwrap();
        assert_eq!(values.named["env"], "prod");
        assert_eq!(values.rest, vec!["extra1", "extra2"]);
    }

    /// `{{args}}` must become several argv entries, not one joined string.
    #[test]
    fn rest_expands_to_separate_argv_entries() {
        let spec = Spec::default();
        let values = parse("test", &spec, &args(&["--nocapture", "--exact"])).unwrap();
        let expanded = expand_argv("test", &args(&["cargo", "test", "{{args}}"]), &values).unwrap();
        assert_eq!(expanded, vec!["cargo", "test", "--nocapture", "--exact"]);
    }

    /// A value with shell metacharacters stays exactly one argv entry.
    ///
    /// This is the whole reason substitution is limited to argv steps: there is
    /// no parser between here and the child process, so there is nothing for the
    /// value to be reinterpreted by.
    #[test]
    fn a_value_with_shell_metacharacters_stays_one_argument() {
        let spec = spec_from("[[args]]\nname = \"msg\"\n");
        let nasty = r#"prod & del /f /s /q C:\ | echo "pwned" %PATH%"#;
        let values = parse("deploy", &spec, &args(&[nasty])).unwrap();
        let expanded = expand_argv("deploy", &args(&["echo", "{{msg}}"]), &values).unwrap();
        assert_eq!(expanded.len(), 2, "{expanded:?}");
        assert_eq!(expanded[1], nasty, "value must survive verbatim");
    }

    #[test]
    fn an_undeclared_placeholder_is_rejected() {
        let spec = spec_from("[[args]]\nname = \"env\"\n");
        let values = parse("deploy", &spec, &args(&["prod"])).unwrap();
        let error = expand_argv("deploy", &args(&["echo", "{{nope}}"]), &values)
            .unwrap_err()
            .to_string();
        assert!(error.contains("not a declared argument"), "{error}");
    }

    #[test]
    fn an_unclosed_placeholder_is_rejected() {
        let values = Values::default();
        let error = substitute("x", "echo {{oops", &values)
            .unwrap_err()
            .to_string();
        assert!(error.contains("unclosed"), "{error}");
    }

    #[test]
    fn double_dash_stops_option_parsing() {
        let spec = spec_from("[flags.wait]\n");
        let values = parse("deploy", &spec, &args(&["--", "--wait"])).unwrap();
        // After `--`, `--wait` is data, not this task's flag.
        assert_eq!(values.named["wait"], "");
        assert_eq!(values.rest, vec!["--wait"]);
    }

    #[test]
    fn an_unknown_flag_is_forwarded_rather_than_rejected() {
        let spec = Spec::default();
        let values = parse("test", &spec, &args(&["--nocapture"])).unwrap();
        assert_eq!(values.rest, vec!["--nocapture"]);
    }

    #[test]
    fn an_option_without_a_value_is_an_error() {
        let spec = spec_from("[options.env]\n");
        let error = parse("deploy", &spec, &args(&["--env"]))
            .unwrap_err()
            .to_string();
        assert!(error.contains("needs a value"), "{error}");
    }

    #[test]
    fn a_required_positional_after_an_optional_one_is_rejected() {
        let spec = spec_from(
            r#"
[[args]]
name = "first"
default = "a"
[[args]]
name = "second"
"#,
        );
        let error = spec.validate("deploy").unwrap_err().to_string();
        assert!(error.contains("follows optional"), "{error}");
    }

    #[test]
    fn a_default_outside_its_choices_is_rejected() {
        let spec = spec_from(
            r#"
[[args]]
name = "env"
choices = ["staging", "prod"]
default = "dev"
"#,
        );
        let error = spec.validate("deploy").unwrap_err().to_string();
        assert!(error.contains("not among its choices"), "{error}");
    }

    #[test]
    fn a_name_declared_as_both_option_and_flag_is_rejected() {
        let spec = spec_from("[options.wait]\n[flags.wait]\n");
        let error = spec.validate("deploy").unwrap_err().to_string();
        assert!(error.contains("both an option and a flag"), "{error}");
    }

    #[test]
    fn duplicate_positional_names_are_rejected() {
        let spec = spec_from("[[args]]\nname = \"env\"\n[[args]]\nname = \"env\"\n");
        let error = spec.validate("deploy").unwrap_err().to_string();
        assert!(error.contains("duplicate"), "{error}");
    }

    #[test]
    fn values_are_exposed_as_environment_variables() {
        let spec = spec_from("[[args]]\nname = \"env\"\n");
        let values = parse("deploy", &spec, &args(&["prod", "extra"])).unwrap();
        let vars = values.env_vars();
        assert_eq!(vars["osdk_arg_env"], "prod");
        assert_eq!(vars["osdk_args"], "extra");
    }

    #[test]
    fn unknown_spec_field_is_rejected_rather_than_ignored() {
        let error = toml::from_str::<Spec>("[[args]]\nname = \"x\"\nchoicse = []\n")
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("choicse") || error.contains("unknown"),
            "{error}"
        );
    }
}
