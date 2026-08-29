# Go Developer Tools

osdk installs Go command packages through the `go:` namespace. The namespace is
separate from the bare `go` runtime: `go@1.24.6` selects a compiler toolchain,
while `go:golang.org/x/tools/gopls@0.20.0` builds and installs a command with
that exact managed toolchain.

## Install and select a tool

Select a managed Go line, then add the command package:

```bash
osdk use go@1.24
osdk use go:golang.org/x/tools/gopls@0.20.0
eval "$(osdk activate bash)"
gopls version
```

The requests can also be used without changing project configuration:

```bash
osdk install go@1.24.6 go:golang.org/x/tools/gopls@0.20.0
osdk exec --tool go@1.24.6 \
  --tool go:golang.org/x/tools/gopls@0.20.0 -- gopls version
```

Every `go:` tool requires exactly one managed Go request. An explicit request
wins; otherwise osdk injects the active project/configured `go` selection,
installs that runtime first, and binds the resolved exact version to the tool.
The ambient `go` on `PATH` is never used as the provider.

For a user-wide selection, configure both entries globally:

```bash
osdk use --global go@1.24
osdk use --global go:golang.org/x/tools/gopls@0.20.0
```

The tool still lives in an osdk-owned fingerprinted install root; global means
that its selection is written to user configuration and the user lock rather
than to the current project.

## Paths and selectors

The subject is a canonical module or nested command-package path:

```text
go:<module-or-command-path>[@SELECTOR]
```

The module host must be lowercase DNS text, while path case is preserved. Empty
components, `.`/`..`, hidden components, whitespace, URL syntax, and a leading
`v` in versions are rejected. Supported selectors are exact semantic versions
(including canonical prereleases), canonical pseudo-versions, `latest`, and
numeric prefixes:

| Request | Selection |
| --- | --- |
| `go:golang.org/x/tools/gopls` or `@latest` | Proxy `@latest` result |
| `go:golang.org/x/tools/gopls@0` | Highest stable `0.x` version in the proxy list |
| `go:golang.org/x/tools/gopls@0.20` | Highest stable `0.20.x` version |
| `go:golang.org/x/tools/gopls@0.20.0` | Exact semantic version |
| `go:example.com/acme/tool@0.0.0-20240801123456-0123456789ab` | Exact canonical Go pseudo-version |

For a nested command path, osdk checks longest module candidates first. It
records the first module root proven by proxy metadata and passes the complete
command path to `go install`. The provider remains the final authority that the
selected module actually contains that command package.

## Build options

Both public options contribute to installation identity:

| Option | Behavior |
| --- | --- |
| `tags` | Comma-separated Go build tags; normalized, sorted, and deduplicated |
| `env` | Semicolon-separated allowlisted build assignments |

The build-environment allowlist is `CGO_ENABLED=0`, `GOAMD64`, `GO386`, `GOARM`,
`GOMIPS`, `GOMIPS64`, and bounded `GOEXPERIMENT` values. `CGO_ENABLED=1` is
rejected because osdk does not yet select and bind a C compiler/linker identity.
Credentials and network/cache overrides such as `GOPROXY`, `GONOSUMDB`,
`GOMODCACHE`, `GOCACHE`, `GOBIN`, and `PATH` cannot be supplied through this
option.

```bash
osdk use 'go:example.com/acme/tool[tags=netgo,env=CGO_ENABLED=0]@1.2.3'
```

## Proxy selection

The built-in candidates are `https://proxy.golang.org` and
`https://goproxy.cn`. The normal `auto`, `ordered`, and pinned source policies
apply; auto mode probes candidates and caches the ranking before metadata
resolution. Exact versions are still verified with the chosen proxy's `.info`
endpoint.

Custom Go proxies must be canonical HTTPS origins or paths without credentials,
query strings, fragments, or trailing slashes. When custom sources are present
without a pin, only those candidates are used, preventing private module paths
from being probed against public defaults. Custom source headers are rejected
because the later native `go install` process cannot preserve osdk's per-request
credential-forwarding boundary. The selected proxy is passed as the only
`GOPROXY` entry, so a started provider does not silently retry another source.

## Isolation and activation

osdk runs one shell-free `go install <command>@v<version>` in a cleared
environment. It forces the selected managed `GOROOT`, a staged `GOBIN`, private
home/GOPATH/temp directories, shared osdk-controlled module and build caches,
`GOENV=off`, and `GOTOOLCHAIN=local`. `GOSUMDB=off` prevents an unrelated
checksum-service request; source trust therefore follows the selected proxy.

Only regular executable files from staged `bin` are published. Their names,
sizes, and SHA-256 values are recorded, and activation/shims expose only those
validated commands. Different versions, tags, build environments, proxies,
module roots, managed Go versions, or managed runtime contents produce distinct
installation identities.

## Lock and offline behavior

`osdk.lock` schema 4 stores compact native metadata: the exact Go runtime
version, `version-only` replay class, selected proxy, and discovered module
root. It also keeps the public `tags` and `env` options and a matching exact
`go` entry in the same platform table. It does not copy `go.sum` or the complete
transitive module graph into `osdk.lock`.

Consequently, a complete installation with the exact matching identity can be
validated and reused offline. A cold offline build or repair is unsupported,
even when the shared Go caches happen to contain some dependencies.

## Lifecycle commands

```bash
osdk current go:golang.org/x/tools/gopls
osdk list go:golang.org/x/tools/gopls
osdk list-remote go:golang.org/x/tools/gopls
osdk where go:golang.org/x/tools/gopls@0.20.0
osdk outdated go:golang.org/x/tools/gopls
osdk upgrade go:golang.org/x/tools/gopls
osdk --yes uninstall go:golang.org/x/tools/gopls@0.20.0
osdk reshim
```

Changing or removing the selected Go runtime invalidates dependent Go-tool
reuse. If several otherwise matching source/runtime identities exist, select
the exact one through `osdk.lock` instead of allowing an ambiguous activation.
`--global` currently changes selection/lock scope only: `where --global` and
`uninstall --global` remain npm-only CLI operations. Use the ordinary exact
`where`/`uninstall` forms for a Go tool selected in the current environment.

For runtime identity, staging, provider, and lock-schema details, see
[Go developer tool implementation](./implementation/go-tools).
