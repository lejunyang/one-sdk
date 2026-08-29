# Go Developer Tool Implementation

This page describes the `go:<module-or-command-path>` dynamic backend behind
the user-facing [Go Developer Tools](../go-tools) workflow. It deliberately
keeps the Go package provider separate from the bare `go` runtime backend.

## Canonical identity and selection

The central `GO_SCHEMA` validates the module/command path, selector, build tags,
and allowlisted build environment before the dynamic registry creates a
`GoPackageBackend`. The host is lowercase DNS text; remaining path components
preserve case and reject URL delimiters, traversal, hidden components, control
characters, and whitespace. Selectors are `latest`, one/two-component numeric
prefixes, exact semantic versions, or canonical Go pseudo-versions.

For `latest`, osdk queries each candidate's `@latest` endpoint. Prefix requests
consume the proxy's bounded `@v/list`; exact requests must be confirmed by a
matching `.info` response. Module-proxy escaping follows Go's uppercase `!x`
form. For nested commands, candidate module roots are tried longest first and
sources are tried in their ranked order within each candidate, so a shorter
module on a faster proxy cannot shadow an existing longer module.

The default proxies are `proxy.golang.org` and `goproxy.cn`. Source selection
uses osdk's probe ranking/cache or configured order/pin. Metadata responses are
bounded to 4 MiB for version lists and 64 KiB for exact/latest records, with a
30-second deadline. Live candidates are exhausted before stale metadata is
accepted. Proxy URLs must be canonical HTTPS without credentials, query,
fragment, or trailing slash; loopback HTTP exists only for tests. When custom
sources are configured without a pin, Go resolution is intentionally restricted
to those custom candidates so private module paths are never probed against the
public defaults. Sources with custom headers are rejected because the native
provider cannot preserve their credential scope.

## Exact managed Go dependency

CLI orchestration injects an active/configured bare `go` request when a `go:`
request has no explicit runtime. It partitions that runtime ahead of dependent
tools, installs or resolves it first, then adds the exact runtime version to
private request metadata. Global `use` reads the user-level Go selection instead
of a nearer project override.

The backend requires a complete osdk-managed Go root and computes a
`b3-go-v1:` identity over its build-critical inventory: `VERSION`, optional
`go.env`, `bin`, `pkg`, `src`, and optional `lib`/`misc`. Regular files are
hashed; symlinks are accepted only when they resolve inside that runtime or
osdk's store. A locked atomic receipt caches the identity while the sorted
path/size/mtime/symlink-target inventory is unchanged. This is a performance
cache for osdk-managed immutable runtimes, not a same-user security boundary: a
process able to rewrite payload bytes, timestamps, and osdk state is outside the
threat model.

## Provider environment

Installation invokes the exact managed `go` executable once, without a shell,
with null stdin, one-hour timeout, and 1 MiB stdout/stderr bounds.
`CommandSpec::clear_env` removes ambient settings and osdk supplies:

```text
HOME, USERPROFILE = <stage>/home
GOROOT             = <exact managed Go root>
GOBIN              = <stage>/bin
GOPATH             = <stage>/gopath
GOMODCACHE         = <cache>/pkg/go-mod
GOCACHE            = <cache>/pkg/go-build
GOENV              = off
GOTOOLCHAIN         = local
GOPROXY             = <one selected proxy>
GONOPROXY           = none
GONOSUMDB           = <discovered module root>
GOSUMDB             = off
PATH                = <exact managed Go bin> + sanitized system paths
TMPDIR, TEMP, TMP   = <stage>/tmp
GIT_TERMINAL_PROMPT = 0
```

The command is `go install [-tags TAGS] <command-path>@v<exact-version>`. The
proxy is the only allowed network source after provider launch; `direct` and
fallback lists are not appended. Disabling the checksum database avoids an
independent network/credential boundary, so integrity is delegated to that
selected proxy. `CGO_ENABLED=1` is rejected until a C compiler/linker can be
selected and bound into the identity.

Module and build caches are shared only inside osdk's cache root. Private home,
GOPATH, and temporary data remain in the stage and are removed before
publication. Neither dependency cache enters osdk's archive CAS.

## Publication and validation

`NativeToolLifecycle` combines the canonical tool/version/platform, public
`tags`/`env`, exact Go dependency and runtime content identity, selected proxy,
module root, and command path into the `b3-v2:` install ID. Its lock covers
candidate validation, provider execution, and publication.

After provider success, osdk removes private workspace directories, writes a
bounded `go-resolution.json`, rejects symlinks/reserved metadata, inventories
regular executables from `bin`, hashes each with SHA-256, writes the native
receipt and dynamic inventory, and publishes the sibling stage with a
no-replace rename. An adjacent metadata seal binds the published metadata.
Reuse, activation, shims, `where`, listing, and uninstall all revalidate this
same identity, receipt, seal, binary content, and managed Go dependency.

## Lock schema 4

Go tools use the shared typed `native` table:

```toml
schema = 4

[platforms.linux-x64.tools.go]
request = "1.24"
version = "1.24.6"

[platforms.linux-x64.tools."go:golang.org/x/tools/gopls"]
request = "0.20"
version = "0.20.0"
options = { tags = "netgo", env = "CGO_ENABLED=0" }

[platforms.linux-x64.tools."go:golang.org/x/tools/gopls".native]
runtime = "go"
runtime_version = "1.24.6"
replay = "version-only"
source = "https://proxy.golang.org"
module = "golang.org/x/tools/gopls"
```

Loading restores the typed fields as private options only after validating the
backend, exact versions, matching same-platform Go entry, proxy, module-root
prefix, and replay class. User-facing `__osdk_*` injection is rejected and those
keys never appear in the public options table. Schemas 1 through 3 cannot
represent this dependency and reject `go:` entries.

`version-only` is intentionally honest: the lock records top-level resolution
and source identity but no transitive module graph or source payload. Cold
offline installation and repair therefore fail before provider execution; only
a complete, exactly matching installation can be reused offline.
