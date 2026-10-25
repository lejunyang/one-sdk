# External Project Configuration

By default, an osdk project keeps its configuration in `osdk.toml` inside the
project directory. External project configuration stores the project's tool
declarations and lockfile under osdk's own configuration directory instead,
binding a directory as a project without writing anything into it.

Typical uses:

- read-only checkouts of third-party repositories, or codebases you must not
  modify;
- keeping several projects' osdk configuration centralized in your own config
  directory;
- scratch copies and archived checkouts, where you do not want to leave an
  `osdk.toml` / `osdk.lock` behind.

## Command overview

```text
osdk project add [PATH]        # bind a directory as an external project; PATH defaults to cwd; --dry-run / --json
osdk project show [PATH]       # show the effective source and binding state; PATH defaults to cwd; --json
osdk project list              # list every external binding; --json emits an array
osdk project relocate OLD NEW  # move a binding; OLD and NEW are both required; --dry-run / --json
osdk project remove PATH        # drop an external binding directory; PATH is required; --dry-run / --json
```

```bash
osdk project add .                 # bind the current directory
osdk project add C:\repos\frontend # bind any directory, counted by its real canonical path
osdk project show .                # which configuration this directory resolves to
osdk project list                  # every binding at a glance
osdk project list --json          # array output with the full status fields
osdk project remove ~/tmp/old-checkout --dry-run
osdk project remove ~/tmp/old-checkout
```

## Bind behavior

- `add` binds the directory after OS-canonicalizing it to its real absolute path:
  relative paths, junctions, 8.3 short names, and case aliases all count by the
  resolved real path; it does not walk up to guess a Git root, installs nothing,
  and does not modify the directory.
- Binding an already-bound directory again does not overwrite or change the
  existing binding.
- A directory that already contains a local `osdk.toml` / `.osdk.toml` can
  still be bound; at runtime the local file wins, and osdk warns on stderr that
  the external configuration is shadowed.

## Where external configuration lives

Every binding occupies one subdirectory under the config directory. A successful
`add` (not `--dry-run`) atomically writes the bundle:

```text
<OSDK_CONFIG_DIR>/projects/<root-id>/
  project.toml    # anchor file; root records the project root's real absolute path
  osdk.toml       # external project tool/task declarations (placeholder comment to start; edit it)
  osdk.lock       # the external project's lockfile; its initial body is the current ordinary lock schema, from the same schema_version authority as the normal writer
```

- `root-id` is the full BLAKE3 of the real project-root path after OS
  canonicalization: junctions, 8.3 short names, and case aliases resolve to the
  same identity; casing follows filesystem semantics — osdk neither lowercases
  additionally nor applies Unicode normalization. It is **not derived from the
  project name or any Git remote**; changing a display name or remote does not
  disturb the binding. Moving or renaming its directory changes the path and
  requires `project relocate`.
- There is no separately hand-editable index: the `<config>/projects/`
  directory and its `project.toml` files are the whole registry.
- `osdk project show .` prints the physical directory behind the binding, so you
  know where to edit `osdk.toml`.

## Discovery and precedence

When you run a command in a directory, osdk walks upward from the current
directory:

1. The nearest project wins — "project" here means both a directory carrying a
   local `osdk.toml` and a bound external project;
2. Within the same project root, a local `osdk.toml` / `.osdk.toml` outranks the
   external configuration;
3. A nearer external sub-project can outrank a local parent further up.

## Configuration and lock are selected as one set

Each command resolves exactly **one** effective source: an external project's
`osdk.toml` and `osdk.lock` always travel together, and likewise for a local
project.

- If a lock goes missing (e.g. deleted by hand), it is regenerated from the
  currently selected source inside that same directory — never borrowed from
  another source.
- A corrupted selected configuration or lock fails explicitly, instead of
  silently falling back to another source.
- The body of an external configuration that a local file shadows is never
  read.
- When multiple candidates exist, osdk says so on stderr: which configuration
  and lock were adopted, and which were ignored.

All existing read/write entry points — `current`, `where`, `exec`, shims, the
shell hook, and `use`, `config`, `source` — use the same source resolution.
There are no new flags for external projects; `--global` still means "user-wide
default" and is unrelated to project bindings.

## Logical project root vs. physical config path are separate

After binding, the **logical project root** is still the bound directory:

- Task, deps, and relative resource paths resolve against the logical project
  root (or the location of the declaring file);
- `extends` locates the `.git` repository boundary from the logical project
  root, and a reference crossing that boundary still errors; without `.git`, the
  existing inheritance behavior is preserved. The external config itself may
  live outside the repository; this does not exempt files named by `extends`.
  Inherited tasks run in their declaring directories. Rooted task names are
  relative to the oldest inherited config (for example `//apps/svc/worker:build`);
- Trust is still evaluated per real configuration file and remains
  project-level trust, tied to the config's real path, its project root, and the
  governed content. `relocate` moves the physical path, so the trust record
  becomes stale and must be re-approved rather than following automatically.

`use` updates the selected external lock along with the external configuration.
If that lock cannot be updated, the previous configuration is restored; no
replacement lock is created in the project tree.

A dependency catalog inherited with `extends` keeps paths relative to the
ancestor that declares `[deps]`. A catalog declared in the external leaf
uses the bound project root.

## use, run, and deps inside an external project

- Running `osdk use npm:xxx` in an external project keeps the isolated
  osdk-managed install: it does **not** edit the project's `package.json` and
  does not read or write the package manager's native lock — the configuration
  is not in the project directory to begin with.
- `osdk run` and `osdk deps` still write project outputs normally (build
  artifacts, dependency directories) relative to the logical project root, so
  external configuration **does not promise zero writes to the project
  directory**.

Unregistered files already present at a destination bundle are preserved and
reported as a conflict. `add` publishes a complete bundle at once. `relocate`
prepares and publishes the new bundle before unregistering the old one, so a
preparation failure leaves the old configuration and lock intact. A process
interrupted after publication may leave both bindings; inspect `project list`
and remove the obsolete one after checking its files.

## Relocating and removing bindings

```bash
osdk project relocate OLD NEW      # use when a project root moved (new machine, moved directory)
osdk project remove PATH           # no longer need this external binding
```

- `relocate` moves the binding to the new root path; the external directory's
  `osdk.toml` and `osdk.lock` are preserved. A missing old root path on disk is
  not deleted automatically.
- `remove` deletes only the external binding directory itself: it does **not**
  delete the project files or the shared tool installations. Preview what would
  be removed with `--dry-run` first.

`add` and the new destination of `relocate` reject regular files. `show`
accepts relative paths and walks upward from a child directory; its `root`,
`binding_dir`, config, and lock all describe the selected project.

## What show and list report

- `show [PATH]` (PATH defaults to cwd) prints `project root`, the `selected`
  source (`local` / `external binding` / `none`), the effective `config:` and
  `lock:` paths, the binding directory, and status lines:
  `WARNING recorded root no longer exists`, `WARNING binding config missing`,
  `shadowed by a local osdk.toml/.osdk.toml`. `--json` emits `root`, `source`
  (`local` / `external` / `none`), `config_path`, `lock_path`, `binding_dir`,
  `shadowed_local_config`, and `missing`.
- `list` takes no positional argument. Plain-text output prints one block per
  binding: the project root, then indented `source: external binding`, `status:`
  (`active`, or a combination of `project root missing`, `config missing`,
  `shadowed by local osdk.toml`), plus the `config:` and `lock:` paths.
  `--json` emits an array with `root`, `binding_dir`, `config_path`,
  `lock_path`, `root_exists`, `config_exists`, and `shadowed_by_local`.

## Explicitly out of scope

An external binding is a path binding: it has no Git remote, branch, or
multi-profile concept, and it does not swap configuration when you switch
branches. If you need several configurations side by side, organize them the
regular way inside configuration files.

Local dependency projects retain a separate `osdk.lock` per installation root. An external bundle currently holds only one manifest per dependency provider. `deps --list --all` can inspect all entries, but materialization and verification reject conflicting manifests before running tools; use separate external bindings for independent projects with the same provider. An existing record is never silently replaced by another project. Supporting multiple records for one provider in one bundle requires a future lock-format design.
