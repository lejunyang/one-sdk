# Android SDK tools

This page covers managing Android SDK packages through osdk: the NDK,
platform-tools (`adb`, `fastboot`), build-tools, cmdline-tools, CMake, platforms
and more.

osdk parses Google's official repository manifest and downloads straight from
Google's servers. It does not proxy or re-host archives, which puts it in the
same position as the SDK Manager bundled with Android Studio.

## Tool names

Each package family maps to one backend, named `android-<family>`:

| Tool | Executables provided | Notes |
| --- | --- | --- |
| `android-platform-tools` | `adb`, `fastboot` | Single package; version is the revision |
| `android-ndk` | clang/lld toolchain | Side-by-side layout |
| `android-build-tools` | `aapt2`, `d8`, `apksigner`, `zipalign` | — |
| `android-cmdline-tools` | `avdmanager`, `lint`, `retrace` | — |
| `android-cmake` | `cmake` | Used by NDK builds |
| `android-platforms` | `android.jar` | No executables |
| `android-emulator` | `emulator` | — |
| `android-sources` | — | Sources; no executables |
| `android-system-images` | — | Emulator disk images; no executables |

```bash
osdk list-remote android-platform-tools
osdk install android-platform-tools@37.0.1 -o accept-licenses=true
osdk exec -t android-platform-tools@37.0.1 -- adb version
```

`ndk-bundle` is deliberately excluded: it is the pre-side-by-side layout and
including it would only create ambiguity with `android-ndk`.

The plural `emulators` family is excluded as well, and it is not a spelling
variant of `android-emulator`. Measured against the live manifest, the two differ
in five ways: `emulators` is versioned side-by-side as `emulators;<build-id>`,
declares `emulator` itself as a dependency, ships a 273 MB Windows archive rather
than 421 MB, names its files `emulator_windows_x64-*` instead of
`emulator-windows_x64-*`, and exists only on the preview channel. It is an
incremental component layered on the singular package, so the version numbers do
not even compare: `emulators;latest` was 37.1.2 while stable `emulator` was
37.1.11.


## License agreements

Almost every Android package requires accepting a license agreement first. osdk
never accepts on your behalf; you must pass it explicitly:

| Option | Effect |
| --- | --- |
| `accept-licenses=true` | Accept every agreement involved in this request |
| `accept-license=<id>` | Accept only the named agreement; comma-separate several |

```bash
# Accept everything this request needs
osdk install android-ndk@29.0.14206865 -o accept-licenses=true

# Accept one specific agreement
osdk install android-ndk@29.0.14206865 -o accept-license=android-sdk-license
```

Accepting by id does **not** leak into other agreements: accepting
`android-sdk-license` is not accepting `android-sdk-preview-license`, so preview
packages still stop.

When nothing has been accepted, the install fails before any bytes are
downloaded, and the error itself names the command that would unblock it.

### Reviewing and exporting

```text
osdk android licenses show TOOL[@VERSION] [--digest-only]
osdk android licenses status
osdk android licenses export --sdk-root DIR
```

```bash
# Print the full agreement text and whether it is already accepted
osdk android licenses show android-ndk@29.0.14206865

# Just the id and digest
osdk android licenses show android-ndk@29.0.14206865 --digest-only

# Which agreements are recorded
osdk android licenses status

# Hand the records to Gradle
osdk android licenses export --sdk-root /path/to/sdk
```

Acceptance is recorded at `<installs>/android-sdk/licenses/<license id>`, whose
contents are the SHA-1 of the full agreement text — the same format the official
tooling and Gradle/AGP read. Exporting that directory lets Gradle reuse your
acceptance instead of prompting again.

The digest is always computed live from the **current** manifest. The fixed
hashes circulating in community guides and CI recipes are snapshots of older
agreement text and stop matching once Google revises the wording, so osdk does
not hard-code them. A consequence worth knowing: when the agreement text
changes, an older acceptance record no longer counts and consent is requested
again — which is what the official tooling does too.

### Lock files and teams

License acceptance is **not** written to `osdk.lock`. A lock file is committed
and replayed on other machines, so recording acceptance there would accept
Google's terms on a teammate's behalf. The lock therefore carries only the
artifact and its checksum, and each machine consents for itself:

```bash
# A teammate installing for the first time in a repo with a committed lock
osdk install                            # stops, and says consent is needed
osdk install -o accept-licenses=true    # consents, then installs the locked artifact
osdk install                            # fine from now on; consent is recorded locally
```

Passing only accept options still replays the lock, because consent does not
select an artifact. Mixing in something that does, such as `channel`, restores
the usual behaviour of bypassing the lock.

## Channels

The manifest sorts packages into stable, beta, dev and canary channels. osdk
installs from stable only unless you opt in:

```bash
osdk install android-ndk@30.0.16138531 -o channel=beta
```

Channel and license are independent. Some packages sit on the stable channel yet
still carry the preview agreement, and those still require accepting
`android-sdk-preview-license`.

## Checksum strength

The Android manifest publishes only **SHA-1** per archive, with no stronger
digest available. That is weaker than every other osdk download source and is
treated as an explicit, source-scoped exception:

- osdk always verifies the SHA-1 the manifest declares; it never installs
  unverified.
- No other backend's requirements are relaxed; they still demand SHA-256 or
  better.
- The weakness is marked in the code so this digest is never mistaken for a
  collision-resistant one.

## Download sources

Google's official repository is the default, with one working public mirror
alongside it:

| Source | URL |
| --- | --- |
| `google` (official) | `https://dl.google.com/android/repository/` |
| `tencent` (mirror) | `https://mirrors.cloud.tencent.com/AndroidSDK/` |

The mirror serves a byte-identical manifest (matching SHA-256), so the SHA-1
values embedded in it remain valid there. Measured throughput from Google
directly was actually higher, so the mirror ranks below it; the effective order
is still decided dynamically by [source selection](./sources-security).

## Shared command names

Two families ship the same R8 launchers: `build-tools` and `cmdline-tools` both
provide `d8`, `r8`, `retrace` and `resourceshrinker`. With both installed, those
names resolve to the `build-tools` copy, which is the one a build invokes.
Everything unique to `cmdline-tools` -- `sdkmanager`, `avdmanager`, `lint` and
the rest -- is still generated as usual.

Any other duplicate name is still reported as a conflict for you to resolve.

## Choosing which shims to generate

Every executable a tool exposes gets a shim by default. Some SDKs are
genuinely large -- one NDK ships 172 executables, a clang wrapper per API
level -- but that is its real shape, and hiding them by default would break the
ordinary way of selecting a compiler, so narrowing is opt-in.

Exclude or restrict them in the config:

```toml
[settings.shims]
exclude = ["apkanalyzer", "*-clang"]
```

A non-empty `include` shims only matching names, and `exclude` is applied last,
so a broad include can be trimmed:

```toml
[settings.shims]
include = ["*"]
exclude = ["d8"]
```

Patterns accept `*` and `?` and ignore case. Qualifying a pattern with a
backend narrows just that tool without listing each executable:

```toml
[settings.shims]
exclude = ["android-ndk:*"]
```

Excluding a name only withholds the shim. The tool stays installed, remains on
PATH under an activated shell, and `osdk exec` still reaches it. Run
`osdk reshim` to apply a change, and `osdk config list` to see the current
values.

A command name shared by two families (above) goes to whichever family survives
the filter: exclude `android-build-tools` and `d8` comes from cmdline-tools
instead.

## Java runtime

Google's Android packages ship **no JDK**. `sdkmanager`, `avdmanager`, `d8`,
`lint` and friends are launchers around bundled jars -- 125 of them in
`cmdline-tools` alone -- with no `java` binary, so they exit immediately when no
JDK is visible.

osdk fills that in: when one of these tools runs and the environment has no
`JAVA_HOME`, it uses an osdk-managed JDK, preferring the version selected for
the current directory and otherwise the newest installed one.

```powershell
osdk install java
osdk install android-cmdline-tools -o accept-licenses=true
sdkmanager --version   # no activation, no manual JAVA_HOME
```

An existing `JAVA_HOME` is never replaced, whether it came from shell
activation or from you, so a system JDK still wins. With no managed JDK
installed the tool reports its own missing-JDK error; install `java` to fix it.

The same applies to `maven`, `gradle` and `kotlin`.

## Environment variables

| Tool | Exported |
| --- | --- |
| `android-ndk` | `ANDROID_NDK_ROOT`, `ANDROID_NDK_HOME` |
| Other Android packages | `ANDROID_SDK_ROOT` |

`ANDROID_SDK_ROOT` points at the shared SDK root that holds the license records,
not at an individual package directory, so tools like Gradle can reuse your
acceptance.

## System images and dependency resolution

Emulator system images are published outside the main manifest, in per-vendor
sub-sites. osdk merges them into one family so a single command lists everything:

```bash
# 263 versions across five sub-sites: default, google_apis,
# google_apis_playstore, android-tv and android-wear
osdk list-remote android-system-images

osdk install "android-system-images@android-35;google_apis;x86_64" \
  -o accept-licenses=true
```

The sub-site manifests are only fetched for this family. Adding six requests to
every `osdk install adb` would be a poor trade for a list nothing else reads.

Archive URLs inside a sub-site manifest are relative to that manifest's own
directory, and two sub-sites can publish the same file name — an
`x86_64-35_r09.zip` exists under more than one vendor. osdk rewrites every URL to
be root-relative while parsing, so a package can never be fetched from the wrong
vendor's directory.

### Dependencies

Unlike the main manifest, where only three preview packages declare a dependency,
dependencies are the norm here: every one of the 56 declarations in the
`google_apis` manifest names `emulator`, most with a minimum revision. osdk
resolves that closure and installs what is missing, so asking for an image gives
you a working emulator.

Two rules exist because of what the real data does:

- **The license gate covers dependencies.** Sub-sites reference eight agreements
  the main manifest never mentions, including `intel-android-sysimage-license`
  and `android-googletv-license`. Consenting to the image's own agreement cannot
  imply consent to a vendor agreement you were never shown.
- **A dependency resolves on the stable channel.** `emulator` is published twice,
  and the preview build carries the higher revision. Following "newest" would
  install a preview to satisfy a package the user never named, and the install
  would then fail its own channel check.

An edge pointing at a family osdk does not curate is skipped with a warning
rather than failing the install: a manifest edge is not a reason to refuse a
package that is otherwise installable.

### Directory layout

Google's tools require one shared SDK directory, while osdk installs each package
into its own versioned directory. Both hold at once: the payload stays where osdk
put it, and a directory link publishes it at the path the Android tools expect —
`system-images/android-35/google_apis/x86_64`, `platform-tools`, `emulator`, and
so on. Nothing is copied, so a 3.5 GB image is stored once.

On Windows the link is an NTFS junction rather than a symlink, because a symlink
needs Developer Mode or elevation while a junction needs neither, and the Android
tools only ever traverse it.

If a real directory already occupies the target path — most often a package that
Google's own `sdkmanager` installed — osdk leaves it alone and warns. Taking that
path over would mean deleting data osdk never owned.

### The emulator's SDK root check

The emulator decides whether a directory is a usable SDK root by looking for a
`platform-tools` child, and nothing else. It checks `ANDROID_HOME`, then
`ANDROID_SDK_ROOT`, then walks up from its own location, rejecting every
candidate that lacks it and ending in `FATAL | Broken AVD system path`.

A root holding only `emulator` and `system-images` is therefore still invalid.
Install platform-tools before creating an AVD:

```bash
osdk install android-platform-tools -o accept-licenses=true
```

osdk warns at install time when the shared root is not yet valid, because the
emulator's own error names the root rather than the missing piece.

### Mirrors

Images are large, so a mirror looks appealing. Measured throughput says
otherwise: the Tencent mirror served byte-identical archives at 4.38 MB/s against
Google's 5.54 MB/s, an 0.79x speed-up. The existing source order already prefers
the faster path, so no image-specific handling was added.

One caveat worth knowing: the mirror does not carry beta images. A missing
`x86_64-ps16k-37.2_r04.zip` reflects that gap, not a broken mirror — its copies of
both the main and sub-site manifests hash identically to Google's.
