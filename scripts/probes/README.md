# One-off probes

Scripts here answer a specific factual question about an external tool, and are
kept because the answer shaped a design decision that would otherwise rest on
someone's recollection. They are **not** part of CI: each is meant to be run by
hand, on a host that can answer the question, when a claim in
`docs/research/` needs rechecking.

Every one of them is read-only with respect to system state, and says so by
verifying it — a probe that quietly modified the host would be worse than no
probe.

## apt: can a mirror be used without touching `/etc`?

- `apt-ephemeral-source.sh` — runs `apt-get update` against a source file
  outside `/etc` and checks that the system's lists and `/etc/apt` are
  untouched. Run as a normal user.
- `apt-ephemeral-source-as-root.sh` — the same question as root, which is the
  case that matters for containers and CI. This one exists because the
  non-root run passed only by accident of privilege: `Dir::Cache` was missing,
  so apt tried to write `/var/cache/apt/pkgcache.bin` and was merely *denied*.
  It compares the system cache's checksum before and after.
- `apt-sandbox-and-mirror-speed.sh` — as root, apt drops to the `_apt` user to
  download and prints "Download is performed unsandboxed as root" when it cannot
  read the target directory. That is a security property being given up, not
  noise, so this checks that granting `_apt` access keeps the sandbox on. It also
  times three mirrors, which is where the 14× spread between them came from.

Findings from these are recorded in
`docs/research/system-package-managers-2026-09-12.zh-CN.md`, section 7.3.1.
Measured on WSL Ubuntu 22.04 (jammy); numbers will differ elsewhere, which is
the point of re-running them rather than trusting the ones written down.
