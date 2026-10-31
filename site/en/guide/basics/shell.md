# Shell Activation and Completions

Shims let any process use the right version for its directory; shell activation switches the whole environment at the prompt. This page covers how both work, how to hook them up, and completion scripts. See [Choosing Which Commands Reach PATH](./path-control) for deciding which commands get shims.

## Shims and shell activation

Installation and `use` generate shims. Regenerate them after moving installation
directories with:

```text
osdk reshim
```

For dynamic tools, `reshim` publishes launchers only for the exact configured
`.osdk-install.json` schema-1 identity. It does not select another same-version
root, and legacy `.osdk-tool.json` state is never executable.

Each shim resolves the version for the current directory at execution time, so
project pins also work in IDEs, CI, and processes without a prompt hook. Shims
avoid recursively invoking themselves. On Windows, `.cmd`/`.bat` targets run via
`%ComSpec% /D /S /C call` to preserve arguments, standard I/O, and status. The
generated `.cmd` wrapper writes the install path in the system OEM code page so a
console-less cmd that falls back to the OEM code page can still reach a non-ASCII
install path; characters the OEM code page cannot express fall back to a
`chcp 65001` line before the quoted path and UTF-8 bytes.

Shell activation syntax is:

```text
osdk activate bash|zsh|fish|powershell|pwsh
osdk deactivate bash|zsh|fish|powershell|pwsh
```

Common setup forms are:

```bash
eval "$(osdk activate bash)"
eval "$(osdk activate zsh)"
osdk activate fish | source
osdk activate powershell | Invoke-Expression
```

The hook recomputes the environment at prompts/directory changes. `PATH` order is:

```text
generated shims > independent npm/pnpm/yarn > Node > other backends > original PATH
```

It also exports backend environment, downstream caches, and enabled model
adapters. osdk stores the original value of every managed variable; `deactivate`
removes the hook and restores that environment. The PowerShell hook prevents
re-entrant command lookup.

### Without a shell hook

IDEs, CI steps, containers, scheduled jobs and `pwsh -NoProfile` never run the
hook. Shims are on the persistent PATH, so the commands themselves still pick the
right version for their directory; what is missing is what the hook exports --
backend environment (`JAVA_HOME`, `ANDROID_HOME`, ...), downstream cache
directories and model adapters -- plus any command without a shim. Two entry
points supply it, both sharing the hook's own computation:

```bash
osdk env                           # dotenv, the default
osdk env --format json
osdk env --format bash             # or zsh, fish, powershell
osdk exec -- cargo build           # without --tool, run under that environment
```

- **dotenv / json** spell out the complete PATH, since whoever reads a file
  cannot say "prepend to the existing PATH". It is built on the PATH from before
  any activation, so exporting from an activated shell does not write another
  directory's tools into the file. dotenv values are unquoted: `docker --env-file`
  keeps quotes as part of the value, while most dotenv parsers strip them. A
  multi-line value cannot be written as dotenv and is refused with a pointer to
  json.
- **Shell formats** are plain `export`s that prepend to the existing PATH, without
  the hook's `OSDK_ORIG_*` / `OSDK_MANAGED_ENV` bookkeeping. They are meant to be
  evaluated once in a shell that is not activated or a CI script, e.g.
  `eval "$(osdk env --format bash)"`. Do not put one in an rc file in place of
  `activate`: it does not follow directory changes.
- **`osdk exec -- <command>`** without `--tool` installs nothing; a selected tool
  that is not installed fails just as it does in an activated shell, so run
  `osdk install` first. Called from an activated shell, a variable the outer
  activation set and this directory does not goes back to its pre-activation
  value. With `--tool` the behavior is unchanged.

The output is the environment of the **current directory**, and project
selection differs between directories; export again when an IDE opens another
project.

### Choosing which shims to generate

Every executable gets a shim by default. `[settings.shims]` narrows that:

```toml
[settings.shims]
include = []                      # non-empty shims only matching names -- across **every** tool
exclude = ["android-ndk:*"]       # applied last, so it always wins
expose = []                       # additively shims withheld commands, affecting no other tool

[settings.shims.tools."conda:m2-base"]
expose = ["make", "sh"]           # per-tool override; fields left out inherit the global list
```

`include` is an allowlist over **every** tool; `expose` only ever adds. Use
`expose` to recover a withheld command, and the `include` under
`[settings.shims.tools."<id>"]` to narrow one tool, where the scope is confined to
that tool. See
[Choosing which commands reach PATH](./path-control)
for the full semantics and pattern syntax.

Patterns accept `*` and `?` and ignore case; a `backend:name` pattern applies to
that backend only. Excluding a name only withholds the shim -- the tool stays
installed, stays on PATH once activated, and `osdk exec` still reaches it. The
exception is an install that shares its prefix with a dependency closure, such as
conda: activation puts none of its directories on PATH and reaches it through its
shims only (see [Choosing which commands reach PATH](./path-control)). Run
`osdk reshim` to apply a change.

Generation and routing read the same decision, so an excluded backend cannot
win a shared command name at run time either.

## Completions

```text
osdk completions bash|elvish|fish|powershell|zsh
```

The command writes completion code to stdout; save or source it according to the
shell's conventions. For example:

```bash
osdk completions bash > ~/.local/share/bash-completion/completions/osdk
osdk completions zsh > ~/.zfunc/_osdk
osdk completions fish > ~/.config/fish/completions/osdk.fish
```
