//! Creates and manages AVDs directly, without going through `avdmanager`.
//!
//! ## Why osdk writes these files itself
//!
//! `avdmanager` cannot be used against an osdk-managed SDK root. Measured on
//! cmdline-tools 23.0.0 / emulator 37.1.11:
//!
//! 1. It derives the SDK root by canonicalising its own jar path and walking up.
//!    osdk exposes `cmdline-tools/latest` as a directory link into the versioned
//!    install, so the walk resolves *through* the link and lands one level above
//!    the real root. It then reports every package as being in an "inconsistent
//!    location" and indexes none of them.
//! 2. `ANDROID_SDK_ROOT` and `ANDROID_HOME` do not override that, and
//!    `create avd` rejects `--sdk_root` outright: the only global flags it takes
//!    are `-s` and `-v`.
//!
//! So there is no flag, and no environment variable, that makes it agree with a
//! linked layout. Writing the two files it would have written is both simpler and
//! more predictable than reshaping the install to satisfy its path arithmetic.
//!
//! ## The one thing it got wrong anyway
//!
//! When avdmanager does succeed it writes a **relative** `image.sysdir.1`
//! (`system-images\android-35\google_apis\x86_64\`), which only resolves against
//! the root *it* derived. With a linked layout that is the wrong directory, so
//! the emulator then fails with `Broken AVD system path`. osdk writes an absolute
//! path instead, which cannot be misread: an AVD created this way boots whatever
//! root the emulator happens to pick.
//!
//! ## Fidelity of the template
//!
//! The hardware defaults below were taken from a `config.ini` avdmanager itself
//! produced, and the only difference in the file osdk writes is the absolute
//! `image.sysdir.1`. That was verified by booting both.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// The `avd/` directory holding devices and their `.ini` pointers.
pub fn avd_home(dirs: &crate::dirs::Dirs) -> PathBuf {
    dirs.data.join("avd")
}

/// A parsed system image id: `android-35;google_apis;x86_64`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageId {
    /// The platform segment, e.g. `android-35`.
    pub platform: String,
    /// The vendor tag, e.g. `google_apis`.
    pub tag: String,
    /// The ABI, e.g. `x86_64`.
    pub abi: String,
}

impl ImageId {
    /// Parse the three-segment form the manifest and `osdk install` both use.
    ///
    /// A `system-images;` prefix is accepted and dropped so the same string works
    /// whether it came from a manifest path or from `osdk list`.
    pub fn parse(value: &str) -> Result<ImageId> {
        let trimmed = value
            .trim()
            .strip_prefix("system-images;")
            .unwrap_or(value.trim());
        let parts: Vec<&str> = trimmed.split(';').collect();
        if parts.len() != 3 || parts.iter().any(|part| part.is_empty()) {
            return Err(Error::other(format!(
                "`{value}` is not a system image id; expected \
                 `android-<api>;<tag>;<abi>`, e.g. `android-35;google_apis;x86_64`"
            )));
        }
        Ok(ImageId {
            platform: parts[0].to_string(),
            tag: parts[1].to_string(),
            abi: parts[2].to_string(),
        })
    }

    /// The id as osdk stores it, which is also the install version.
    pub fn as_version(&self) -> String {
        format!("{};{};{}", self.platform, self.tag, self.abi)
    }

    /// The api level, when the platform segment carries one.
    pub fn api_level(&self) -> Option<&str> {
        self.platform.strip_prefix("android-")
    }

    /// The CPU architecture the emulator expects for this ABI.
    ///
    /// `hw.cpu.arch` is not the ABI: a 32-bit x86 image runs an `x86` CPU while
    /// arm64 images report `arm64`, and getting this wrong makes the emulator
    /// refuse the image.
    pub fn cpu_arch(&self) -> &str {
        match self.abi.as_str() {
            "x86_64" => "x86_64",
            "x86" => "x86",
            "arm64-v8a" => "arm64",
            "armeabi-v7a" => "arm",
            other => other,
        }
    }
}

/// A device osdk knows about.
#[derive(Debug, Clone)]
pub struct Avd {
    /// Device name, without the `.avd` suffix.
    pub name: String,
    /// The `<name>.avd` directory.
    pub path: PathBuf,
    /// Value of `image.sysdir.1`, when the config could be read.
    pub image_dir: Option<String>,
    /// Value of `target`, e.g. `android-35`.
    pub target: Option<String>,
}

impl Avd {
    /// Whether the recorded system image directory still resolves.
    ///
    /// Worth reporting separately from existence: an AVD whose image was
    /// uninstalled looks fine until the emulator dies on it.
    pub fn image_present(&self) -> bool {
        self.image_dir
            .as_deref()
            .map(|dir| Path::new(dir).join("system.img").is_file())
            .unwrap_or(false)
    }
}

/// Read one `key=value` from an ini-style file.
fn ini_value(text: &str, key: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let (name, value) = line.split_once('=')?;
        (name.trim() == key).then(|| value.trim().to_string())
    })
}

/// List the devices under osdk's AVD home.
pub fn list(dirs: &crate::dirs::Dirs) -> Vec<Avd> {
    let home = avd_home(dirs);
    let Ok(entries) = std::fs::read_dir(&home) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_suffix(".avd"))
        else {
            continue;
        };
        let config = std::fs::read_to_string(path.join("config.ini")).unwrap_or_default();
        found.push(Avd {
            name: name.to_string(),
            image_dir: ini_value(&config, "image.sysdir.1"),
            target: ini_value(&config, "target"),
            path: path.clone(),
        });
    }
    found.sort_by(|a, b| a.name.cmp(&b.name));
    found
}

/// Options for a new device.
#[derive(Debug, Clone, Default)]
pub struct CreateOptions {
    /// Replace an existing device of the same name.
    pub force: bool,
    /// Userdata partition size, e.g. `8G`.
    pub data_size: Option<String>,
    /// SD card size, e.g. `512M`.
    pub sdcard_size: Option<String>,
}

/// Render an AVD's `config.ini`.
///
/// `image_dir` must be absolute: a relative value is only meaningful against the
/// SDK root the emulator picks at runtime, which is exactly the ambiguity that
/// makes avdmanager's own output unreliable under a linked layout.
pub fn render_config(
    name: &str,
    image: &ImageId,
    image_dir: &Path,
    tag_display: &str,
    options: &CreateOptions,
) -> String {
    let data_size = options.data_size.as_deref().unwrap_or("10G");
    let sdcard_size = options.sdcard_size.as_deref().unwrap_or("512 MB");
    // Keys are emitted in sorted order, matching what avdmanager writes, so the
    // two files can be diffed directly.
    let mut fields: BTreeMap<&str, String> = BTreeMap::new();
    // PlayStore images require a signed configuration osdk does not synthesise,
    // so the flag stays off unless the image itself is a playstore variant.
    fields.insert(
        "PlayStore.enabled",
        if image.tag == "google_apis_playstore" {
            "yes".into()
        } else {
            "no".into()
        },
    );
    fields.insert("abi.type", image.abi.clone());
    fields.insert("avd.id", name.to_string());
    fields.insert("avd.ini.encoding", "UTF-8".into());
    fields.insert("avd.name", name.to_string());
    fields.insert("disk.cachePartition", "yes".into());
    fields.insert("disk.cachePartition.size", "66MB".into());
    fields.insert("disk.dataPartition.size", data_size.to_string());
    fields.insert("fastboot.forceColdBoot", "no".into());
    fields.insert("fastboot.forceFastBoot", "yes".into());
    fields.insert("hw.accelerometer", "yes".into());
    fields.insert("hw.audioInput", "yes".into());
    fields.insert("hw.audioOutput", "yes".into());
    fields.insert("hw.battery", "yes".into());
    fields.insert("hw.camera.back", "emulated".into());
    fields.insert("hw.camera.front", "none".into());
    fields.insert("hw.cpu.arch", image.cpu_arch().to_string());
    fields.insert("hw.cpu.ncore", "4".into());
    fields.insert("hw.dPad", "yes".into());
    fields.insert("hw.gps", "yes".into());
    fields.insert("hw.gpu.enabled", "no".into());
    fields.insert("hw.gpu.mode", "auto".into());
    fields.insert("hw.gsmModem", "yes".into());
    fields.insert("hw.initialOrientation", "portrait".into());
    fields.insert("hw.keyboard", "yes".into());
    fields.insert("hw.lcd.density", "420".into());
    fields.insert("hw.lcd.depth", "32".into());
    fields.insert("hw.lcd.height", "2400".into());
    fields.insert("hw.lcd.width", "1080".into());
    fields.insert("hw.mainKeys", "no".into());
    fields.insert("hw.ramSize", "2048".into());
    fields.insert("hw.screen", "multi-touch".into());
    fields.insert("hw.sdCard", "yes".into());
    fields.insert("hw.sensors.orientation", "yes".into());
    fields.insert("hw.sensors.proximity", "yes".into());
    fields.insert("hw.trackBall", "no".into());
    fields.insert("hw.useext4", "yes".into());
    // The whole reason this module exists: an absolute path, so the emulator
    // cannot resolve it against a root it derived differently.
    fields.insert(
        "image.sysdir.1",
        format!("{}{}", image_dir.display(), std::path::MAIN_SEPARATOR),
    );
    fields.insert("kernel.newDeviceNaming", "autodetect".into());
    fields.insert("kernel.supportsYaffs2", "autodetect".into());
    fields.insert("runtime.network.latency", "none".into());
    fields.insert("runtime.network.speed", "full".into());
    fields.insert("sdcard.size", sdcard_size.to_string());
    fields.insert("showDeviceFrame", "no".into());
    fields.insert("tag.display", tag_display.to_string());
    fields.insert("tag.id", image.tag.clone());
    fields.insert("tag.ids", image.tag.clone());
    fields.insert("target", image.platform.clone());
    fields.insert("vm.heapSize", "256".into());

    let mut out = String::new();
    for (key, value) in fields {
        out.push_str(key);
        out.push('=');
        out.push_str(&value);
        out.push('\n');
    }
    out
}

/// Render the `<name>.ini` pointer file that sits beside the device directory.
///
/// The emulator finds a device through this file, not by scanning: without it
/// `-avd <name>` reports the device as unknown.
pub fn render_pointer(path: &Path, target: &str) -> String {
    format!(
        "avd.ini.encoding=UTF-8\npath={}\ntarget={}\n",
        path.display(),
        target
    )
}

/// Whether a path is safe to write into `config.ini`.
///
/// The emulator performs `%VAR%` environment expansion on `image.sysdir.1`.
/// Measured on 37.1.11: given osdk's percent-encoded versioned directory name
/// (`~v1~%61%6E%64...`) it logged `Environment variable 61 is not set` for every
/// hex pair, expanded each to nothing, and opened the mangled remainder --
/// ending in `Broken AVD system path`. So the value must not merely be absolute,
/// it must contain no `%`. The bridged path under the SDK root satisfies both.
pub fn is_emulator_safe_path(path: &Path) -> bool {
    !path.display().to_string().contains('%')
}

/// Create a device from an installed system image.
///
/// `image_dir` is the directory holding `system.img`; the caller resolves it from
/// the install layout so this function stays independent of how packages are laid
/// out. Returns the created `.avd` directory.
pub fn create(
    dirs: &crate::dirs::Dirs,
    name: &str,
    image: &ImageId,
    image_dir: &Path,
    tag_display: &str,
    options: &CreateOptions,
) -> Result<PathBuf> {
    validate_name(name)?;
    if !image_dir.join("system.img").is_file() {
        return Err(Error::other(format!(
            "no system.img under {}; install the image first with \
             `osdk install android-system-images@{}`",
            image_dir.display(),
            image.as_version()
        )));
    }
    let home = avd_home(dirs);
    let device = home.join(format!("{name}.avd"));
    let pointer = home.join(format!("{name}.ini"));
    if device.exists() && !options.force {
        return Err(Error::other(format!(
            "an AVD named `{name}` already exists at {}; pass --force to replace it",
            device.display()
        )));
    }
    if device.exists() {
        std::fs::remove_dir_all(&device).map_err(|error| Error::io(&device, error))?;
    }
    crate::dirs::create_dir_all(&device)?;

    // Absolute, but deliberately *not* canonicalised: on Windows that resolves
    // directory links, which would turn the bridged path straight back into the
    // percent-encoded install directory the emulator cannot read.
    let absolute = if image_dir.is_absolute() {
        strip_verbatim(image_dir)
    } else {
        let base = std::env::current_dir().map_err(|error| Error::io(image_dir, error))?;
        strip_verbatim(&base.join(image_dir))
    };
    // The emulator expands `%VAR%` in this value, so a percent-encoded path
    // would be mangled beyond recognition. The caller is expected to pass the
    // bridged path for exactly this reason; refuse rather than write a config
    // that fails much later inside the emulator.
    if !is_emulator_safe_path(&absolute) {
        return Err(Error::other(format!(
            "{} contains `%`, which the emulator expands as an environment \
             variable reference and would mangle; pass the path bridged into the \
             SDK root instead",
            absolute.display()
        )));
    }
    let config = render_config(name, image, &absolute, tag_display, options);
    let config_path = device.join("config.ini");
    std::fs::write(&config_path, config.as_bytes())
        .map_err(|error| Error::io(&config_path, error))?;
    let pointer_body = render_pointer(&device, &image.platform);
    std::fs::write(&pointer, pointer_body.as_bytes())
        .map_err(|error| Error::io(&pointer, error))?;
    Ok(device)
}

/// Remove a device and its pointer file.
pub fn delete(dirs: &crate::dirs::Dirs, name: &str) -> Result<bool> {
    validate_name(name)?;
    let home = avd_home(dirs);
    let device = home.join(format!("{name}.avd"));
    let pointer = home.join(format!("{name}.ini"));
    let existed = device.exists() || pointer.exists();
    if device.exists() {
        std::fs::remove_dir_all(&device).map_err(|error| Error::io(&device, error))?;
    }
    if pointer.exists() {
        std::fs::remove_file(&pointer).map_err(|error| Error::io(&pointer, error))?;
    }
    Ok(existed)
}

/// Drop the `\\?\` prefix `canonicalize` adds on Windows.
///
/// The emulator parses `config.ini` itself and does not accept the verbatim form.
fn strip_verbatim(path: &Path) -> PathBuf {
    let text = path.display().to_string();
    match text.strip_prefix(r"\\?\") {
        Some(rest) => PathBuf::from(rest),
        None => path.to_path_buf(),
    }
}

/// Reject names that would escape the AVD home or confuse the emulator.
fn validate_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && !name.contains(['/', '\\', ':', ';'])
        && name != "."
        && name != ".."
        && !name.chars().any(char::is_whitespace);
    if !ok {
        return Err(Error::other(format!(
            "`{name}` is not a usable AVD name; use letters, digits, `.`, `-` and `_`"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_ids_parse_with_or_without_the_manifest_prefix() {
        let bare = ImageId::parse("android-35;google_apis;x86_64").unwrap();
        assert_eq!(bare.platform, "android-35");
        assert_eq!(bare.tag, "google_apis");
        assert_eq!(bare.abi, "x86_64");
        // The same string as it appears in a manifest path.
        let prefixed = ImageId::parse("system-images;android-35;google_apis;x86_64").unwrap();
        assert_eq!(prefixed, bare);
        assert_eq!(bare.as_version(), "android-35;google_apis;x86_64");
        assert_eq!(bare.api_level(), Some("35"));
    }

    #[test]
    fn malformed_image_ids_are_rejected_rather_than_guessed() {
        // A partial id would otherwise produce an AVD pointing at nothing.
        assert!(ImageId::parse("android-35;google_apis").is_err());
        assert!(ImageId::parse("android-35;;x86_64").is_err());
        assert!(ImageId::parse("").is_err());
        assert!(ImageId::parse("android-35;google_apis;x86_64;extra").is_err());
    }

    #[test]
    fn cpu_arch_is_not_the_abi_string() {
        // The emulator refuses an image when hw.cpu.arch disagrees, and the two
        // spellings differ for every architecture except x86_64.
        let arch = |abi: &str| {
            ImageId {
                platform: "android-35".into(),
                tag: "google_apis".into(),
                abi: abi.into(),
            }
            .cpu_arch()
            .to_string()
        };
        assert_eq!(arch("x86_64"), "x86_64");
        assert_eq!(arch("x86"), "x86");
        assert_eq!(arch("arm64-v8a"), "arm64");
        assert_eq!(arch("armeabi-v7a"), "arm");
    }

    #[test]
    fn the_system_image_path_is_written_absolute() {
        // This is the defect being fixed: avdmanager writes a relative path that
        // only resolves against the root it derived, which is the wrong directory
        // under a linked layout.
        let image = ImageId::parse("android-35;google_apis;x86_64").unwrap();
        let dir = PathBuf::from(if cfg!(windows) {
            r"E:\osdk\installs\android-sdk\system-images\android-35\google_apis\x86_64"
        } else {
            "/osdk/installs/android-sdk/system-images/android-35/google_apis/x86_64"
        });
        let config = render_config(
            "pixel",
            &image,
            &dir,
            "Google APIs",
            &CreateOptions::default(),
        );
        let value = ini_value(&config, "image.sysdir.1").unwrap();
        assert!(
            Path::new(&value).is_absolute(),
            "must be absolute, got {value}"
        );
        // Trailing separator, matching what the emulator is given by avdmanager.
        assert!(value.ends_with(std::path::MAIN_SEPARATOR));
        assert!(!value.starts_with("system-images"));
    }

    #[test]
    fn config_carries_the_fields_the_emulator_matches_on() {
        let image = ImageId::parse("android-34;google_apis_playstore;arm64-v8a").unwrap();
        let config = render_config(
            "play",
            &image,
            Path::new("/images/x"),
            "Google Play",
            &CreateOptions::default(),
        );
        assert_eq!(ini_value(&config, "abi.type").as_deref(), Some("arm64-v8a"));
        assert_eq!(ini_value(&config, "hw.cpu.arch").as_deref(), Some("arm64"));
        assert_eq!(ini_value(&config, "target").as_deref(), Some("android-34"));
        assert_eq!(
            ini_value(&config, "tag.id").as_deref(),
            Some("google_apis_playstore")
        );
        // A playstore image is the only case where the store is enabled.
        assert_eq!(
            ini_value(&config, "PlayStore.enabled").as_deref(),
            Some("yes")
        );
        let plain = ImageId::parse("android-34;google_apis;arm64-v8a").unwrap();
        let config = render_config(
            "plain",
            &plain,
            Path::new("/images/x"),
            "Google APIs",
            &CreateOptions::default(),
        );
        assert_eq!(
            ini_value(&config, "PlayStore.enabled").as_deref(),
            Some("no")
        );
    }

    #[test]
    fn sizes_are_overridable_but_have_defaults() {
        let image = ImageId::parse("android-35;google_apis;x86_64").unwrap();
        let default = render_config(
            "a",
            &image,
            Path::new("/x"),
            "Google APIs",
            &CreateOptions::default(),
        );
        assert_eq!(
            ini_value(&default, "disk.dataPartition.size").as_deref(),
            Some("10G")
        );
        let custom = render_config(
            "a",
            &image,
            Path::new("/x"),
            "Google APIs",
            &CreateOptions {
                force: false,
                data_size: Some("8G".into()),
                sdcard_size: Some("1G".into()),
            },
        );
        assert_eq!(
            ini_value(&custom, "disk.dataPartition.size").as_deref(),
            Some("8G")
        );
        assert_eq!(ini_value(&custom, "sdcard.size").as_deref(), Some("1G"));
    }

    #[test]
    fn keys_are_sorted_so_the_file_can_be_diffed_against_avdmanagers() {
        let image = ImageId::parse("android-35;google_apis;x86_64").unwrap();
        let config = render_config(
            "a",
            &image,
            Path::new("/x"),
            "Google APIs",
            &CreateOptions::default(),
        );
        let keys: Vec<&str> = config
            .lines()
            .filter_map(|line| line.split_once('=').map(|(key, _)| key))
            .collect();
        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(keys, sorted);
    }

    #[test]
    fn pointer_file_names_the_device_directory() {
        // Without this file the emulator reports the AVD as unknown, however
        // correct config.ini is.
        let body = render_pointer(Path::new("/avd/pixel.avd"), "android-35");
        assert!(body.contains("path=/avd/pixel.avd"));
        assert!(body.contains("target=android-35"));
        assert!(body.contains("avd.ini.encoding=UTF-8"));
    }

    #[test]
    fn names_that_could_escape_the_avd_home_are_rejected() {
        assert!(validate_name("pixel-35").is_ok());
        assert!(validate_name("a_b.c").is_ok());
        assert!(validate_name("").is_err());
        assert!(validate_name("..").is_err());
        assert!(validate_name("a/b").is_err());
        assert!(validate_name(r"a\b").is_err());
        // A `;` would collide with the image id syntax, and a space breaks the
        // emulator's own argument handling.
        assert!(validate_name("a;b").is_err());
        assert!(validate_name("two words").is_err());
    }

    #[test]
    fn a_percent_in_the_image_path_is_rejected_not_written() {
        // Measured on emulator 37.1.11: it expands `%VAR%` inside
        // `image.sysdir.1`. Given osdk's own percent-encoded versioned directory
        // (`~v1~%61%6E%64...`) it logged `Environment variable 61 is not set` for
        // every hex pair, expanded them away, and died with
        // `Broken AVD system path`. So "absolute" is not a strong enough
        // requirement -- the value must also contain no `%`.
        assert!(is_emulator_safe_path(Path::new(
            r"E:\osdk\installs\android-sdk\system-images\android-35\google_apis\x86_64"
        )));
        assert!(!is_emulator_safe_path(Path::new(
            r"E:\osdk\installs\android-system-images\~v1~%61%6E%64%72%6F%69%64"
        )));

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let dirs = crate::dirs::Dirs::resolve_from(|key| match key {
            "OSDK_DATA_DIR" => Some(root.join("data").display().to_string()),
            "OSDK_CACHE_DIR" => Some(root.join("cache").display().to_string()),
            "OSDK_CONFIG_DIR" => Some(root.join("config").display().to_string()),
            _ => None,
        })
        .unwrap();
        // A real directory whose name carries the encoding osdk uses.
        let encoded = temp.path().join("~v1~%61%6E%64");
        std::fs::create_dir_all(&encoded).unwrap();
        std::fs::write(encoded.join("system.img"), b"image").unwrap();
        let image = ImageId::parse("android-35;google_apis;x86_64").unwrap();
        let error = create(
            &dirs,
            "probe",
            &image,
            &encoded,
            "Google APIs",
            &CreateOptions::default(),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains('%'), "unexpected error: {error}");
        // Nothing may be left behind: a half-written AVD would fail later, in the
        // emulator, with a much less obvious message.
        assert!(!avd_home(&dirs).join("probe.ini").exists());
    }

    #[test]
    fn creating_refuses_an_image_directory_without_a_system_image() {
        // Otherwise the AVD is created and only fails much later, inside the
        // emulator, with a far less obvious message.
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let dirs = crate::dirs::Dirs::resolve_from(|key| match key {
            "OSDK_DATA_DIR" => Some(root.join("data").display().to_string()),
            "OSDK_CACHE_DIR" => Some(root.join("cache").display().to_string()),
            "OSDK_CONFIG_DIR" => Some(root.join("config").display().to_string()),
            _ => None,
        })
        .unwrap();
        let image = ImageId::parse("android-35;google_apis;x86_64").unwrap();
        let empty = temp.path().join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        let error = create(
            &dirs,
            "probe",
            &image,
            &empty,
            "Google APIs",
            &CreateOptions::default(),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("system.img"), "unexpected error: {error}");
    }
}
