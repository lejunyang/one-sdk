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
osdk project add [PATH]        # bind a directory as an external project; PATH defaults to cwd
osdk project show [PATH]       # show which project source this directory actually uses
osdk project list              # list every external project binding
osdk project relocate OLD NEW  # move a binding from one root path to another
osdk project remove PATH       # drop an external binding; --dry-run previews first
```

```bash
osdk project add .                 # bind the current directory
osdk project add C:\repos\frontend # bind any directory, exactly as given
osdk project show .                 # which configuration this directory resolves to
osdk project list                   # every binding at a glance
osdk project remove ~/tmp/old-checkout --dry-run
osdk project remove ~/tmp/old-checkout
```

## Bind behavior

- `add` canonicalizes the directory you give to its real absolute path and binds
  that: relative paths and junction aliases resolve to their target; it does not
  walk up to guess a Git root, installs nothing, and does not modify the directory.
- Binding an already-bound directory again does not overwrite or change the
  existing binding.
- A directory that already contains a local `osdk.toml` / `.osdk.toml` can
  still be bound; at runtime the local file wins, and osdk warns on stderr that
  the external configuration is shadowed.

## Where external configuration lives

Every binding occupies one subdirectory under the config directory:

```text
<OSDK_CONFIG_DIR>/projects/<root-id>/
  project.toml    # schema = 1; root records the project root's real absolute path
  osdk.toml       # the external project's tool and task declarations
  osdk.lock       # the external project's lockfile
```

- `root-id` is a full BLAKE3 of the platform-aware path identity. It is **not
  derived from the project name or any Git remote**; renaming the project or
  changing its remote does not disturb the binding.
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

- A local project without a lock never borrows the external one; an external
  project without a lock generates its own inside the external directory.
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
  existing inheritance behavior is preserved;
- Trust is still evaluated per real configuration file and remains
  project-level trust, tied to the config's real path, its project root, and the
  governed content. `relocate` moves the physical path, so the trust record
  becomes stale and must be re-approved rather than following automatically.

## use, run, and deps inside an external project

- Running `osdk use npm:xxx` in an external project keeps the isolated
  osdk-managed install: it does **not** edit the project's `package.json` and
  does not read or write the package manager's native lock — the configuration
  is not in the project directory to begin with.
- `osdk run` and `osdk deps` still write project outputs normally (build
  artifacts, dependency directories) relative to the logical project root, so
  external configuration **does not promise zero writes to the project
  directory**.

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

## What show and list report

- `show [PATH]` reports which source this directory resolves to (external
  binding or local configuration), the physical path of the external binding,
  whether it is shadowed by a local file, and whether the bound project root
  still exists on disk (missing).
- `list` enumerates every external binding with its source and status.

## Explicitly out of scope

An external binding is a path binding: it has no Git remote, branch, or
multi-profile concept, and it does not swap configuration when you switch
branches. If you need several configurations side by side, organize them the
regular way inside configuration files.
