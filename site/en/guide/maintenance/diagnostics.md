# Diagnostics and Install Verification

`osdk doctor` reports the state of the environment; `osdk doctor --verify` re-hashes installed files to find anything changed after installation.

## Diagnostics

```text
osdk doctor
osdk doctor --verify
osdk doctor --verify --tool TOOL
osdk config path
osdk config list
osdk config get KEY [-g]
osdk source list TOOL
osdk registry test [MANAGER]
```

| Command | Output |
| --- | --- |
| `doctor` | Platform, data/store/install directories, whether store and installs share a filesystem, shim path and PATH presence, backend IDs, and proxy state |
| `doctor --verify` | Everything above, then re-hashes every installed file and names what no longer matches |
| `doctor --verify --tool` | The same check limited to one tool; a full pass reads every byte and takes minutes |
| `config path` | Config directory, user file, and current project configuration |
| `config list` | Selected effective settings/directories, registry, model environment, tools, and alias.tools |
| `source list` | Sources and pin for one backend/provider; `doctor` does not list mirrors |
| `registry test` | Anonymous npm-compatible registry probe and selection plan |
| `container doctor` | Read-only Docker/containerd selection plus a separate Buildx report |

Top-level `doctor` currently does not print `link_mode`; use `config list` to
inspect it. It is distinct from `container doctor`, which diagnoses native
container control planes.

The `doctor` proxy line needs a word of explanation. osdk's HTTP client reads a
proxy only from the environment (`HTTPS_PROXY` / `HTTP_PROXY` / `ALL_PROXY`, in
either case) and **never** from the Windows "system proxy" -- that is a WinINET
registry setting read by browsers and other desktop apps. So with the Windows
toggle on but no such variable set, your browser works while osdk times out;
`doctor` reports "Windows system proxy is ON but osdk ignores it" together with
the exact variable to set, and shows the environment proxy when one is present.
It diagnoses only and never silently adopts the system proxy, because that would
change where traffic goes. Any credentials embedded in a proxy value are
redacted before printing.

## Verifying installed files

osdk verifies bytes as they are downloaded, but until they are checked again
nothing notices when an install changes afterwards. Three things do that: a tool
that updates itself in place, a manual edit, and a half-restored backup or bit
rot. In each case osdk keeps reporting the version it installed while a
different binary actually runs.

`osdk doctor --verify` re-hashes every file recorded in each install's
`.osdk-manifest.json` and reports what no longer matches:

```text
osdk doctor --verify
```

```text
  verifying installed files
  node@20.11.1 : no longer matches what osdk installed
    E:\osdk-data\data\installs\node\20.11.1
      node.exe: contents changed
  checked 9 install(s), 1 changed
  reinstall to restore (a plain install skips what is already there):
    osdk install --force node@20.11.1
```

It reports four kinds of drift: contents changed, missing, file type changed
(a file replaced by a link, or the reverse), and a link that now points
somewhere else. Installs made before the manifest existed are reported as
unverifiable rather than silently passing.

This reads every file, so it is opt-in. Plain `osdk doctor` stays a fast
environment check, and nothing on the execution path hashes anything — running a
tool is not slowed down.

The cost is disk read speed, not hashing. A full pass over 11.6 GB across 32,463
files measured about 4.5 minutes, dominated by two NDKs and a system image.
Name the tool you actually suspect to keep it usable:

```text
osdk doctor --verify --tool node
```

`doctor --verify` checks each dynamic install identity separately. New PyPI installations record file hashes without relocating the virtual environment. Reinstall older installations with `--force` to establish their baseline; bytecode caches are excluded.

That same check took under a second. `--tool` requires `--verify`, and an
unknown name is rejected rather than silently checking nothing.

### Repairing what drifted

A plain `osdk install` treats an existing install as done and returns
immediately, so it will not repair one whose files changed. `--force` reinstalls
over it:

```text
osdk install --force node@20.11.1
```

Only the versions you name are affected. Dependencies that osdk installs on your
behalf are not forced along with them: bringing a dependency into place is not a
reason to rebuild it.

Because installs are hardlinked into the CAS, editing an installed file also
edits the store object behind it. osdk therefore confirms an object still hashes
to the name it is filed under before reusing it, and discards it if not, so a
reinstall genuinely repairs instead of linking the damaged bytes back. Intact
objects are still reused, so deduplication is unaffected.

### What this does not do

Verification is a snapshot, not a policy. It tells you an install no longer
matches what osdk put there; it cannot tell you whether the change was a
legitimate self-update or something hostile, because on disk the two are
identical. Tools that update themselves will keep reporting drift after every
update — for those, `--force` reinstalls the version osdk has pinned, which is
the point of pinning it.
