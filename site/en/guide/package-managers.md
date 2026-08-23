# JavaScript Package Managers

osdk treats npm, pnpm, and Yarn as independently lockable backends and also
manages Bun and Deno directly. Before commands that may fetch npm packages, it
can select a registry without rewriting the project's lockfile.

## Install and commands

All managers use the common lifecycle commands:

```text
osdk install MANAGER[@VERSION]... [-o|--opt KEY=VALUE ...]
osdk lock [MANAGER[@VERSION] ...] [-o|--opt KEY=VALUE ...]
osdk upgrade [MANAGER[@VERSION] ...] [-o|--opt KEY=VALUE ...]
osdk use|u MANAGER[@VERSION] [-g|--global] [-o|--opt KEY=VALUE ...]
osdk exec (-t|--tool TOOL[@VERSION])... -- COMMAND [ARG ...]
osdk list|ls [MANAGER]
osdk list-remote|lsr MANAGER [FILTER]
osdk current [MANAGER]
osdk where MANAGER[@VERSION]
osdk uninstall|rm MANAGER@VERSION
```

```bash
osdk install npm@11.5.2
osdk install pnpm@10.15.0 yarn@4.9.2
osdk install bun@latest deno@latest
```

`-o/--opt KEY=VALUE` is accepted by the shared install, lock, upgrade, and use
interfaces, but these package-manager backends currently define no dedicated
backend options.

| Backend | Installed artifact and verification | Exposed commands | Adds Node automatically |
| --- | --- | --- | --- |
| npm | npm registry `npm` package, SRI | `npm`, `npx` | Yes |
| pnpm | Standalone `@pnpm/<os>-<arch>` package, SRI | `pnpm`, `pnpx`; the latter routes to `pnpm dlx` | Yes |
| Yarn 1 | `yarn` package, SRI | `yarn`, `yarnpkg` | Yes |
| Yarn 2+ | `@yarnpkg/cli-dist` package, SRI | `yarn`, `yarnpkg` | Yes |
| Bun | `@oven/bun-*` platform package, SRI | `bun`, `bunx`; the latter routes to `bun x` | No |
| Deno | `@deno/*` platform package, SRI | `deno` | No |

npm and Yarn launchers use Node from `PATH`. Whenever a request contains npm,
pnpm, or Yarn but not Node, osdk adds Node automatically: it first follows
project discovery and falls back to `latest`. This also applies to explicit tool
lists. Manager bins precede the managed Node bin, so they do not depend on a
user-global Node.

## `packageManager` discovery

osdk discovers exact npm, pnpm, or Yarn versions in this order:

1. npm/pnpm/Yarn in `[tools]` from the nearest ancestor project configuration;
2. the nearest ancestor `package.json#packageManager`;
3. `devEngines.packageManager` in that `package.json` (an object or first array entry).

```json
{
  "engines": { "node": ">=20 <23" },
  "packageManager": "pnpm@10.15.0"
}
```

Only exact `npm|pnpm|yarn@semver` values are supported. Missing versions,
Bun/Deno, URLs, paths, and values with a `#`, hash, or `+` build suffix fail.
`packageManager` wins over `devEngines.packageManager`. No-argument
`install`/`lock`/`upgrade` automatically add the manager and Node; `current`
reports the corresponding `package.json` source.

## Registry preflight

The diagnostic command has this complete syntax:

```text
osdk registry test [MANAGER]
```

`MANAGER` accepts `npm|npx`, `pnpm|pnpx`, `bun|bunx`, `deno`, and
`yarn-classic|yarn1|yarn@1` or
`yarn-berry|yarn2|yarn3|yarn4|yarn@2|yarn@3|yarn@4`. For bare
`yarn|yarnpkg`, osdk tests only the project-determined major when available, or
both Classic and Berry otherwise. Omitting the manager checks npm, pnpm, one or
both applicable Yarn families, Bun, and Deno. It probes only; it installs nothing.

```bash
osdk registry test
osdk registry test pnpm
osdk registry test yarn
```

### Automatically covered invocations

Direct shims, shell activation, and `osdk exec` evaluate these commands before
starting the manager:

| Program | Commands that trigger preflight |
| --- | --- |
| `npm` | `install`, `i`, `ci`, `add`, `update`, `up`, `exec` |
| `npx` | An invocation with a positional target, or `--package/-p` or `--call/-c` fetch forms |
| `pnpm` | `install`, `i`, `add`, `update`, `up`, `fetch`, `dlx`, `deploy` |
| `pnpx` | An invocation with a positional target |
| Yarn 1/2+ | No subcommand, or `install`, `add`, `upgrade`, `up`, `dlx`, `create` |
| `bun` | `install`, `i`, `ci`, `add`, `update`, `x` |
| `bunx` | An invocation with a positional target |
| `deno` | `add`, `bench`, `cache`, `check`, `ci`, `compile`, `doc`, `eval`, `info`, `install`, `outdated`, `run`, `serve`, `task`, `test`, `update` |

If the Yarn major cannot be determined, the execution path passes through
without guessing. Child arguments after a launcher/script operand are not
misclassified as manager arguments.

### Cases that skip preflight

The manager keeps its native choice without probing or injection when:

- osdk itself uses `--offline`;
- command arguments explicitly select a registry;
- the corresponding registry environment variable is already set;
- manager arguments select another working directory or config file;
- npm/pnpm/Yarn/Bun use real `--offline`, or Deno uses `--cached-only` on a supported command;
- the invocation is `--help`, `-h`, `--version`, `-v`, or a command outside the allowlist;
- native configuration cannot be parsed safely or contains a private, unknown, scoped, authenticated, TLS-customized, proxied, or custom-config registry policy.

`--prefer-offline`, frozen lockfile, and immutable-cache modes do not guarantee
no network, so preflight can still run. Deno has no universal `--offline`; its
`ci`, `outdated`, and `update` do not support `--cached-only`.

### Candidate selection and single execution

```toml
[registries.npm]
urls = [
  "https://registry.npmmirror.com/",
  "https://registry.npmjs.org/",
]
probe_timeout_ms = 1500
```

| Manager | Only this variable is injected after selection |
| --- | --- |
| npm/npx | `npm_config_registry` |
| pnpm/pnpx | `pnpm_config_registry` |
| Yarn 1 | `YARN_REGISTRY` |
| Yarn 2+ | `YARN_NPM_REGISTRY_SERVER` |
| Bun | `BUN_CONFIG_REGISTRY` |
| Deno's npm layer | `NPM_CONFIG_REGISTRY` |

With no configured list, built-in npmjs and npmmirror candidates are probed
anonymously in parallel and the lowest-latency healthy endpoint wins. An
explicit user/project list preserves order and selects the first healthy entry;
project `[registries]` replaces the complete user section. Every eligible
invocation probes afresh. If all candidates fail, the manager starts zero times.
Once it starts, failure is returned without switching and replaying scripts,
lockfile writes, or `node_modules` changes.

osdk reads `.npmrc`, `.yarnrc`, `.yarnrc.yml`, `bunfig.toml`/`.bunfig.toml`, and
related user/global files, but only recognized public npmjs/npmmirror endpoints
become candidates. It does not rewrite project lockfiles or intercept network
traffic; absolute artifact URLs in metadata or lockfiles may bypass the default
registry.

## Native cache semantics

These locations preserve each tool's native format; they are not a unified
cross-manager tarball CAS.

| Tool | Environment | osdk path and meaning |
| --- | --- | --- |
| npm | `npm_config_cache` | `<cache>/pkg/npm`, npm cacache |
| pnpm <=10 | `PNPM_HOME`, `npm_config_store_dir` | `<cache>/pkg/pnpm` and `<cache>/pkg/pnpm-store` |
| pnpm >=11 | `PNPM_HOME`, `pnpm_config_store_dir` | Same paths; only the store variable changes |
| Yarn 1 | `YARN_CACHE_FOLDER` | `<cache>/pkg/yarn-classic` |
| Yarn 2+ | `YARN_GLOBAL_FOLDER` | `<cache>/pkg/yarn` |
| Bun | `BUN_INSTALL_CACHE_DIR` | `<cache>/pkg/bun` |
| Deno | `DENO_DIR` | `<cache>/pkg/deno`; also compiled artifacts and some runtime state |

Yarn 2/3 still defaults to project `.yarn/cache` as its active cache, while the
default mirror uses `${YARN_GLOBAL_FOLDER}/cache`. Yarn 4 enables global cache
by default. osdk does not force `enableGlobalCache` or `cacheFolder`.

Shims, `exec`, and shell hooks apply these variables when the relevant backend
is active and preserve an existing user value by default. `osdk cache clean`
removes only osdk's downloaded SDK archives, not these native caches. See
[Storage, Shell, and Extensions](./storage-shell) for the complete boundary.
