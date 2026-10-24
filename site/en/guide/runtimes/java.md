# Java and JVM Tools

Configuration reference: [applicable tool_order sources](../config/tool-selection#java), [project source diagnostics](../config/tool-selection#inspect-sources), and [this tool’s selectors, managed variables and cache overrides](../config/tool-environment#java).

This page covers the Java JDK and JRE, plus JVM tools such as Maven, Gradle, and Kotlin.

## Java JDK and JRE

```bash
osdk install java@21
osdk install java@zulu-17.0.1
osdk install java@21 -o distribution=zulu -o package-type=jdk
osdk install java@21 -o package-type=jre
```

| Option | Values | Default |
| --- | --- | --- |
| `distribution` | A Foojay distribution ID such as `temurin` or `zulu` | `temurin` |
| `package-type` | `jdk` or `jre` | `jdk` |

The distribution can also be inline, as in `java@temurin-21`. Foojay lookup
filters distribution, OS, architecture, archive type, JDK/JRE, and Linux libc.
A JRE uses identity `jre-<resolved-version>` and can coexist with the matching
JDK. Execution exports `JAVA_HOME`.

Temurin versions carry a build number (such as `21.0.12+8`), and a PSU adds a
fourth segment (such as `21.0.12.1+1`). You may omit the build number in the
request: `java@21.0.12` matches the same-core `21.0.12+8`, and only falls back
to a four-part PSU when no same-core release exists.

The embedded Temurin LTS catalog covers 8, 11, 17, 21, and 25, allowing
resolution with an empty metadata cache. A locked artifact remains installable
when Foojay is unavailable. Configure a Foojay-compatible `/packages` endpoint
or static mirror with:

```toml
[settings.java]
catalog_url = "https://mirror.example/disco/v3.0/packages"
```

Unless that single explicit catalog is set, Java queries each ranked
Foojay-compatible source. A reachable endpoint that lacks the requested
distribution/JDK/JRE combination falls through, and checksum detail lookup uses
the same order before the vendor archive enters the shared verified pipeline.

## JVM tools

```bash
osdk install maven@3.9.16
osdk install "gradle@=9.3.1"
osdk install kotlin@2.4.10
```

**Gradle resolves from the upstream version index**, so any historical release is
installable: `osdk list-remote gradle` lists every finished release (179 measured
on 2026-09-14), and each version's download URL and SHA-256 come from the index
rather than being hardcoded per version. Nightlies, `-rc-N` and `-milestone-N`
entries are classified as unstable, so `latest` only ever lands on a finished
release; ask for a preview by name. An entry that publishes no checksum is
refused rather than installed unverified.

Maven and Kotlin have no comparable machine-readable index upstream, so they stay
a fixed catalog: Maven `3.9.16` with SHA-512 and Kotlin `2.4.10` with SHA-256;
other versions fail. All three have their own installation directory and shims;
Kotlin also has a GitHub proxy download candidate. Maven and Kotlin use the
ranked effective source URLs. Gradle tries each source index and rebases the
index's absolute distribution URL across all ranked download roots, so a source
with a readable index but a missing target zip can still fall through.

::: tip Relationship to the Gradle wrapper
When a project has `gradle/wrapper/gradle-wrapper.properties`, **the wrapper
remains authoritative** -- its `distributionSha256Sum` is a stronger guarantee
than a version pin. An osdk gradle pin is for the cases without a wrapper, such
as a new project or invoking `gradle` directly. The two agree on the digest: the
index's checksum for 9.3.1 is byte-identical to the `distributionSha256Sum` a
wrapper pins for it.
:::
