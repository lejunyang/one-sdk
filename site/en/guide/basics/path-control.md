# Choosing Which Commands Reach PATH

An install often brings more executables than you asked for: a conda prefix holds
an entire dependency closure, and an Android NDK ships 172 of them.
`[settings.shims]` decides which ones get a shim and reach PATH.

Withholding a shim is not the same as not installing: the file stays in the
install directory and remains available through `osdk exec` and an activated
shell.

## The three lists

| Setting | Meaning | Scope |
| --- | --- | --- |
| `include` | **Allowlist**: when non-empty, anything not listed is withheld | every tool |
| `expose` | **Additive**: also shim these, without affecting any other tool | every tool |
| `exclude` | Skipped, applied last, trims the result of the other two | every tool |

Order: `include` picks the candidate set → `expose` adds → `exclude` removes.

`expose` wins over a narrow `include` elsewhere. When both match a name, one says
"shim this" and the other says "not on the list"; letting `include` win would mean
silently dropping an explicit request.

## `include` is a global allowlist, not "add one back"

This is the easiest mistake to make:

```bash
# Dangerous: meant to recover make, loses the shims for cargo / go / node
osdk config set shims.include "conda:m2-base:make"
```

Once `include` is non-empty it becomes an allowlist over **every** tool, so naming
one command declares "and nothing else". In practice that single command took 646
shims down to zero.

To recover a withheld command, use `expose`:

```bash
# Safe: only ever adds
osdk config set shims.expose "conda:m2-base:make"
```

Nothing is wrong with `include` itself — it means "only these", which is the right
thing when you genuinely want to narrow down. Prefer the per-tool form below.

## Per-tool overrides

All three lists can be scoped to a single tool, keyed `shims.<tool>.<field>`:

```bash
osdk config set shims.conda:m2-base.expose  "make,sh,bash,tr,awk"
osdk config set shims.android-ndk.include   "clang,clang++,llvm-strip"
osdk config set shims.conda:m2-base.exclude "ls,test"

osdk config get   shims.conda:m2-base.expose
osdk config unset shims.conda:m2-base.expose
```

In TOML:

```toml
[settings.shims.tools."conda:m2-base"]
expose = ["make", "sh", "bash", "tr", "awk"]
```

Scoping is what makes `include` safe: an `include` under `android-ndk` can only
affect the NDK's own commands and cannot take `cargo` away. **When narrowing one
tool, prefer this form over the global `include`.**

Overrides are **per field**: a field you set replaces the global one, a field you
leave out inherits it. Adjusting only `expose` therefore does not clear the global
`exclude`. `config get` prints `inherit` for an unset field, so it is
distinguishable from one explicitly set to an empty list.

Tool keys match a backend id **exactly** — no globs. Use the global lists for
patterns that span tools.

## Pattern syntax

Every element of the three lists is a pattern, with the same rules:

| Form | Matched against | Example |
| --- | --- | --- |
| no `:` | the command name | `make`, `clang*` |
| contains `:` | `<backend>:<command>` | `conda:m2-base:make`, `android-ndk:*` |

- `*` matches any run of characters, `?` matches exactly one;
- matching is case-insensitive (Windows executables are too);
- patterns match the whole name, not a substring.

Both forms work inside a per-tool list, but since the scope is already fixed, a
bare command name reads better there.

## Metapackages: no shims at all by default

A metapackage such as `conda:m2-base` installs no commands of its own — everything
in the prefix belongs to the packages it pulls in — so nothing is shimmed by
default. That is deliberate: otherwise the msys `ls`, `test` and `sort` would
shadow their Windows namesakes.

Name the ones you need with `expose`:

```bash
osdk config set shims.conda:m2-base.expose "make,sh,bash,tr,awk,grep,printf"
osdk reshim
```

Exposing only what you actually use keeps the rest from interfering with system
commands.

## Reshim after changing these

`config set` edits configuration only; it does not touch the shims on disk. Follow
it with:

```bash
osdk reshim
```

`osdk where --bins <tool>` verifies the outcome, listing `published` (shimmed) and
`withheld` separately.

::: tip These settings do not need trust
`settings.shims` only decides which commands get a shim. It neither executes code
nor changes where downloads come from, so writing it into a project `osdk.toml`
never asks for trust. See
[Which keys require trust](../config/trust#which-keys-require-trust) for the ones that do.
:::
