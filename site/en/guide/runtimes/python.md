# Python

Configuration reference: [applicable tool_order sources](../config/tool-selection#python), [project source diagnostics](../config/tool-selection#inspect-sources), and [this tool’s selectors, managed variables and cache overrides](../config/tool-environment#python).

`python@3.14` is shorthand for default CPython. The complete request form is
`python@IMPLEMENTATION-VERSION+VARIANT`:

```bash
osdk install python@3.14
osdk install python@cpython-3.14+freethreaded
osdk install python@cpython-3.14+debug
osdk install python@cpython-3.14+freethreaded+debug
osdk install python@pypy-3.11
osdk install python@graalpy-3.12
osdk install python@pyodide-3.14
```

| Implementation | Supported variants | Identity examples |
| --- | --- | --- |
| `cpython` | `default`, `freethreaded`, `debug`, `freethreaded+debug` | `3.14.7`, `cpython-3.14.7+debug` |
| `pypy` | `default` | `pypy-3.11.x` |
| `graalpy` | `default` | `graalpy-3.12.x` |
| `pyodide` | `default` | `pyodide-3.14.x` |

| Option | Value | Effect |
| --- | --- | --- |
| `variant` | A variant above | Equivalent to request suffix `+VARIANT`; the explicit option wins |
| `tag` | A python-build-standalone date such as `20240224` | Select a historical PBS release for classic CPython |

Implementation, exact Python version, variant, and catalog artifact are locked,
and distinct identities can coexist. Standard CPython uses the embedded
python-build-standalone index; multiple implementations, variants, and
pre-releases use an embedded verified catalog. A custom catalog needs its exact
SHA-256:

```toml
[settings.python]
catalog_url = "/approved/python-catalog.json"
catalog_sha256 = "0123456789abcdef..."
```

`catalog_url` accepts HTTP(S) or a local path. Only a valid digest, schema, and
checksum on every artifact can replace last-good. Refresh failure tries
last-good and then the embedded catalog. See [Pre-releases](../sources/offline#pre-releases).

For classic CPython, osdk merges `SHA256SUMS` from every reachable source for the
same release tag, preserving the higher-ranked entry for duplicate filenames.
A first mirror missing one platform artifact can therefore be completed by a
later source and enter normal verified download failover.

Interpreter discovery uses:

```text
osdk python find [REQUEST]
```

```bash
osdk python find
osdk python find pypy-3.11
osdk python find 3.14+freethreaded
```

Managed results are filtered by the optional request. osdk then scans `PATH` and
system candidates, deduplicates them, and labels each as `managed`, `PATH`, or
`system`. It returns an error if nothing is found.
