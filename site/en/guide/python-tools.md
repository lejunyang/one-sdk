# Python Developer Tools

osdk installs Python command-line tools from PyPI through the `pypi:` namespace,
giving each tool its own virtual environment.

::: tip What makes this backend different
`github:` and `http:` download an archive and are done. A Python tool is not
self-contained: it needs an interpreter, a virtual environment, and a dependency
tree that can run deep. So osdk does not resolve Python dependencies itself --
that would mean implementing PubGrub, PEP 440 version semantics, PEP 425 wheel
tag selection, and PEP 517 source builds, and the consequence of getting it wrong
is not slowness but **installing the wrong package**.

osdk owns what it is genuinely better placed to own: index mirror selection,
cache placement, fail-closed gating, and install layout. Resolution and
installation are delegated to a managed subprocess.
:::

## Installing

```bash
osdk install pypi:cowsay@6.1
osdk use pypi:cowsay@6.1          # select a version so the shim knows which to run
cowsay -t hello
```

`install` generates shims automatically; there is no need to run `osdk reshim`.
As with every other backend, though, `install` only puts the tool on disk and
**does not pin a version** -- that is what `osdk use` writes. When no version is
selected the shim says exactly what to run.

You can pin it in a project instead:

```toml
[tools]
"pypi:cowsay" = "6.1"
```

`latest` and version ranges resolve by reading the index, the same as every
other backend:

```bash
osdk install pypi:cowsay@latest
osdk list-remote pypi:cowsay
```

::: tip Versions are not semver
Python versions have no fixed segment count: `6.1` and `2026.7.22` are both
complete versions, not prefixes.

Ordering follows PEP 440 rather than string order, so `0.10.0` ranks above
`0.9.0`. Pre-releases (`1.0rc1`, `2.0b3`, `3.0.dev1`) are never chosen by
`latest` — they carry no `-`, so they need recognising separately — but an
explicit request for one still installs.
:::

## Leaving off the prefix

The `pypi:` prefix is required, because the same name usually exists in more than
one channel. osdk will not guess for you, but it does list the channels that
provide it, along with what separates them:

```
$ osdk install uv
error: `uv` is not a backend on its own, but these namespaces provide it:
  osdk install pypi:uv  (latest 0.12.14, published by the project itself)
  osdk install conda:uv  (repackaged by conda-forge, so it can lag upstream)
```

These are not equivalent. The conda-forge uv is built from `astral-sh/uv` by the
feedstock, so the code is genuine, but packaging is done by community volunteers
and it measured one release behind PyPI (0.12.13 vs 0.12.14). That is why `pypi:`
is the better choice when both work.

Once installed, the bare command works without any prefix — the shim takes over:

```bash
osdk use --global pypi:uv
uv --version
```

## Names and extras

Project names are normalized per PEP 503: runs of `.`, `-`, and `_` are
equivalent and comparison is case-insensitive. All of the following are therefore
**one tool** rather than several install directories:

| What you write | Normalized |
| --- | --- |
| `pypi:Zope.Interface` | `pypi:zope-interface` |
| `pypi:zope_interface` | `pypi:zope-interface` |
| `pypi:Typing-Extensions` | `pypi:typing-extensions` |

PEP 508 extras are an option rather than part of the name:

```bash
osdk install "pypi:httpx[extras=socks]@0.27.0"
```

Extras are part of the **install identity**. `pypi:httpx` and
`pypi:httpx[extras=socks]` are two different installs -- otherwise the second
request would silently reuse the first environment and the extra would never be
installed. Extras are sorted (`[extras=b,a]` equals `[extras=a,b]`) because they
are a set with no precedence. This is deliberately the opposite of conda's
`channels`, where order *is* solver precedence.

## One environment per tool, dependencies still shared

Each tool gets its own virtual environment, so two CLIs that need incompatible
versions of one library do not fight over a shared one.

Isolation would normally mean paying for a full copy of every shared dependency,
but not under uv: it hard-links unpacked files from its own cache into each
environment, so N environments share one copy of the bytes. Measured on Windows
x64 with two environments that both installed `certifi`:

| Installer | Shared between environments | Per environment |
| --- | --- | --- |
| uv | Yes (both environments plus the cache share one inode) | 762,964 B |
| pip | No, independent copies | 6,812,960 B |

That is 8.9x for identical content. **osdk deliberately does not add a
wheel-level content store of its own**: osdk's store keys unpacked SDK archives
while uv's cache keys unpacked site-packages trees, so the key semantics and
lifetimes differ and a second implementation would yield two incomplete caches
instead of one good one. What osdk contributes is putting uv's cache under
`<cache>/pkg/uv`, so the reuse is osdk-managed rather than scattered.

## The uv and pip paths

With uv installed (`osdk install pypi:uv`) osdk uses it; without it, osdk falls
back to `python -m venv` plus that environment's own pip. A fallback is
**announced**: which path ran, what it costs, and how to get the faster one.
Staying silent would turn a measurable difference in speed and capability into an
unexplained mystery.

The two paths are not equivalent, so the differences are stated plainly:

| | uv | pip fallback |
| --- | --- | --- |
| Shares dependencies across environments | Yes | **No** |
| Resolution speed | Fast | Slow |
| `--relocatable` | Supported | **Unsupported**; errors rather than ignoring it |
| Seeded packages | pip only with `--seed` | Always pip, never setuptools or wheel |

When uv-only behaviour is required, osdk can be told to require it, in which case
a missing uv fails closed instead of downgrading.

::: tip uv is detected by "can it start", not "does the path exist"
On Windows a `uv.ps1`, or a file carrying only a shebang, resolves through a path
lookup yet cannot be started as a process. So osdk actually runs `uv --version`.
When uv is found but unusable, the message says it "could not be started" rather
than "not installed" -- the file is right there, and the latter would send you
looking in the wrong place.
:::

## Indexes and mirrors

Python indexes are configured under `[registries.python]`, managed through osdk's
own configuration, so you do not have to hand-edit `pip.conf` or `uv.toml`:

```toml
[registries.python]
urls = ["https://pypi.tuna.tsinghua.edu.cn/simple/"]
```

osdk ranks the candidates with a fresh anonymous probe and picks the fastest
healthy one. A probe checks more than HTTP 200: it validates the response *shape*
(PEP 503 anchors or a PEP 691 `files` array), so a captive portal answering 200
with a welcome page cannot rank as a healthy mirror.

::: danger A mirror only ever replaces the default index
A mirror is a complete copy of PyPI and therefore carries upstream's package
names -- including malicious ones. Ranking it *above* the default index (uv's
`--index`/`UV_INDEX`, or `--extra-index-url` on either tool) is exactly what turns
a mirror into a dependency-confusion vector. osdk therefore maps a mirror onto the
default index only (`UV_DEFAULT_INDEX` / `PIP_INDEX_URL`), and cannot express
"extra index" at the type level.

Arguments that switch a check off are never forwarded:
`--no-verify-hashes`, `--trusted-host`, `--allow-insecure-host`,
`--extra-index-url`, and `--index`, in both bare and `--flag=value` spellings.
:::

Index URLs must be HTTPS and must not embed credentials. This is stricter than the
npm registry rule, which still accepts http, because the index is where artifact
hashes come from: a downgradeable transport would let an attacker rewrite both the
artifact and the hash meant to detect the rewrite.

The pip path additionally pins `PIP_CONFIG_FILE`. uv ignores `pip.conf` by design
while pip reads it, so a leftover `pip.conf` on your system -- one pointing at an
untrusted index, say -- could otherwise override everything above unnoticed.

## Interpreters

Environments are always built against an **osdk-managed interpreter**, never
whichever `python` happens to be on PATH. The newest installed version is used
unless you name one:

```bash
osdk install "pypi:ruff[python=3.12]@0.6.9"
```

This constraint is deliberate. Building against a PATH interpreter would make the
environment depend on machine state osdk does not control, and on many machines
that `python` is an older system copy while osdk manages a different one. For the
same reason the uv path sets `UV_PYTHON_DOWNLOADS=never`, so uv cannot fetch an
interpreter behind osdk's back.

## Which commands are exposed

osdk exposes only the tool's **own** commands. An environment's `python`, `pip`,
and `activate` scripts are its plumbing and never become shims -- otherwise
`pypi:ruff` would shadow your managed `python`.

Command names are discovered from what is actually on disk rather than assuming
the console script matches the project name, which frequently it does not.
