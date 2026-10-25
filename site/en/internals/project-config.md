# External Project Config Bindings

The user-facing workflow lives in [External Project Configuration](../guide/project/external-config). This page explains the design trade-offs: why the physical config path is separated from the logical project root, why root-id is a content-derived path hash, and which silent states were deliberately turned into fail-closed errors.

## Motivation: the blind spot of a local-config-first model

The existing model assumes project configuration always lives in `osdk.toml` inside the project directory: discovery walks up from cwd, `use`/`lock` write back there, and trust records hang on that file. It cannot cover three common cases: read-only checkouts of third-party repositories, codebases whose files you must not touch, and multi-project machines where you want all configuration managed centrally under `OSDK_CONFIG_DIR`. The binding design deliberately adds no second configuration format: the external directory holds ordinary `osdk.toml` and `osdk.lock`, only physically located under the config directory, with one anchor file pointing them back at the real project root.

## root-id: a content-derived directory name, not an index

The `root-id` of `<config>/projects/<root-id>/` is the full BLAKE3 of the platform-aware normalized project-root path. It is deliberately not the project name or a Git remote:

- Changing a project display name or remote does not change the binding. Renaming or moving its directory changes the canonical path and requires relocation.
- The directory name *is* the identity, so there is **no separately editable index file**. The `<config>/projects/` directory itself is the registry: enumerate it to discover every binding. The class of inconsistency "index entry points at a directory that no longer exists" cannot exist by construction.
- `project.toml` (schema = 1) records exactly one thing: `root`, the real project root's absolute path. Once the physical directory and logical root are separated, the runtime needs an explicit anchor to point back at — otherwise the correspondence "configuration lives at X, project lives at Y" has nowhere to live.
- The path identity is decided by the real path after OS canonicalization (dunce): junctions, 8.3 short names, and case aliases resolve to the same identity; casing follows filesystem semantics — osdk neither lowercases additionally nor applies Unicode normalization. The same directory written differently therefore yields exactly one binding.

The boundary check exempts only the initial external leaf. All inherited
files are checked against the logical repository, using the same canonical
path representation on Windows. Task-root rebasing uses each layer's logical
directory; loading and trust checking share the oldest inherited task anchor.

Discovery normalizes the starting directory before walking ancestors, so
relative CLI paths and symlink aliases reach the same selected context.
`show` reports that context's root rather than the input subdirectory.

Bundle publication stages files beside `projects/` on the same filesystem,
then renames the complete directory into place; enumeration cannot see staging
manifests. Relocation copies config/lock before publishing a new manifest and
unregistering the old bundle. Publication failures preserve the source. A crash
after publication can leave two complete registrations; this is recoverable
with `project list` and `remove`, not a cross-directory transaction. Unknown
files left in the old bundle are preserved during cleanup.

Tool selection writes and rollback snapshots both use the selected
ProjectContext lock path, while the metadata mutex remains keyed on the
logical project root.

Dependency discovery compares the selected leaf with `deps_anchor`: only an
external leaf receives the logical-root override. Inherited catalogs and their
own inheritance chains retain the declaring ancestor as their base.

## Single-writer source selection: config and lock as one set

While walking up from cwd, candidates include both a local `osdk.toml` and bound external projects. The selection rules:

1. The nearest project wins;
2. Within the same project root, local configuration outranks external;
3. A nearer external sub-project can outrank a farther local parent.

Once a source is chosen, **`osdk.toml` and `osdk.lock` are always used as a set**:

- A binding is born with a lock: `add` atomically writes `osdk.lock`; its initial body is the current ordinary lock schema — the same `schema_version` authority as the normal writer, with no hard-coded version. If a lock later goes missing (e.g. deleted by hand), it is rebuilt from the currently selected source inside that same directory — never borrowed from another source. Mixing the two would produce an environment whose "configuration came from A, resolution came from B" — harder to debug than a missing lock.
- A corrupted selected configuration or lock fails explicitly instead of falling back to another source; a silent fallback would make the user believe the external configuration was in effect.
- The body of a shadowed external configuration is never read. The most dangerous silent fork is "the local file exists but is incomplete, so osdk quietly backfills the external body"; the rule is shadowed = not read at all, with a stderr warning naming the adopted and ignored sources.
- `current`, `where`, `exec`, shims, the shell hook, and `use`/`config`/`source` share the same source resolution, so activation and lock writing cannot pick different sources.

Runtime lock-consumption semantics do not expand with external bindings: an external project only changes the lock's physical location; there is no per-tool mixing or cross-source merging.

## Three consequences of separating logical root from physical path

- **Relative paths resolve against the logical root.** Tasks, deps, and relative resource paths resolve against the bound project root (or the declaring file's location), not the external directory — resolving against the external directory would point at entirely the wrong place.
- **`extends` keeps logical ancestors and repository boundaries.** The boundary is found by walking up from the logical project root to `.git`; references crossing it still error and external bindings get no exemption. Without `.git`, the existing inheritance behavior is preserved.
- **Trust is approved per real config file; relocate triggers re-approval.** The trust store ties records to the config's real path, project root, and governed-content hash. `relocate` moves the physical directory, so the trust record becomes stale and must be re-approved with `osdk trust`. This is deliberate: if trust followed automatically, one approved configuration could silently widen its approved surface to a new path.

## Write boundary: external configuration does not promise project-zero writes

An external `osdk use npm:xxx` keeps the isolated install and never touches the project's `package.json` or native lock — the configuration is not in the project directory to begin with. But `osdk run` and `osdk deps` still write build artifacts and dependency directories against the logical project root. The docs therefore never promise "external config = read-only project", which would disappoint anyone placing it on a truly read-only volume.

## Failure modes and handling

| State | Handling |
| --- | --- |
| The bound directory already contains a local `osdk.toml` | Binding is allowed; the local file wins, the external config is shadowed, stderr warns |
| Re-`add`ing the same directory | The existing binding is not overwritten |
| A selected config/lock is corrupted | Hard error; no source fallback |
| The bound project root no longer exists on disk (missing) | Plain-text `list`'s `status` and `show`'s WARNING lines expose it directly (`project root missing` / `config missing`); `--json` additionally gives `root_exists` / `config_exists` / `missing`; `relocate` preserves config and lock; nothing is auto-deleted |
| `remove` | Deletes only the external binding directory; project files and the shared install pool are untouched; `--dry-run` previews |

## Explicitly not done

- No Git-root guessing: `add` binds the given path exactly, so "the bound directory" cannot disagree with "the project root the user meant";
- No tool installation is triggered;
- No Git remote, branch, or multi-profile concept — a binding is a path binding, and switching branches does not swap configuration;
- No files are ever generated inside the project directory.

Local dependency projects retain a separate `osdk.lock` per installation root. An external bundle currently holds only one manifest per dependency provider. `deps --list --all` can inspect all entries, but materialization and verification reject conflicting manifests before running tools; use separate external bindings for independent projects with the same provider. An existing record is never silently replaced by another project. Supporting multiple records for one provider in one bundle requires a future lock-format design.
