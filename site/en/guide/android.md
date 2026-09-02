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

```bash
osdk list-remote android-platform-tools
osdk install android-platform-tools@37.0.1 -o accept-licenses=true
osdk exec -t android-platform-tools@37.0.1 -- adb version
```

`ndk-bundle` is deliberately excluded: it is the pre-side-by-side layout and
including it would only create ambiguity with `android-ndk`. System images live
in separate sub-site manifests and are not covered yet either.

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
