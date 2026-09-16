//! Mirror acceleration for the Linux distribution managers.
//!
//! The shape here is different from winget's, and the difference is the point.
//! winget can only replace the machine-wide default source: its mirrors install
//! as a fixed MSIX identity, so coexistence is impossible and acceleration means
//! changing global state. apt can be pointed at a mirror for the duration of one
//! command, touching nothing outside a temporary directory. Measured on Ubuntu
//! 22.04: `/var/cache/apt/pkgcache.bin` unchanged by md5, `/var/lib/apt/lists`
//! still 53 entries, nothing written under `/etc/apt`.
//!
//! So the default is the ephemeral form. Rewriting `sources.list` stays possible
//! for someone who wants the whole host accelerated, but it is no longer the
//! only way to get a fast download, and it is the more dangerous one: a broken
//! sources.list stops the machine installing anything at all.
//!
//! Two findings from that measurement are load-bearing, and both were failures
//! that looked like successes:
//!
//! - **`Dir::Cache` cannot be omitted.** With only `Dir::Cache::Archives` set,
//!   apt still writes `/var/cache/apt/pkgcache.bin`. As an unprivileged user
//!   that is refused by permissions, so the run looks clean; as root it really
//!   does modify system cache. "Touches nothing" held by accident.
//! - **Under root the sandbox is silently lost.** apt drops to the `_apt` user
//!   to download, cannot read a default-permission temporary directory, prints
//!   "Download is performed unsandboxed as root" and carries on. That is a real
//!   security property abandoned, not noise.

use std::path::Path;

use serde::Serialize;

use super::distro::DistroManager;
use crate::process::CommandSpec;

/// A distribution mirror osdk knows about.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct DistroMirror {
    pub name: &'static str,
    /// Base URL, without the distribution-specific suffix.
    pub base: &'static str,
}

/// Debian and Ubuntu mirrors.
///
/// Ranked by measurement, never by reputation: on one network at one moment
/// these three differed by 14x, and not in the order their names would suggest
/// (Aliyun 5878, USTC 2040, TUNA 411 KB/s).
pub const DEBIAN_MIRRORS: &[DistroMirror] = &[
    DistroMirror {
        name: "aliyun",
        base: "https://mirrors.aliyun.com",
    },
    DistroMirror {
        name: "ustc",
        base: "https://mirrors.ustc.edu.cn",
    },
    DistroMirror {
        name: "tuna",
        base: "https://mirrors.tuna.tsinghua.edu.cn",
    },
];

/// Alpine mirrors.
pub const ALPINE_MIRRORS: &[DistroMirror] = &[
    DistroMirror {
        name: "aliyun",
        base: "https://mirrors.aliyun.com",
    },
    DistroMirror {
        name: "ustc",
        base: "https://mirrors.ustc.edu.cn",
    },
    DistroMirror {
        name: "tuna",
        base: "https://mirrors.tuna.tsinghua.edu.cn",
    },
];

/// Mirrors for a manager, or none when osdk has no candidates for it.
///
/// pacman and dnf return nothing on purpose. Their mirror configuration is a
/// mirrorlist file with its own ranking machinery (`reflector`,
/// `dnf-plugin-fastestmirror`), and there is no per-invocation override with the
/// properties apt's has. Offering a half-working version would be worse than
/// saying osdk does not do it.
pub fn mirrors_for(manager: DistroManager) -> &'static [DistroMirror] {
    match manager {
        DistroManager::Apt => DEBIAN_MIRRORS,
        DistroManager::Apk => ALPINE_MIRRORS,
        DistroManager::Pacman | DistroManager::Dnf => &[],
    }
}

/// Which distribution a Debian-family host is running.
///
/// Needed because the URL path differs: Debian lives under `/debian`, Ubuntu
/// under `/ubuntu`, and pointing one at the other's path yields a mirror that
/// resolves but serves the wrong packages.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DebianFlavour {
    /// `debian` or `ubuntu`.
    pub distribution: String,
    /// Release codename, such as `bookworm` or `jammy`.
    pub codename: String,
}

/// Read the distribution and codename from `/etc/os-release`.
///
/// Parsed from the file rather than asked of `lsb_release`, which is not
/// installed in minimal images -- exactly the images this is for.
pub fn read_flavour(os_release: &str) -> Option<DebianFlavour> {
    let mut id = None;
    let mut codename = None;

    for line in os_release.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim().trim_matches('"').to_owned();
        match key.trim() {
            "ID" => id = Some(value),
            "VERSION_CODENAME" => codename = Some(value),
            _ => {}
        }
    }

    let id = id?;
    let codename = codename?;
    if codename.is_empty() {
        return None;
    }

    // Derivatives report their own ID but are served from the base
    // distribution's path. Anything not recognised is declined rather than
    // guessed: a wrong path gives a mirror that works and serves the wrong
    // packages, which is harder to notice than an outright failure.
    let distribution = match id.as_str() {
        "debian" => "debian",
        "ubuntu" => "ubuntu",
        _ => return None,
    };

    Some(DebianFlavour {
        distribution: distribution.to_owned(),
        codename,
    })
}

/// The `sources.list` body that points apt at one mirror.
pub fn apt_sources_list(mirror: &DistroMirror, flavour: &DebianFlavour) -> String {
    let base = mirror.base.trim_end_matches('/');
    let DebianFlavour {
        distribution,
        codename,
    } = flavour;

    // Ubuntu and Debian both name the suite `<codename>-security`. On the
    // official infrastructure Debian serves it from a separate host, but on a
    // mirror everything lives under one base, so only the components differ.
    let components = if distribution == "ubuntu" {
        "main restricted universe multiverse"
    } else {
        "main contrib non-free non-free-firmware"
    };

    format!(
        "deb {base}/{distribution}/ {codename} {components}\n\
         deb {base}/{distribution}/ {codename}-updates {components}\n\
         deb {base}/{distribution}/ {codename}-security {components}\n"
    )
}

/// The options that confine an apt invocation to a scratch directory.
///
/// Every entry here was necessary in measurement. `Dir::Cache` in particular is
/// not redundant with `Dir::Cache::Archives`: without it apt writes the system
/// package cache, which only permissions were preventing.
pub fn apt_confinement_options(scratch: &Path, sources_list: &Path) -> Vec<String> {
    let scratch = scratch.display();
    vec![
        format!("-oDir::Etc::SourceList={}", sources_list.display()),
        // An empty directory, so nothing from /etc/apt/sources.list.d leaks in.
        format!("-oDir::Etc::SourceParts={scratch}/empty"),
        format!("-oDir::State::Lists={scratch}/lists"),
        format!("-oDir::State={scratch}"),
        format!("-oDir::Cache={scratch}"),
        // Translation files triple the index download and are never read here.
        "-oAcquire::Languages=none".to_owned(),
    ]
}

/// Whether apt would lose its download sandbox in this scratch directory.
///
/// Running as root, apt drops to the `_apt` user to fetch. If that user cannot
/// traverse the scratch directory, apt prints a warning and continues without
/// the sandbox rather than failing -- so this has to be arranged in advance,
/// not detected afterwards.
pub const fn scratch_needs_apt_ownership(is_root: bool) -> bool {
    is_root
}

/// A prepared scratch directory that makes one apt invocation use a mirror.
///
/// Holds the directory alive: dropping it removes everything, which is what
/// keeps "touches nothing outside a temporary directory" true even when an
/// install fails partway.
#[derive(Debug)]
pub struct EphemeralAptSource {
    directory: tempfile::TempDir,
    /// The mirror this source points at, for reporting.
    pub mirror: &'static str,
    /// Options to add to the apt command line.
    pub options: Vec<String>,
}

impl EphemeralAptSource {
    /// Where the scratch lives, for diagnostics.
    pub fn path(&self) -> &Path {
        self.directory.path()
    }
}

/// Build an ephemeral apt source pointing at `mirror`.
///
/// The caller passes `is_root` rather than this function probing for it, so the
/// ownership arrangement below can be exercised from a test on any machine.
///
/// Returns an error rather than silently skipping acceleration: a mirror that
/// was asked for and quietly not used is the failure mode this whole module
/// exists to avoid.
pub fn prepare_ephemeral_apt_source(
    mirror: &DistroMirror,
    flavour: &DebianFlavour,
    is_root: bool,
) -> std::io::Result<EphemeralAptSource> {
    let directory = tempfile::TempDir::new()?;
    let root = directory.path().to_path_buf();

    let lists = root.join("lists");
    let empty = root.join("empty");
    // apt requires these to exist; it will not create them itself, and the
    // error it gives when they are missing does not name the directory.
    std::fs::create_dir_all(&lists)?;
    std::fs::create_dir_all(&empty)?;
    std::fs::create_dir_all(root.join("lists/partial"))?;
    std::fs::create_dir_all(root.join("archives/partial"))?;

    let sources_list = root.join("osdk-mirror.list");
    std::fs::write(&sources_list, apt_sources_list(mirror, flavour))?;

    if scratch_needs_apt_ownership(is_root) {
        // Measured: without this apt prints "Download is performed unsandboxed
        // as root" and continues. It does not fail, so the sandbox is lost
        // silently -- and the sandbox is precisely what stops a malicious
        // archive attacking the downloader as root.
        grant_apt_user_access(&root, &[&lists, &empty])?;
    }

    Ok(EphemeralAptSource {
        directory,
        mirror: mirror.name,
        options: apt_confinement_options(&root, &sources_list),
    })
}

/// The command that fills an ephemeral source with a package index.
///
/// Not optional, and the reason is worth stating: `Dir::State::Lists` points at
/// an empty scratch, so apt starts with no index at all. Measured on Ubuntu
/// 22.04 -- without this step every install fails with `E: Unable to locate
/// package`, including packages that plainly exist. The system's own lists are
/// deliberately not consulted, which is the entire point of the confinement, so
/// the replacement has to be populated before it is useful.
///
/// The failure is especially misleading because it names the package rather
/// than the index: it reads as "that package does not exist" when the truth is
/// "nothing exists yet".
pub fn apt_refresh_command(source: &EphemeralAptSource) -> CommandSpec {
    CommandSpec::new("apt-get")
        .args(&source.options)
        .arg("update")
}

/// Let the unprivileged `_apt` user reach the scratch directory.
///
/// Best-effort by design: a host with no `_apt` user has no sandbox to lose, so
/// failing to find it is not an error. A failure to *apply* ownership that did
/// resolve is an error, because that is the case where apt would carry on
/// unsandboxed.
#[cfg(unix)]
fn grant_apt_user_access(root: &Path, owned: &[&Path]) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    // Traversable, so `_apt` can descend; not writable by others.
    std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o755))?;

    let Some(uid) = apt_user_id() else {
        return Ok(());
    };
    for path in owned {
        chown_recursively(path, uid)?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn grant_apt_user_access(_root: &Path, _owned: &[&Path]) -> std::io::Result<()> {
    Ok(())
}

/// The numeric uid of `_apt`, if this host has one.
#[cfg(unix)]
fn apt_user_id() -> Option<u32> {
    let passwd = std::fs::read_to_string("/etc/passwd").ok()?;
    passwd.lines().find_map(|line| {
        let mut fields = line.split(':');
        (fields.next()? == "_apt").then(|| fields.nth(1)?.parse().ok())?
    })
}

#[cfg(unix)]
fn chown_recursively(path: &Path, uid: u32) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;

    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::other("path contains a NUL byte"))?;
    // SAFETY: the pointer is a valid NUL-terminated string for this call, and
    // `chown` does not retain it.
    if unsafe { libc::chown(c_path.as_ptr(), uid, u32::MAX) } != 0 {
        return Err(std::io::Error::last_os_error());
    }

    if path.is_dir() {
        for entry in std::fs::read_dir(path)? {
            chown_recursively(&entry?.path(), uid)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ubuntu_is_recognised_with_its_codename() {
        let text = "NAME=\"Ubuntu\"\nID=ubuntu\nVERSION_CODENAME=jammy\n";

        let flavour = read_flavour(text).expect("ubuntu is recognised");

        assert_eq!(flavour.distribution, "ubuntu");
        assert_eq!(flavour.codename, "jammy");
    }

    #[test]
    fn debian_is_recognised_with_its_codename() {
        let text = "ID=debian\nVERSION_CODENAME=bookworm\n";

        let flavour = read_flavour(text).expect("debian is recognised");

        assert_eq!(flavour.distribution, "debian");
        assert_eq!(flavour.codename, "bookworm");
    }

    #[test]
    fn an_unknown_distribution_is_declined_rather_than_guessed() {
        // Serving Ubuntu paths to a distribution that is not Ubuntu produces a
        // mirror that resolves and serves the wrong packages -- worse than no
        // mirror, because it looks like it worked.
        let text = "ID=gentoo\nVERSION_CODENAME=whatever\n";

        assert!(read_flavour(text).is_none());
    }

    #[test]
    fn a_release_without_a_codename_is_declined() {
        let text = "ID=debian\nVERSION_ID=\"12\"\n";

        assert!(read_flavour(text).is_none());
    }

    #[test]
    fn the_sources_list_names_the_mirror_and_never_the_official_host() {
        let flavour = DebianFlavour {
            distribution: "ubuntu".to_owned(),
            codename: "jammy".to_owned(),
        };

        let body = apt_sources_list(&DEBIAN_MIRRORS[0], &flavour);

        assert!(body.contains("mirrors.aliyun.com/ubuntu/ jammy main"));
        assert!(body.contains("jammy-updates"));
        assert!(body.contains("jammy-security"));
        assert!(
            !body.contains("archive.ubuntu.com") && !body.contains("security.ubuntu.com"),
            "an ephemeral source that still names the official host is not a mirror: {body}"
        );
    }

    #[test]
    fn debian_gets_its_own_components_not_ubuntus() {
        let flavour = DebianFlavour {
            distribution: "debian".to_owned(),
            codename: "bookworm".to_owned(),
        };

        let body = apt_sources_list(&DEBIAN_MIRRORS[1], &flavour);

        assert!(body.contains("main contrib non-free"));
        assert!(
            !body.contains("universe"),
            "universe is an Ubuntu component and does not exist on Debian: {body}"
        );
    }

    #[test]
    fn confinement_redirects_the_package_cache_not_only_the_archives() {
        // The measured trap: with only Dir::Cache::Archives, apt writes
        // /var/cache/apt/pkgcache.bin. Unprivileged that is refused and the run
        // looks clean; as root it modifies system state.
        let options = apt_confinement_options(Path::new("/tmp/scratch"), Path::new("/tmp/s.list"));

        assert!(
            options.iter().any(|o| o.starts_with("-oDir::Cache=")),
            "Dir::Cache must be redirected, not just its Archives subkey: {options:?}"
        );
        assert!(options.iter().any(|o| o.starts_with("-oDir::State=")));
        assert!(options
            .iter()
            .any(|o| o.starts_with("-oDir::Etc::SourceParts=")));
    }

    #[test]
    fn confinement_points_at_the_given_sources_list() {
        let options =
            apt_confinement_options(Path::new("/tmp/scratch"), Path::new("/tmp/mirror.list"));

        assert!(options
            .iter()
            .any(|o| o == "-oDir::Etc::SourceList=/tmp/mirror.list"));
    }

    #[test]
    fn managers_without_a_usable_override_offer_no_mirrors() {
        // Not an oversight: pacman and dnf configure mirrors through a
        // mirrorlist with its own ranking tools, and have no per-invocation
        // override with these properties.
        assert!(mirrors_for(DistroManager::Pacman).is_empty());
        assert!(mirrors_for(DistroManager::Dnf).is_empty());
        assert!(!mirrors_for(DistroManager::Apt).is_empty());
        assert!(!mirrors_for(DistroManager::Apk).is_empty());
    }

    #[test]
    fn a_prepared_source_creates_every_directory_apt_requires() {
        // apt does not create these itself, and the error when they are missing
        // does not name the directory, so a missing one costs real debugging.
        let flavour = DebianFlavour {
            distribution: "ubuntu".to_owned(),
            codename: "jammy".to_owned(),
        };

        let source = prepare_ephemeral_apt_source(&DEBIAN_MIRRORS[0], &flavour, false)
            .expect("the scratch is created");
        let root = source.path();

        for required in ["lists", "empty", "lists/partial", "archives/partial"] {
            assert!(
                root.join(required).is_dir(),
                "apt needs {required} to exist"
            );
        }
        assert!(root.join("osdk-mirror.list").is_file());
    }

    #[test]
    fn the_prepared_source_file_contains_the_mirror() {
        let flavour = DebianFlavour {
            distribution: "debian".to_owned(),
            codename: "bookworm".to_owned(),
        };

        let source = prepare_ephemeral_apt_source(&DEBIAN_MIRRORS[1], &flavour, false)
            .expect("the scratch is created");
        let body = std::fs::read_to_string(source.path().join("osdk-mirror.list"))
            .expect("the list is readable");

        assert!(body.contains("mirrors.ustc.edu.cn/debian/ bookworm"));
        assert_eq!(source.mirror, "ustc");
    }

    #[test]
    fn dropping_the_source_removes_everything_it_made() {
        // "Touches nothing outside a temporary directory" has to survive a
        // failed install, not just a successful one.
        let flavour = DebianFlavour {
            distribution: "ubuntu".to_owned(),
            codename: "jammy".to_owned(),
        };
        let path = {
            let source = prepare_ephemeral_apt_source(&DEBIAN_MIRRORS[0], &flavour, false)
                .expect("the scratch is created");
            source.path().to_path_buf()
        };

        assert!(!path.exists(), "the scratch outlived its owner: {path:?}");
    }

    #[test]
    fn the_options_point_inside_the_scratch_and_nowhere_else() {
        let flavour = DebianFlavour {
            distribution: "ubuntu".to_owned(),
            codename: "jammy".to_owned(),
        };

        let source = prepare_ephemeral_apt_source(&DEBIAN_MIRRORS[0], &flavour, false)
            .expect("the scratch is created");
        let root = source.path().display().to_string();

        for option in &source.options {
            // Acquire::Languages is the one option that names no path.
            if option.contains("Languages") {
                continue;
            }
            let (_, value) = option.split_once('=').expect("every option has a value");
            assert!(
                value.starts_with(&root),
                "an option escaped the scratch directory: {option}"
            );
        }
    }

    #[test]
    fn root_is_the_only_case_that_needs_the_apt_user_arrangement() {
        // Measured: as root, apt drops to `_apt` to download and silently
        // abandons the sandbox if it cannot traverse the scratch. Unprivileged
        // runs never drop, so there is nothing to arrange.
        assert!(scratch_needs_apt_ownership(true));
        assert!(!scratch_needs_apt_ownership(false));
    }
}
