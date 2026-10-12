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
installed, stays on PATH once activated, and `osdk exec` still reaches it. Run
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
