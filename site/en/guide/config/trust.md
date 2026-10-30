# Project Configuration Trust

```text
osdk [--yes] trust [PATH]
osdk trust list
osdk untrust [PATH]
```

`PATH` may name a config file or directory; a directory triggers upward project
discovery.

## Which keys require trust

Judgement is still per key, but the gate opens according to the **command's
scopes**: a command is blocked only by keys it can actually reach and make
effective. Trust means "an unreviewed config must not do anything", so a
command that does nothing receives no gate.

The four scopes and the keys they check:

| Scope | Commands that reach it | Keys checked |
| --- | --- | --- |
| Install | `install`, `use`, `upgrade`, `lock`, `self upgrade`; also `run` when a selected task must acquire a missing `tools` requirement | verification switches and catalogs in `settings`, `sources`, `registries`, `tools.allow_builds` |
| Deps | bare `install`, `run`/`exec` by default, explicit `deps` | `sources`, `registries`, registries/build/`run` inside `[deps]` |
| Run tasks | `run` (not `--dry-run`) | `[task]`: its `shell` picks the interpreter for every task |
| System packages | `pkg apply` | `[sys.pkg]` entries that actually **apply on this machine** |
| Container | any `container` operation | runtime/builder/registries in `[container]` |

Governed keys and their reasons:

| Key | Why |
| --- | --- |
| `[sources]`, `[registries]` | Change where subprocesses download from |
| `settings.verify_signatures`, `settings.require_checksums`, `settings.attestations` | Disable or downgrade artifact verification |
| `settings.python`, `settings.java` | Both carry `catalog_url`, which decides which runtime bytes get installed |
| `allow_builds`, explicitly enabled in `[tools]` | Lets npm lifecycle scripts run, or lets pypi build source distributions (running `setup.py` or a build backend) |
| `cargo:` entries in `[tools]`, including aliases that name one | Compiling a crate always runs its `build.rs` and procedural macros, with no switch to turn that off; a version change is new code and needs re-approval |
| registries/`run`/build switches under `[deps.<p>]` | Redirect dependency bytes or run an arbitrary command |
| `[sys.pkg]` (`pkg apply` only) | Installs machine-wide, may prompt for elevation, and is not covered by `osdk.lock` |

Two narrowings worth stating:

- **`[sys.pkg]` gates only `pkg apply`, and only when an entry applies here.** A
  config whose packages all carry `when = { os = "linux" }` blocks no command on a Mac, and
  an entry whose manager cannot exist on this OS (such as an `apt:` entry on
  Windows) is ignored too -- a package that can never execute here needs no
  review. `pkg status`/`plan`/`doctor` are read-only anyway and are not gated.
- **`[models]` requires trust for no command**, including entries carrying an
  `endpoint`. Model bytes are content and osdk never executes them; downloads
  still verify against pinned digests, so a redirected endpoint cannot turn a
  content fetch into code execution.

**Declaring which tools or packages to install never requires trust on its own.**
That covers `[tools]` and `[alias.tools]`, including their `npm:`, `github:`, `http:`,
`go:`, `pypi:` and `conda:` entries: npm installs pass
`--ignore-scripts` by default, `pypi:` installs use wheels only, an `http:` artifact without a `sha256` is refused
outright, and `go:` builds run with `CGO_ENABLED=0`. The one exception is
`cargo:`: `cargo install` can only build from source, and building runs every
`build.rs` and procedural macro in the dependency graph, so the declaration is
itself the grant (see the table above). This is the same act as
adding a line to `package.json` -- adding a package or changing a version never
asks for re-approval.

`settings.node` (a single `corepack` bool) and the npm choice in `settings`
(a lowest-priority fallback between npm and pnpm) do not require trust either.

Two things fail closed: an **unknown top-level section** for acting commands and
an **unregistered `settings` key** both require trust. A key this build cannot
interpret is not cleared just because it is unrecognized.

## What the gate covers: command scopes decide

- **Read-only inspection**: `list`, `current`, `where`, `doctor`, `completions`,
  `config get`/`config list`, and `task list` / `task info` / `task deps`. They
  only report or display state and reach no install, download or execution, so
  they keep working even when the config carries governed keys. They are also
  exactly what you run *while deciding* whether to trust a project -- refusing
  them hides both the evidence and the way out.
- **Trust management itself**: `trust`, `untrust`.
- **`config set` / `config unset` / `config migrate`**: the way an untrusted config is edited back
  into shape; gating them would block the only exit with the very config being
  undone.
- Every other command is gated according to the scopes above. A command may
  carry several: bare `install` reaches both Install and Deps, as do `run` and
  `exec` by default; `--no-deps` removes the Deps scope, and `--dry-run` leaves
  run with no scope at all.

**Tools dispatched through the shim are a separate line.** `cargo`, `node` and
the rest are started by the shim, which gates only the keys it can act on
itself -- `sources`, `registries` and the like, which decide where a subprocess
it starts will fetch from. A table the shim never reads, such as `[sys.pkg]` or
`[task]`, does not stop you from using tools in that directory.

## Trust identity and record states

```bash
osdk --yes trust                 # nearest project configuration
osdk --yes trust ./osdk.toml     # explicit file
osdk trust list                  # records and their state
osdk untrust                     # revoke the nearest project configuration
osdk trust prune --dry-run       # preview which dead records would go
osdk trust prune                 # drop records whose config file is gone
```

`trust list` reports four states, and the right response differs for each:

| State | Meaning | What to do |
| --- | --- | --- |
| `active` | File present, governed keys match the approval | nothing |
| `changed` | File present, governed keys differ | review, then `osdk trust` again |
| `missing` | File gone, its directory still readable | `osdk trust prune` can drop it |
| `unreachable` | Its directory is unreadable too | check whether the volume is mounted; **never** pruned |

`prune` removes only `missing`, deliberately. `changed` means the project is still
there and merely needs another look, so dropping its record would resurface later
as an unexplained "untrusted". And on Windows `unreachable` is exactly what a
detached USB disk, network share or WSL mount looks like, so treating it as garbage
would revoke valid approvals whenever a volume happened to be unplugged.
`--dry-run` uses the same candidate list as the real run, so the preview is what
will actually happen.

Persistent identity binds the canonical path and the BLAKE3 of the normalized
TOML content **of the governed keys listed above**. Because the hash covers only
those keys, both gates read the same judgement: a change that needed no trust
also cannot invalidate an existing record. Bumping a tool version, adding a
dependency, changing `jobs`, adding a comment and reordering keys all leave the
record intact; editing a governed key makes it `changed`, and moving the
repository makes it `missing` or `unreachable`. Symlinks resolve to their real
target. Records live in `$OSDK_CONFIG_DIR/trusted-configs.toml`.

For external project configuration (see
[External Project Configuration](../project/external-config)), the trusted object is
still the real `osdk.toml` inside the external directory: approval binds that file's
real path, project root, and governed content. After `osdk project relocate` moves
a project root, the physical config path moves with it and the trust record must be
re-approved with `osdk trust`; it never follows automatically.

CI may set `OSDK_TRUSTED_CONFIG_PATHS` to an OS path-list of reviewed files or
directories. Matching project files are trusted for that process without being
written to the local store. `trust` and `untrust` themselves load only user
configuration, so an untrusted project cannot influence its own approval.

`osdk config set` and `osdk config unset` also run before the trust check.
Otherwise the exit would be blocked by the very config being undone -- `unset`
could not remove the key causing the refusal. Both address one named key in one
named file and never act on what the untrusted config asks for. `config get` and
`config list` are not gated either: they only display the config's merged
values and do nothing.
