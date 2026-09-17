# Model Snapshots

osdk manages Hugging Face and ModelScope repositories as immutable, multi-file
snapshots. Model files enter the same BLAKE3 CAS as SDKs, while resolution,
manifests, current-snapshot pointers, and environment adapters remain separate.

## Command reference

```text
osdk model pull NAME REFERENCE
  [--endpoint URL]
  [--forward-credentials]
  [--include GLOB]...
  [--exclude GLOB]...
  [--variant LABEL]
  [--no-lock]

osdk model sync [--prune] [--dry-run]
osdk model list
osdk model path NAME
osdk model verify NAME
osdk model remove NAME [--keep-lock]

osdk model env enable [huggingface|modelscope] [--force]
osdk model env disable [huggingface|modelscope]
osdk model env list
```

| `pull` argument | Effect |
| --- | --- |
| `NAME` | Local logical name containing only ASCII letters, digits, `.`, `_`, or `-` |
| `REFERENCE` | `PROVIDER:owner/repo@revision` |
| `--endpoint URL` | Override the provider endpoint ahead of environment and source selection |
| `--forward-credentials` | Allow this explicit custom endpoint to receive the provider token |
| `--include GLOB` | Repeatable; a file must match at least one include when includes are present |
| `--exclude GLOB` | Repeatable; remove matches from the include result |
| `--variant LABEL` | Record an identity/manifest/lock label; it **does not select files** |
| `--no-lock` | Do not update `osdk.lock` at the nearest project location |

The other model commands have no optional arguments. `list` shows each logical
name's current snapshot; `path` prints its current directory; `verify` checks all
files. `remove` deletes every snapshot for the logical name and immediately runs
CAS GC; it currently does not ask for confirmation.

## Provider references

```text
hf:owner/repo@revision
huggingface:owner/repo@revision
hugging-face:owner/repo@revision

ms:owner/repo@revision
modelscope:owner/repo@revision
model-scope:owner/repo@revision
```

Without a revision, Hugging Face defaults to `main` and ModelScope to `master`.
A repository must be exactly two `owner/name` segments, each using ASCII letters,
digits, `.`, `_`, or `-`.

```bash
osdk model pull qwen25 hf:Qwen/Qwen2.5-7B-Instruct@main
osdk model pull qwen25-ms ms:Qwen/Qwen2.5-7B-Instruct@master
osdk model pull qwen25 hf:Qwen/Qwen2.5-7B-Instruct@main \
  --include '*.json' --include '*.safetensors' \
  --exclude 'original/*' --variant safetensors-fp16
```

Hugging Face resolves a branch or tag to an immutable commit SHA. When
ModelScope's file API has no equivalent commit, osdk derives a
`revision+manifest-<16 hex>` identity from the requested revision and sorted
file paths, sizes, and SHA-256 values. Remote paths must be safe relative paths.

## Downloads, verification, and local layout

Files download concurrently according to `settings.jobs` and support Range/ETag
resume. An upstream SHA-256 is enforced when available; otherwise osdk still
computes and records a local SHA-256. `model verify` checks both CAS BLAKE3 and
manifest SHA-256. Snapshots and `current.json` are published through same-directory
temporary paths followed by rename; this does not guarantee fsync durability,
portable replacement atomicity, or transaction isolation across different
snapshot writers.

```text
<data>/models/<name>/
├── current.json
├── .locks/<snapshot>.lock
└── snapshots/<snapshot>/
    ├── .osdk-model.json
    ├── .osdk-manifest.json
    ├── .osdk-complete
    └── downloaded files...
```

With `--offline`, metadata and every selected file must already be cached. That
is sufficient to rematerialize a deleted snapshot. Automatic source failover
stays within one provider and never converts a Hugging Face repository into a
ModelScope repository.

## Lockfile

By default, `pull` writes `[models.<name>]` at the top level of `osdk.lock` with:

- provider, repository, requested revision, and immutable revision;
- effective endpoint and optional variant label;
- each file's path, size, and SHA-256.

Tokens, cookies, temporary signed download URLs, and ETags are not written. A
model update merges only the same-name model record and preserves platform tool
sections. See [Reproducible Lockfiles](./lockfiles) for the full schema.

`endpoint` records the provider's official endpoint: a model is identified by
provider, repository and immutable revision, and every file's SHA-256 is locked,
so the host is not part of the identity. A mirror endpoint therefore collapses to
the official one, while a custom endpoint is left unchanged.

### Restoring from the lock

`osdk model sync` is the reader the `[models]` section never had -- `pull` writes,
`sync` replays, the same relationship tools have between `lock` and `install`.
`osdk install` deliberately does not fetch models: weights are far too large to
download as a side effect of installing tools, so this is its own verb.

```bash
osdk model sync                 # restore every model the lock declares
osdk model sync --dry-run       # report what would happen
osdk model sync --prune         # also delete snapshots the lock no longer declares
osdk model sync --prune --dry-run
```

A restore rebuilds the reference from the lock's **immutable revision**, not from
`requested_revision`: replaying a branch name would resolve to wherever it points
now, which is the opposite of what a lock is for. Only the files the lock names
are fetched, so a repository that gained files after locking cannot silently grow
the snapshot, and every file's size and SHA-256 is compared afterwards -- a
mismatch fails, because pinning content is the point.

A snapshot that is already present and verifies is not re-downloaded: the lock
carries each file's digest, so "is this the thing the lock describes" is
answerable locally, and re-fetching gigabytes to answer it would be absurd. A
snapshot that fails verification is re-pulled, since at that point the local copy
is not what was committed.

`--prune` is off by default: it deletes materialized weights, which are expensive
to re-fetch, so it must be asked for rather than happening as a side effect.

### `remove` keeps the lock in step

`osdk model remove <name>` drops the lock entry along with the local snapshot.
Previously only the snapshot went, leaving the lock still claiming it, so the next
`sync` would faithfully pull back exactly what had just been removed.

Use `--keep-lock` to remove locally without changing what the project declares; a
later `sync` restores it.

## Endpoints and credentials

Resolution priority is:

```text
--endpoint
> HF_ENDPOINT / MODELSCOPE_ENDPOINT / MODELSCOPE_DOMAIN
> source pin, probe ranking, and built-in endpoint
```

| Provider | Token variables, highest priority first |
| --- | --- |
| Hugging Face | `OSDK_HF_TOKEN`, `HF_TOKEN`, `HUGGING_FACE_HUB_TOKEN` |
| ModelScope | `OSDK_MODELSCOPE_TOKEN`, `MODELSCOPE_API_TOKEN` |

Official `https://huggingface.co`, `https://modelscope.cn`, and
`https://www.modelscope.ai` endpoints may receive their provider credentials. A
custom source or `--endpoint` is anonymous unless `--forward-credentials` or the
source's `forward_credentials = true` allows forwarding. ModelScope uses both a
Bearer header and `m_session_id` cookie.

Model providers use the source command surface, but testing requires a repository:

```text
osdk source list huggingface|modelscope
osdk source test huggingface|modelscope --model owner/repo[@revision]
osdk source add huggingface|modelscope --id ID --download-url URL
  [--index-url URL] [--forward-credentials]
osdk source remove huggingface|modelscope ID
osdk source pin huggingface|modelscope ID
osdk source unpin huggingface|modelscope
```

The probe resolves repository metadata and then samples up to 1 MiB from a real
file. Anonymous and credential-bearing probes use different cache keys. See
[Sources and Supply-chain Security](./sources-security) for source behavior.

## Global model environment

Install activation in the shell first, then enable provider adapters:

```bash
eval "$(osdk activate bash)"
osdk model env enable                       # both providers
osdk model env enable huggingface
osdk model env enable modelscope --force
osdk model env list
osdk model env disable huggingface
osdk model env disable                      # both providers
```

`enable`/`disable` accept only the optional provider. `--force` belongs only to
`enable` and permits overriding pre-existing provider variables. State is saved
in user configuration; a project cannot change `env` or `env_force`. An active
shell applies changes at the next prompt, while new activation applies them
immediately. `deactivate` restores captured originals.

The Hugging Face adapter exports:

```text
HF_ENDPOINT
HF_HOME=<cache>/pkg/models/huggingface
HF_HUB_CACHE=<...>/hub
HF_XET_CACHE=<...>/xet
HF_ASSETS_CACHE=<...>/assets
HF_HUB_OFFLINE=1                    # only when osdk is offline
```

The ModelScope adapter exports:

```text
MODELSCOPE_ENDPOINT
MODELSCOPE_CACHE=<cache>/pkg/models/modelscope
```

ModelScope has no equivalent universal offline variable, so osdk does not invent
`MODELSCOPE_OFFLINE`. For a managed custom endpoint that cannot receive
credentials, osdk also clears relevant tokens, disables implicit Hugging Face
tokens, and uses an isolated anonymous home to prevent credential leakage.
