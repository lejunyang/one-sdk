# Application Dependencies

`osdk install` installs **tools**: it puts Node, pnpm, or Python into osdk's own
isolated directories. `osdk deps` installs **a project's own dependency
manifest**: it reads `package.json`, drives the project's own package manager,
and lands the whole dependency closure inside the project.

The two do not overlap:

| What you want | Use |
| --- | --- |
| Provide pnpm itself | `osdk install pnpm`, or `[tools]` |
| Turn a whole `package.json` into `node_modules` | `osdk deps` |
| Add or remove one dependency | `osdk install npm:<package>` |

## Enabling it

`deps` does not act just because a `package.json` is nearby. That would be an
implicit, large side effect -- the same judgement that keeps `osdk install` from
fetching models on its own. Declare a provider explicitly:

```toml
# osdk.toml
[deps.pnpm]
```

Then:

```bash
osdk deps --list            # detected providers and their freshness
osdk deps --dry-run         # print what would run, without running it
osdk deps                   # materialize the manifest
osdk deps --explain         # also explain each freshness decision
```

With no `[deps]` section, `osdk deps` only tells you what it found:

```
no `[deps]` section; found manifests osdk could manage:
  /path/to/project/package.json
    candidates: bun, npm, pnpm, yarn

enable one in osdk.toml, for example:
  [deps.bun]
```

## Supported providers

The four Node installers: `npm`, `pnpm`, `yarn`, and `bun`.

## How the installer is chosen

Highest priority first; nothing lower can take a project away from something
higher:

1. `packageManager` in `package.json` -- the project's own declaration wins.
2. A native lockfile already on disk -- whoever wrote it keeps managing it.
3. `[deps.<provider>].installer`.
4. The provider itself.

Two contradictions are refused rather than guessed: a declaration of `pnpm`
next to nothing but a `yarn.lock`, and two lockfiles of the same ecosystem in one
directory. Both name the specific files so you can decide which to keep.

A manifest that will not parse is also an error rather than a skip. Walking
silently past a broken `package.json` would install the wrong project's
dependencies, or none, and report success.

## Frozen installs

osdk looks for the native lockfile itself instead of passing a flag and assuming
it took effect:

- Lockfile present → a frozen install (`npm ci`, `pnpm --frozen-lockfile`,
  yarn berry `--immutable`, yarn classic and bun `--frozen-lockfile`).
- No lockfile → a plain install, plus a `warning:` line saying a lockfile will be
  created.

**Why this cannot be delegated**: `yarn@1.22.19` and `bun` exit 0 and install
normally with no lockfile at all, and `yarn@1` silently accepts berry's
`--immutable` while neither freezing nor blocking scripts. Passing the flag
without checking would be silently wrong on two of the four.

To require a lockfile in CI:

```bash
osdk deps --frozen
```

That turns the fallback above into an error instead of a warning it is easy to
miss.

## When the package manager is not installed

`deps` installs it, through the same tool install path `osdk install` uses -- so
source selection, verification and the CAS are identical, and there is no second
installer to keep honest.

The tool lands in osdk's isolated directories, **not** in your project. `deps`
puts dependencies in the project; the package manager that installs them is a
tool.

```bash
$ osdk deps
installing node for deps provider `npm`
installing node@26.10.0 ...installed node@26.10.0
npm.cmd install --ignore-scripts
added 2 packages in 827ms
```

In CI you usually want tools to come from an explicit `osdk install`, so that a
run cannot quietly acquire a different version. `--no-install-tools` turns the
acquisition off -- it forbids *acquiring*, not using one you installed yourself:

```bash
osdk deps --no-install-tools
```

It fails when a tool is missing, naming the command that would fix it.

## Build scripts are off by default

A dependency's `preinstall` / `install` / `postinstall` hooks do not run.
Declaring which packages to install cannot execute anything the publisher did not
ship as plain files, so the declaration itself needs no approval.

Turning them on does need approval, because that is the point at which arbitrary
code runs on your machine:

```toml
[deps.pnpm]
allow_build_from_source = true
```

## Changing the registry

```toml
[deps.pnpm]
index = "https://registry.example.com/"
```

Redirecting where bytes come from requires approving the config -- because the
source changed, not because anything executes. Achieving the same thing through
`env` (for instance `NPM_CONFIG_REGISTRY`) requires approval too: a gate one
spelling walks around is not a gate.

## Full configuration

```toml
[deps]
disable = ["npm"]           # off here even if a broader layer enabled it

[deps.pnpm]
auto = true                 # allow materializing ahead of run/exec
sources = ["package.json"]  # files that decide freshness (replaces the default)
outputs = ["node_modules"]  # missing means stale (replaces the default)
dir = "apps/api"            # run in a subdirectory
depends = ["npm"]           # materialize another provider first
timeout = "10m"
installer = "pnpm"
index = "https://registry.npmjs.org/"
allow_build_from_source = false
env = { CI = "1" }
```

## Freshness

`deps` records a hash of the inputs of the last successful run and compares it
next time. The hash covers the contents of `sources` **and the command that
actually ran**, environment included -- yarn berry disables build scripts through
`YARN_ENABLE_SCRIPTS`, so leaving env out would hash two materially different
runs the same.

State lives in osdk's cache directory, never in your project. `--force` runs
regardless of the decision.

Undeclared `sources`, or declared sources that match no files at all, never count
as fresh: a predicate that matches nothing is vacuously true, which would dress
up "nothing was checked" as "nothing changed".
