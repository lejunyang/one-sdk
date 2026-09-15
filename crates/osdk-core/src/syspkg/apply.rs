//! Registering a measured mirror into the host package manager's own
//! configuration -- the one layer of mirror acceleration that changes state
//! beyond osdk.
//!
//! Kept apart from [`super::mirror`] the way `container/apply.rs` is kept apart
//! from `container/mirror.rs`: planning is read-only and always safe to run,
//! while applying needs administrator rights, alters configuration every winget
//! caller on the machine shares, and weakens the trust chain. Those belong to
//! different modules because they carry different risk, not because the code is
//! long.
//!
//! # Why a plan must be confirmed by fingerprint
//!
//! A plan is generated from the sources registered at that moment. Between
//! generating it and running it, that registration list can change -- another
//! administrator, another tool, or the user in a second terminal. Executing a
//! stale plan would remove or replace a source the user never saw described.
//! So a plan carries a fingerprint of the state it was computed from, and
//! applying re-reads that state and refuses when it no longer matches, exactly
//! as `container/apply.rs` refuses on `StaleInput`.

use serde::Serialize;
use sha2::{Digest, Sha256};

use super::report::SourceRecord;

/// How a mirror gets into winget's configuration.
///
/// Only one shape exists, and that is a finding rather than a simplification:
/// adding a mirror as a second source fails with `0x80073D06`, because a
/// `Microsoft.PreIndexed.Package` source installs under one fixed MSIX identity
/// (`Microsoft.Winget.Source_8wekyb3d8bbwe`) and a mirror ships a copy of that
/// same package. Verified on an elevated host. The enum is kept as a named type
/// so the plan states its shape explicitly, and so Homebrew -- which mirrors
/// through environment variables instead -- can add its own variant without
/// reinterpreting this one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RegistrationShape {
    /// Remove the source named `winget` and re-add that name pointing at the
    /// mirror.
    ///
    /// What mirror operators document, and the only shape winget permits. It is
    /// invasive: between the two commands the host has no package source at all,
    /// and afterwards the official endpoint is no longer registered.
    ReplaceDefault,
}

/// One command a plan will run, recorded in full so the user sees it before
/// confirming.
///
/// Stored as argv rather than a joined string: a plan is shown to a human *and*
/// executed, and re-parsing a display string is how quoting bugs turn into the
/// wrong command being run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PlannedCommand {
    pub program: String,
    pub args: Vec<String>,
}

impl PlannedCommand {
    /// The command as a runnable spec.
    ///
    /// Built from the stored argv, so what runs is exactly what was planned and
    /// displayed -- never a re-parse of the display string, which is where
    /// quoting bugs become a different command.
    pub fn to_spec(&self) -> crate::process::CommandSpec {
        crate::process::CommandSpec::new(&self.program).args(&self.args)
    }

    /// The command as a human reads it. Display only; never re-parsed.
    pub fn display(&self) -> String {
        let mut rendered = self.program.clone();
        for arg in &self.args {
            rendered.push(' ');
            // Quote what a shell would need quoted, so a copied line behaves.
            if arg.contains(' ') {
                rendered.push('"');
                rendered.push_str(arg);
                rendered.push('"');
            } else {
                rendered.push_str(arg);
            }
        }
        rendered
    }
}

/// What applying a mirror registration would cost the user, stated up front.
///
/// These are not warnings osdk invents; each is a verified consequence, and a
/// user confirming a plan is entitled to see them before deciding.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Consequence {
    /// The change needs administrator rights.
    NeedsAdministrator,
    /// A mirror cannot carry the built-in source's `StoreOrigin` trust marker,
    /// so the trust chain is weaker afterwards.
    LosesStoreOriginTrust,
    /// Only the package index is mirrored; installers still come from each
    /// vendor, so downloads do not get faster.
    IndexOnlyAcceleration,
    /// The official endpoint will no longer be registered.
    OfficialSourceRemoved,
    /// Affects every winget caller on this machine, not just osdk.
    MachineWide,
    /// Between removing and re-adding, the host briefly has no package source.
    ///
    /// Named explicitly because it is the window in which a failure hurts: if
    /// the add fails, winget is left with nothing until the rollback runs.
    BrieflyWithoutAnySource,
}

/// Why applying a mirror would fail, established before anything is changed.
///
/// Refusing up front is the whole point. Windows rejects an older package with
/// `0x80073D06`, and a mirror lagging upstream is the common case rather than an
/// edge case, so running the destructive sequence and letting it fail would
/// leave the user to recover from a failure osdk could have predicted.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case", tag = "reason", content = "detail")]
pub enum Infeasible {
    /// The mirror's package is older than the one already installed, so Windows
    /// would reject it with `0x80073D06`.
    MirrorIsStale {
        /// When the mirror last published, as an HTTP date.
        mirror_last_modified: String,
        /// When the currently registered source last published.
        installed_last_modified: String,
    },
    /// The mirror's publish time could not be established, so staleness cannot
    /// be ruled out.
    ///
    /// Treated as infeasible rather than proceeding hopefully: the failure this
    /// check exists to prevent is exactly what happens when it guesses wrong.
    PublishTimeUnknown { endpoint: String },
    /// The mirror was measured as unreachable.
    MirrorUnreachable { endpoint: String },
}

/// Compare publish times to decide whether a mirror can actually be installed.
///
/// # Why publish time rather than the version number
///
/// The obvious check would compare MSIX versions, but a mirror publishes no
/// version metadata: measured against USTC, `/version` is a plain 404, and
/// Huawei's mirror answers `200` with the same 11,963-byte HTML page for *any*
/// path, including invented ones -- so trusting a status code there would feed
/// an HTML document into a destructive decision.
///
/// `Last-Modified` is available from a single `HEAD`, and while it cannot be
/// converted into a version (`2026.915.1714.48` was published at 17:45 GMT, so
/// the `1714` is a build time, not the publish time), the two move together:
/// USTC's 10:21 publish corresponds to version `1105`, older than the installed
/// `1714`. So it orders correctly, which is all a staleness check needs.
pub fn assess_feasibility(
    mirror_last_modified: Option<&str>,
    installed_last_modified: Option<&str>,
    endpoint: &str,
    reachable: bool,
) -> std::result::Result<(), Infeasible> {
    if !reachable {
        return Err(Infeasible::MirrorUnreachable {
            endpoint: endpoint.to_owned(),
        });
    }

    let (Some(mirror), Some(installed)) = (mirror_last_modified, installed_last_modified) else {
        return Err(Infeasible::PublishTimeUnknown {
            endpoint: endpoint.to_owned(),
        });
    };

    let (Some(mirror_at), Some(installed_at)) =
        (parse_http_date(mirror), parse_http_date(installed))
    else {
        // An unparsable date is not evidence of freshness.
        return Err(Infeasible::PublishTimeUnknown {
            endpoint: endpoint.to_owned(),
        });
    };

    if mirror_at < installed_at {
        return Err(Infeasible::MirrorIsStale {
            mirror_last_modified: mirror.to_owned(),
            installed_last_modified: installed.to_owned(),
        });
    }
    Ok(())
}

/// Parse an RFC 7231 IMF-fixdate into a comparable tuple.
///
/// Only the one format HTTP requires servers to emit is accepted. The obsolete
/// RFC 850 and asctime forms are deliberately rejected rather than guessed at:
/// this feeds a destructive decision, and a misparsed date that happens to
/// compare "newer" is worse than declining to judge.
fn parse_http_date(value: &str) -> Option<(i32, u8, u8, u8, u8, u8)> {
    // Example: `Tue, 15 Sep 2026 10:21:23 GMT`
    let value = value.trim();
    let rest = value.split_once(", ")?.1;
    let mut parts = rest.split(' ');
    let day: u8 = parts.next()?.parse().ok()?;
    let month = match parts.next()? {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let year: i32 = parts.next()?.parse().ok()?;
    let mut clock = parts.next()?.split(':');
    let hour: u8 = clock.next()?.parse().ok()?;
    let minute: u8 = clock.next()?.parse().ok()?;
    let second: u8 = clock.next()?.parse().ok()?;
    // Anything other than GMT would need a zone table; HTTP mandates GMT.
    if parts.next()? != "GMT" {
        return None;
    }
    Some((year, month, day, hour, minute, second))
}

/// A read-only description of a mirror registration, safe to generate anywhere.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct MirrorPlan {
    /// Mirror this plan would register, by osdk's own id.
    pub mirror_id: String,
    pub endpoint: String,
    pub shape: RegistrationShape,
    /// Name the source will carry once registered.
    pub source_name: String,
    pub commands: Vec<PlannedCommand>,
    pub consequences: Vec<Consequence>,
    /// Command that undoes this plan, when one exists.
    pub rollback: Option<PlannedCommand>,
    /// Fingerprint of the registration state this plan was computed from.
    ///
    /// Confirming a plan means confirming *this* state; applying re-derives it
    /// and refuses on a mismatch.
    pub fingerprint: String,
}

impl MirrorPlan {
    /// Build the plan for replacing winget's default source with a mirror.
    ///
    /// Generated only after [`assess_feasibility`] passes, so a plan the user is
    /// shown is one that can actually run. The command sequence mirrors what the
    /// mirror operators document, and the rollback is winget's own `source
    /// reset`, which restores the built-in definition rather than trying to
    /// re-add the official URL from osdk's own constants -- winget knows the
    /// correct built-in source better than a hardcoded endpoint does.
    pub fn replace_default(mirror_id: &str, endpoint: &str, registered: &[SourceRecord]) -> Self {
        let source_name = super::mirror::DEFAULT_WINGET_SOURCE_NAME;
        let program = "winget".to_owned();

        Self {
            mirror_id: mirror_id.to_owned(),
            endpoint: endpoint.to_owned(),
            shape: RegistrationShape::ReplaceDefault,
            source_name: source_name.to_owned(),
            commands: vec![
                PlannedCommand {
                    program: program.clone(),
                    args: vec![
                        "source".to_owned(),
                        "remove".to_owned(),
                        "--name".to_owned(),
                        source_name.to_owned(),
                        "--disable-interactivity".to_owned(),
                    ],
                },
                PlannedCommand {
                    program: program.clone(),
                    args: vec![
                        "source".to_owned(),
                        "add".to_owned(),
                        "--name".to_owned(),
                        source_name.to_owned(),
                        "--arg".to_owned(),
                        endpoint.to_owned(),
                        "--type".to_owned(),
                        "Microsoft.PreIndexed.Package".to_owned(),
                        // Required from winget 1.8 on, per the mirror's own
                        // instructions; harmless on older clients.
                        "--trust-level".to_owned(),
                        "trusted".to_owned(),
                        "--accept-source-agreements".to_owned(),
                        "--disable-interactivity".to_owned(),
                    ],
                },
            ],
            consequences: vec![
                Consequence::NeedsAdministrator,
                Consequence::MachineWide,
                Consequence::OfficialSourceRemoved,
                Consequence::BrieflyWithoutAnySource,
                Consequence::LosesStoreOriginTrust,
                Consequence::IndexOnlyAcceleration,
            ],
            rollback: Some(PlannedCommand {
                program,
                args: vec![
                    "source".to_owned(),
                    "reset".to_owned(),
                    "--name".to_owned(),
                    source_name.to_owned(),
                    "--force".to_owned(),
                    "--disable-interactivity".to_owned(),
                ],
            }),
            fingerprint: fingerprint_sources(registered),
        }
    }
}

/// Fingerprint the registration list a plan was computed from.
///
/// Covers each source's name, endpoint and trust, because a change to any of
/// them changes what applying would do. Sorted first so an ordering difference
/// from winget cannot invalidate an otherwise identical plan.
pub fn fingerprint_sources(registered: &[SourceRecord]) -> String {
    let mut entries: Vec<String> = registered
        .iter()
        .map(|source| {
            format!(
                "{}\u{1f}{}\u{1f}{:?}",
                source.name,
                source.endpoint.as_deref().unwrap_or(""),
                source.trust
            )
        })
        .collect();
    entries.sort();

    let mut hasher = Sha256::new();
    for entry in &entries {
        hasher.update(entry.as_bytes());
        hasher.update([0x1e]);
    }
    format!("{:x}", hasher.finalize())
}

/// What happened when a plan ran.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ApplyOutcome {
    /// Commands that completed successfully, in order.
    pub completed: Vec<String>,
    /// The command that failed, when one did.
    pub failed: Option<String>,
    /// Exit code of the failing command.
    pub exit_code: Option<i32>,
    /// Whether the rollback ran, and whether it succeeded.
    pub rolled_back: Option<bool>,
}

/// Why a plan was not run at all.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case", tag = "reason", content = "detail")]
pub enum ApplyRefused {
    /// No confirmation was supplied for a state-changing plan.
    NotConfirmed { fingerprint: String },
    /// The supplied confirmation does not match this plan.
    ///
    /// Distinct from [`Self::StateChanged`]: a wrong fingerprint is a typo or a
    /// stale copy-paste, while a changed state means the host moved underneath
    /// the plan.
    WrongFingerprint { expected: String, supplied: String },
    /// The registered sources changed after the plan was built.
    StateChanged { planned: String, current: String },
}

/// Run a confirmed plan, rolling back if one of its commands fails.
///
/// # Why the rollback is unconditional on failure
///
/// The sequence removes the only package source before adding its replacement,
/// so a failure between the two leaves winget with no source at all -- and the
/// most likely failure is precisely the one measured on a real host, a mirror
/// older than the installed package rejected with `0x80073D06`. Leaving the user
/// there would be worse than never having started, so a failed apply runs
/// `winget source reset` before returning. The rollback's own success is
/// reported rather than assumed, because a rollback that silently failed is the
/// one outcome the user must not be told is fine.
///
/// State is re-read immediately before running and compared against the
/// fingerprint the plan was built from, the same `StaleInput` guard
/// `container/apply.rs` applies to a config file.
pub fn apply_plan(
    runner: &dyn crate::process::CommandRunner,
    plan: &MirrorPlan,
    current_sources: &[SourceRecord],
    accepted_fingerprint: Option<&str>,
) -> std::result::Result<ApplyOutcome, ApplyRefused> {
    let Some(accepted) = accepted_fingerprint else {
        return Err(ApplyRefused::NotConfirmed {
            fingerprint: plan.fingerprint.clone(),
        });
    };
    if accepted != plan.fingerprint {
        return Err(ApplyRefused::WrongFingerprint {
            expected: plan.fingerprint.clone(),
            supplied: accepted.to_owned(),
        });
    }

    // Re-derive the state now: a plan confirmed a minute ago may describe a host
    // that no longer exists.
    let current = fingerprint_sources(current_sources);
    if current != plan.fingerprint {
        return Err(ApplyRefused::StateChanged {
            planned: plan.fingerprint.clone(),
            current,
        });
    }

    let mut completed = Vec::new();
    for command in &plan.commands {
        let spec = command.to_spec();
        let status = runner.run_foreground(&spec);
        let ok = status.as_ref().map(|s| s.success()).unwrap_or(false);
        if ok {
            completed.push(command.display());
            continue;
        }

        let exit_code = status.ok().and_then(|s| s.code());
        let rolled_back = plan.rollback.as_ref().map(|rollback| {
            runner
                .run_foreground(&rollback.to_spec())
                .map(|s| s.success())
                .unwrap_or(false)
        });
        return Ok(ApplyOutcome {
            completed,
            failed: Some(command.display()),
            exit_code,
            rolled_back,
        });
    }

    Ok(ApplyOutcome {
        completed,
        failed: None,
        exit_code: None,
        rolled_back: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::syspkg::mirror::{Acceleration, MirrorMeasurement};
    use crate::syspkg::report::SourceTrust;

    fn source(name: &str, endpoint: &str, trust: SourceTrust) -> SourceRecord {
        SourceRecord {
            identifier: format!("{name}.Id"),
            name: name.to_owned(),
            endpoint: Some(endpoint.to_owned()),
            kind: Some("Microsoft.PreIndexed.Package".to_owned()),
            trust,
        }
    }

    #[test]
    fn the_fingerprint_ignores_the_order_winget_happened_to_list_sources_in() {
        let a = source("winget", "https://cdn.example/cache", SourceTrust::Trusted);
        let b = source("msstore", "https://store.example/v9", SourceTrust::Trusted);

        assert_eq!(
            fingerprint_sources(&[a.clone(), b.clone()]),
            fingerprint_sources(&[b, a]),
            "a reordered listing is the same state and must not invalidate a plan"
        );
    }

    #[test]
    fn a_changed_endpoint_changes_the_fingerprint() {
        let before = [source(
            "winget",
            "https://cdn.example/cache",
            SourceTrust::Trusted,
        )];
        let after = [source(
            "winget",
            "https://mirror.example/cache",
            SourceTrust::Trusted,
        )];

        // This is the case the fingerprint exists for: the name is unchanged, so
        // only the endpoint reveals that the source was repointed.
        assert_ne!(
            fingerprint_sources(&before),
            fingerprint_sources(&after),
            "a repointed source must invalidate a plan computed before it"
        );
    }

    #[test]
    fn a_changed_trust_level_changes_the_fingerprint() {
        let before = [source(
            "m",
            "https://mirror.example/s",
            SourceTrust::Trusted,
        )];
        let after = [source(
            "m",
            "https://mirror.example/s",
            SourceTrust::Untrusted,
        )];

        assert_ne!(fingerprint_sources(&before), fingerprint_sources(&after));
    }

    #[test]
    fn adding_a_source_changes_the_fingerprint() {
        let before = [source(
            "winget",
            "https://cdn.example/cache",
            SourceTrust::Trusted,
        )];
        let after = [
            source("winget", "https://cdn.example/cache", SourceTrust::Trusted),
            source("extra", "https://other.example/s", SourceTrust::Trusted),
        ];

        assert_ne!(fingerprint_sources(&before), fingerprint_sources(&after));
    }

    #[test]
    fn an_empty_registration_list_still_fingerprints() {
        // A host whose sources could not be read must not panic or collide with
        // a populated host.
        let empty = fingerprint_sources(&[]);
        let populated =
            fingerprint_sources(&[source("winget", "https://c/e", SourceTrust::Trusted)]);

        assert!(!empty.is_empty());
        assert_ne!(empty, populated);
    }

    #[test]
    fn field_boundaries_cannot_be_forged_by_a_name_containing_the_separator() {
        // Without a delimiter between fields, "a" + "bc" and "ab" + "c" would
        // hash identically, letting a crafted source name impersonate a state.
        let one = [source("a", "bc", SourceTrust::Trusted)];
        let two = [source("ab", "c", SourceTrust::Trusted)];

        assert_ne!(fingerprint_sources(&one), fingerprint_sources(&two));
    }

    #[test]
    fn a_planned_command_renders_the_way_a_user_would_type_it() {
        let command = PlannedCommand {
            program: "winget".to_owned(),
            args: vec![
                "source".to_owned(),
                "add".to_owned(),
                "--name".to_owned(),
                "ustc".to_owned(),
            ],
        };

        assert_eq!(command.display(), "winget source add --name ustc");
    }

    #[test]
    fn an_argument_with_a_space_is_quoted_so_a_copied_line_still_works() {
        let command = PlannedCommand {
            program: "winget".to_owned(),
            args: vec!["--name".to_owned(), "my mirror".to_owned()],
        };

        assert_eq!(command.display(), "winget --name \"my mirror\"");
    }

    const OFFICIAL_AT: &str = "Tue, 15 Sep 2026 17:45:23 GMT";
    const USTC_AT: &str = "Tue, 15 Sep 2026 10:21:23 GMT";
    const NEWER_AT: &str = "Wed, 16 Sep 2026 09:00:00 GMT";

    #[test]
    fn a_mirror_older_than_the_installed_package_is_refused_up_front() {
        // The real failure this prevents: USTC published at 10:21 (version 1105)
        // while the host has the 17:45 build (version 1714), and winget rejects
        // the older package with 0x80073D06.
        let verdict = assess_feasibility(Some(USTC_AT), Some(OFFICIAL_AT), "https://m/s", true);

        assert_eq!(
            verdict,
            Err(Infeasible::MirrorIsStale {
                mirror_last_modified: USTC_AT.to_owned(),
                installed_last_modified: OFFICIAL_AT.to_owned(),
            })
        );
    }

    #[test]
    fn a_mirror_newer_than_the_installed_package_is_allowed() {
        assert_eq!(
            assess_feasibility(Some(NEWER_AT), Some(OFFICIAL_AT), "https://m/s", true),
            Ok(())
        );
    }

    #[test]
    fn an_identical_publish_time_is_allowed_rather_than_refused() {
        // Equal is not stale: re-adding the same package succeeds.
        assert_eq!(
            assess_feasibility(Some(OFFICIAL_AT), Some(OFFICIAL_AT), "https://m/s", true),
            Ok(())
        );
    }

    #[test]
    fn an_unknown_publish_time_is_refused_rather_than_assumed_fresh() {
        // Proceeding hopefully here would cause the exact failure the check
        // exists to prevent.
        assert_eq!(
            assess_feasibility(None, Some(OFFICIAL_AT), "https://m/s", true),
            Err(Infeasible::PublishTimeUnknown {
                endpoint: "https://m/s".to_owned()
            })
        );
        assert_eq!(
            assess_feasibility(Some(NEWER_AT), None, "https://m/s", true),
            Err(Infeasible::PublishTimeUnknown {
                endpoint: "https://m/s".to_owned()
            })
        );
    }

    #[test]
    fn an_unparsable_date_is_not_evidence_of_freshness() {
        // Huawei's mirror answers 200 with an HTML page for any path, so a body
        // or header that is not a date must never be read as "newer".
        assert_eq!(
            assess_feasibility(
                Some("<!DOCTYPE html>"),
                Some(OFFICIAL_AT),
                "https://m/s",
                true
            ),
            Err(Infeasible::PublishTimeUnknown {
                endpoint: "https://m/s".to_owned()
            })
        );
    }

    #[test]
    fn an_unreachable_mirror_is_refused_before_any_date_comparison() {
        assert_eq!(
            assess_feasibility(Some(NEWER_AT), Some(OFFICIAL_AT), "https://m/s", false),
            Err(Infeasible::MirrorUnreachable {
                endpoint: "https://m/s".to_owned()
            })
        );
    }

    #[test]
    fn dates_are_ordered_across_a_month_boundary() {
        let august = "Sun, 31 Aug 2026 23:59:59 GMT";
        let september = "Tue, 01 Sep 2026 00:00:01 GMT";

        assert_eq!(
            assess_feasibility(Some(august), Some(september), "https://m/s", true),
            Err(Infeasible::MirrorIsStale {
                mirror_last_modified: august.to_owned(),
                installed_last_modified: september.to_owned(),
            }),
            "August must order before September, not by string comparison"
        );
        assert_eq!(
            assess_feasibility(Some(september), Some(august), "https://m/s", true),
            Ok(())
        );
    }

    #[test]
    fn a_month_name_that_sorts_wrong_alphabetically_still_orders_by_time() {
        // The case that catches a string comparison: "Dec" < "Sep" as text, but
        // December is *later* than September. Comparing the raw headers would
        // call the newer mirror stale and refuse a valid apply.
        let september = "Tue, 15 Sep 2026 10:00:00 GMT";
        let december = "Tue, 15 Dec 2026 10:00:00 GMT";

        assert_eq!(
            assess_feasibility(Some(december), Some(september), "https://m/s", true),
            Ok(()),
            "December is newer than September despite sorting earlier as text"
        );
        assert_eq!(
            assess_feasibility(Some(september), Some(december), "https://m/s", true),
            Err(Infeasible::MirrorIsStale {
                mirror_last_modified: september.to_owned(),
                installed_last_modified: december.to_owned(),
            })
        );
    }

    #[test]
    fn the_weekday_prefix_does_not_drive_the_comparison() {
        // "Fri, 09 Oct" is older than "Thu, 15 Oct", but a comparison starting
        // at the weekday would see "Fri" > "Thu" and invert the verdict.
        let ninth = "Fri, 09 Oct 2026 10:00:00 GMT";
        let fifteenth = "Thu, 15 Oct 2026 10:00:00 GMT";

        assert_eq!(
            assess_feasibility(Some(fifteenth), Some(ninth), "https://m/s", true),
            Ok(())
        );
        assert_eq!(
            assess_feasibility(Some(ninth), Some(fifteenth), "https://m/s", true),
            Err(Infeasible::MirrorIsStale {
                mirror_last_modified: ninth.to_owned(),
                installed_last_modified: fifteenth.to_owned(),
            })
        );
    }

    #[test]
    fn a_non_gmt_zone_is_rejected_rather_than_silently_misread() {
        assert!(parse_http_date("Tue, 15 Sep 2026 10:21:23 UTC").is_none());
        assert!(parse_http_date("Tue, 15 Sep 2026 10:21:23 +0800").is_none());
    }

    #[test]
    fn the_plan_replaces_the_default_source_and_can_be_rolled_back() {
        let registered = [source("winget", "https://cdn/c", SourceTrust::Trusted)];
        let plan = MirrorPlan::replace_default(
            "ustc",
            "https://mirrors.ustc.edu.cn/winget-source",
            &registered,
        );

        assert_eq!(plan.shape, RegistrationShape::ReplaceDefault);
        assert_eq!(plan.source_name, "winget");

        // remove then add, in that order: the sequence the operators document.
        assert_eq!(plan.commands.len(), 2);
        assert!(plan.commands[0]
            .display()
            .contains("source remove --name winget"));
        assert!(plan.commands[1]
            .display()
            .contains("source add --name winget"));
        assert!(
            plan.commands[1].display().contains("--trust-level trusted"),
            "winget 1.8+ requires it for a third-party source"
        );

        // A destructive plan without a rollback is not acceptable.
        let rollback = plan.rollback.as_ref().expect("a rollback command");
        assert!(
            rollback.display().contains("source reset --name winget"),
            "reset restores winget's own built-in definition, got: {}",
            rollback.display()
        );
    }

    #[test]
    fn the_plan_discloses_every_cost_including_the_sourceless_window() {
        let plan = MirrorPlan::replace_default("ustc", "https://m/s", &[]);

        for expected in [
            Consequence::NeedsAdministrator,
            Consequence::MachineWide,
            Consequence::OfficialSourceRemoved,
            Consequence::BrieflyWithoutAnySource,
            Consequence::LosesStoreOriginTrust,
            Consequence::IndexOnlyAcceleration,
        ] {
            assert!(
                plan.consequences.contains(&expected),
                "a confirmation prompt must not omit {expected:?}"
            );
        }
    }

    #[test]
    fn the_plan_carries_the_fingerprint_of_the_state_it_was_built_from() {
        let registered = [source("winget", "https://cdn/c", SourceTrust::Trusted)];
        let plan = MirrorPlan::replace_default("ustc", "https://m/s", &registered);

        assert_eq!(plan.fingerprint, fingerprint_sources(&registered));
        assert_ne!(plan.fingerprint, fingerprint_sources(&[]));
    }

    /// A runner that fails a chosen command and records every invocation.
    struct ScriptedRunner {
        fail_on: Vec<String>,
        calls: std::sync::Mutex<Vec<String>>,
    }

    impl ScriptedRunner {
        fn failing_on(needles: &[&str]) -> Self {
            Self {
                fail_on: needles.iter().map(|n| (*n).to_owned()).collect(),
                calls: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn new(fail_on: Option<&str>) -> Self {
            Self {
                fail_on: fail_on.map(str::to_owned).into_iter().collect(),
                calls: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().expect("scripted runner lock").clone()
        }
    }

    #[cfg(windows)]
    fn scripted_status(code: i32) -> std::process::ExitStatus {
        use std::os::windows::process::ExitStatusExt;
        std::process::ExitStatus::from_raw(code as u32)
    }

    #[cfg(unix)]
    fn scripted_status(code: i32) -> std::process::ExitStatus {
        use std::os::unix::process::ExitStatusExt;
        std::process::ExitStatus::from_raw(code << 8)
    }

    impl crate::process::CommandRunner for ScriptedRunner {
        fn run_captured(
            &self,
            _command: &crate::process::CommandSpec,
            _limits: crate::process::CaptureLimits,
        ) -> crate::process::CommandOutcome {
            panic!("apply must use run_foreground so the user sees winget's own output");
        }

        fn run_foreground(
            &self,
            command: &crate::process::CommandSpec,
        ) -> std::io::Result<std::process::ExitStatus> {
            // `CommandSpec`'s Debug redacts argument values on purpose, so the
            // arguments are read through `arguments()` instead.
            let rendered = command
                .arguments()
                .iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join(" ");
            self.calls
                .lock()
                .expect("scripted runner lock")
                .push(rendered.clone());
            let fails = self
                .fail_on
                .iter()
                .any(|needle| rendered.contains(needle.as_str()));
            Ok(scripted_status(if fails { 1 } else { 0 }))
        }
    }

    fn plan_for_tests(registered: &[SourceRecord]) -> MirrorPlan {
        MirrorPlan::replace_default(
            "ustc",
            "https://mirrors.ustc.edu.cn/winget-source",
            registered,
        )
    }

    #[test]
    fn a_plan_without_confirmation_is_refused_before_anything_runs() {
        let registered = [source("winget", "https://cdn/c", SourceTrust::Trusted)];
        let plan = plan_for_tests(&registered);
        let runner = ScriptedRunner::new(None);

        let refusal = apply_plan(&runner, &plan, &registered, None);

        assert_eq!(
            refusal,
            Err(ApplyRefused::NotConfirmed {
                fingerprint: plan.fingerprint.clone()
            })
        );
        assert!(
            runner.calls().is_empty(),
            "an unconfirmed plan must not run a single command"
        );
    }

    #[test]
    fn a_mismatched_confirmation_is_refused_before_anything_runs() {
        let registered = [source("winget", "https://cdn/c", SourceTrust::Trusted)];
        let plan = plan_for_tests(&registered);
        let runner = ScriptedRunner::new(None);

        let refusal = apply_plan(&runner, &plan, &registered, Some("not-the-fingerprint"));

        assert!(matches!(
            refusal,
            Err(ApplyRefused::WrongFingerprint { .. })
        ));
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn a_plan_is_refused_when_the_host_changed_after_it_was_built() {
        let planned = [source("winget", "https://cdn/c", SourceTrust::Trusted)];
        let plan = plan_for_tests(&planned);

        // Someone added a source between planning and applying.
        let now = [
            source("winget", "https://cdn/c", SourceTrust::Trusted),
            source("extra", "https://other/s", SourceTrust::Trusted),
        ];
        let runner = ScriptedRunner::new(None);

        let refusal = apply_plan(&runner, &plan, &now, Some(&plan.fingerprint));

        assert!(matches!(refusal, Err(ApplyRefused::StateChanged { .. })));
        assert!(
            runner.calls().is_empty(),
            "a stale plan must not touch a host it no longer describes"
        );
    }

    #[test]
    fn a_successful_apply_runs_both_commands_and_no_rollback() {
        let registered = [source("winget", "https://cdn/c", SourceTrust::Trusted)];
        let plan = plan_for_tests(&registered);
        let runner = ScriptedRunner::new(None);

        let outcome = apply_plan(&runner, &plan, &registered, Some(&plan.fingerprint)).unwrap();

        assert_eq!(outcome.completed.len(), 2);
        assert_eq!(outcome.failed, None);
        assert_eq!(
            outcome.rolled_back, None,
            "nothing failed, so nothing to undo"
        );
        assert_eq!(runner.calls().len(), 2);
    }

    #[test]
    fn a_failed_add_rolls_back_so_the_host_is_not_left_without_a_source() {
        let registered = [source("winget", "https://cdn/c", SourceTrust::Trusted)];
        let plan = plan_for_tests(&registered);
        // The measured real-world failure: remove succeeds, add is rejected.
        let runner = ScriptedRunner::new(Some("source add"));

        let outcome = apply_plan(&runner, &plan, &registered, Some(&plan.fingerprint)).unwrap();

        assert_eq!(outcome.completed.len(), 1, "remove succeeded");
        assert!(outcome.failed.as_deref().unwrap().contains("source add"));
        assert_eq!(
            outcome.rolled_back,
            Some(true),
            "leaving winget with no source at all is the outcome this prevents"
        );

        let calls = runner.calls();
        assert_eq!(calls.len(), 3, "remove, failed add, then reset");
        assert!(
            calls[2].contains("reset"),
            "the third call must be the rollback, got: {}",
            calls[2]
        );
    }

    #[test]
    fn a_rollback_that_itself_fails_is_reported_rather_than_hidden() {
        let registered = [source("winget", "https://cdn/c", SourceTrust::Trusted)];
        let plan = plan_for_tests(&registered);
        // The worst case, and the one the user most needs told plainly: the add
        // fails and the rollback fails too. `--arg` appears only in the add and
        // `--force` only in the reset, so the remove still succeeds and the
        // sequence actually reaches the rollback.
        let runner = ScriptedRunner::failing_on(&["--arg", "--force"]);

        let outcome = apply_plan(&runner, &plan, &registered, Some(&plan.fingerprint)).unwrap();

        assert!(
            outcome.failed.is_some(),
            "the add must fail for a rollback to be attempted at all"
        );
        assert_eq!(
            outcome.rolled_back,
            Some(false),
            "a failed rollback must never be reported as fine"
        );
    }

    #[test]
    fn index_only_acceleration_is_always_disclosed_for_winget() {
        let measurement = MirrorMeasurement {
            source_id: "ustc".to_owned(),
            endpoint: "https://mirrors.ustc.edu.cn/winget-source".to_owned(),
            kind: crate::source::SourceKind::Mirror,
            acceleration: Acceleration::IndexOnly,
            reachable: Some(true),
            ttfb_ms: Some(100),
            throughput: Some(5_000_000.0),
        };

        assert!(!measurement.acceleration.accelerates_downloads());
    }
}
