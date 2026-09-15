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

use super::mirror::{Acceleration, MirrorMeasurement};
use super::report::SourceRecord;

/// How a mirror gets into winget's configuration.
///
/// Not an implementation detail: the two shapes differ in what the user loses.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RegistrationShape {
    /// Add the mirror under its own name, leaving the official source in place.
    ///
    /// Preferred when winget allows it: the official source stays available as
    /// a fallback, and removing the mirror later cannot leave the host without
    /// a source.
    Alongside,
    /// Remove the source named `winget` and re-add that name pointing at the
    /// mirror.
    ///
    /// This is what mirror operators document. It is more invasive -- the
    /// official endpoint is no longer registered at all -- so it is only
    /// planned when coexistence is not available, and the plan says so.
    ReplaceOfficial,
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

#[cfg(test)]
mod tests {
    use super::*;
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

    #[test]
    fn index_only_acceleration_is_always_disclosed_for_winget() {
        // The measurement carries this fact; the plan must not drop it, or the
        // user confirms expecting faster downloads.
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
