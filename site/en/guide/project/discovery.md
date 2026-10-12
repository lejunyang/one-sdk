# Project Toolchains

osdk looks upward from the current directory for version declarations and understands each ecosystem's native version files. This page covers where the active version comes from and in what order, and how npm tools fit into a Node project. See [Configuration Files](../config/files) for the complete schema.

## Project version discovery

Active-version sources use this type-first priority. Within each type, osdk
walks from the current directory toward ancestors:

1. `[tools]` in `osdk.toml` / `.osdk.toml`;
2. `.tool-versions`;
3. native ecosystem version files;
4. Node metadata in `package.json`;
5. user-global `[tools]`.

Therefore, an ancestor `osdk.toml` still outranks a nearer `.nvmrc`.

| Backend | Native files |
| --- | --- |
| Node.js | `.nvmrc`, `.node-version`, then `package.json#engines.node` and `devEngines.runtime` |
| Python | `.python-version` |
| Java | `.java-version`, `.sdkmanrc` |
| Go | the `go` directive in `go.mod`, `.go-version` |
| Rust | `rust-toolchain.toml`, `rust-toolchain` |
| Maven / Gradle / Kotlin | `.mvn-version`, `.gradle-version`, `.kotlin-version` |
| Bun / Deno | `.bun-version`, `.dvmrc` |

Single-value files use the first non-empty, non-comment line and strip a leading
`v`. Rust TOML reads `[toolchain].channel`. Node npm semver ranges resolve to the
highest matching stable version; an invalid range fails explicitly.

::: warning Current no-argument lifecycle boundary
`current`, shims, and shell hooks read all native files above. No-argument
`lock`, `upgrade`, and `install` when no usable lock exists currently enumerate
merged `[tools]`/`.tool-versions`, then additionally discover `packageManager`
and Node. A lone `.python-version`, `.java-version`, `go.mod`, or
`rust-toolchain.toml` does not automatically add that non-Node tool to those
lifecycle commands. Put it in `[tools]` or pass it explicitly.
:::

## npm tools in a real project

From any directory below a Node project, this command adds the package to the
project rooted at the nearest `package.json`:

```bash
osdk use npm:prettier@3
```

The package stays in its existing `dependencies`, `devDependencies`,
`optionalDependencies`, or `peerDependencies` section; a new package defaults
to `devDependencies`. osdk follows the project's declared `packageManager`, then
the installer that owns the incumbent lock, and otherwise the configured
default; `-o installer=npm|pnpm` forces an explicit choice. It does not retry a
failed operation through another installer. With no
`package.json` in the ancestor chain, the command keeps the legacy isolated
osdk-managed install and shim behavior.

Project-aware use updates the native `package.json` and package-manager lock,
then writes an exact Node selection and structured npm entry to `osdk.toml`. It
also writes a compact `osdk.lock` entry with installer, scope, Node, and native-
lock identity. That metadata does not contain the transitive dependency graph;
the native package-manager lock remains its source. Keep all four project files
together. See [npm Developer Tools](../tools/npm) for installer and activation
details.

For shell activation, `use` also generates local derived state under
`.osdk/npm-bin/`. Only curated launchers for the configured npm tools are
activated; the whole `node_modules/.bin` directory is never added to PATH. Add
`/.osdk/npm-bin/` to an ignore file at the package root, or
`**/.osdk/npm-bin/` at a repository root that contains nested packages, while
continuing to commit the four source-of-truth files above.
