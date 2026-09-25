# Model Snapshots

osdk manages Hugging Face and ModelScope repositories as immutable, multi-file
snapshots. Model files enter the same BLAKE3 CAS as SDKs, while resolution,
manifests, current-snapshot pointers, and environment adapters remain separate.

## Command reference

```text
osdk model pull NAME [REFERENCE]
  [--endpoint URL]
  [--forward-credentials]
  [--include GLOB]...
  [--exclude GLOB]...
  [--variant LABEL]
  [--no-lock]

osdk model sync [--prune] [--dry-run]
osdk model list
osdk model path NAME [--stable]
osdk model verify NAME
osdk model remove NAME [--keep-lock]

osdk model env enable [huggingface|modelscope] [--force]
osdk model env disable [huggingface|modelscope]
osdk model env list

osdk model view add <comfyui|hf-cache> <name> [--profile P] [--map PREFIX=CATEGORY]...
osdk model view list
osdk model view path <comfyui|hf-cache> [--profile P]
osdk model view rebuild [<comfyui|hf-cache>]
osdk model view remove <comfyui|hf-cache> [--profile P] [--model NAME]
osdk model view export <comfyui|hf-cache> [--profile P] [--to extra_model_paths.yaml]
osdk model view doctor <comfyui|hf-cache> [--profile P]
```

| `pull` argument | Effect |
| --- | --- |
| `NAME` | Local logical name containing only ASCII letters, digits, `.`, `_`, or `-` |
| `REFERENCE` | Optional `PROVIDER:owner/repo@revision`; when omitted, read `[models.NAME].source` |
| `--endpoint URL` | Override the provider endpoint ahead of declaration, environment, and source selection |
| `--forward-credentials` | Allow this explicit custom endpoint to receive the provider token |
| `--include GLOB` | Repeatable; a file must match at least one include when includes are present |
| `--exclude GLOB` | Repeatable; remove matches from the include result |
| `--variant LABEL` | Record an identity/manifest/lock label; it **does not select files** |
| `--no-lock` | Do not update `osdk.lock` at the nearest project location |

`list` shows each logical name's current snapshot; `path` prints its current
directory; `verify` checks all files. `remove` deletes every snapshot for the
logical name and immediately runs CAS GC; it currently does not ask for
confirmation.

`path --stable` prints `<data>/models/<name>/current`, a directory link resolving
to the current snapshot (a junction on Windows, a symlink elsewhere). Snapshot
directory names embed a content hash, so they change whenever `--include`,
`--exclude` or the revision changes. **Use `--stable` for any path that gets
written down somewhere**: a ComfyUI `extra_model_paths.yaml`, a llama.cpp `-m`, a
constant in a script. Without `--stable` you get the real hashed snapshot path,
which is fine for one-off use.

```bash
osdk model path qwen25            # …/snapshots/9f1c2a…
osdk model path qwen25 --stable   # …/qwen25/current  ← still valid after the next pull
```

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

Files download concurrently according to `settings.jobs`. Model files default to
six attempts with 1/2/4/8/8-second backoff and visible retry warnings. Configure
`sources.model_download_attempts` and `sources.model_download_retry_base_ms` with
`osdk config set`. Downloads support Range/ETag resume. An upstream SHA-256 is
enforced when available; otherwise osdk still
computes and records a local SHA-256. `model verify` checks both CAS BLAKE3 and
manifest SHA-256. Snapshots and `current.json` are published through same-directory
temporary paths followed by rename; this does not guarantee fsync durability,
portable replacement atomicity, or transaction isolation across different
snapshot writers.

```text
<data>/models/<name>/
├── current.json
├── current -> snapshots/<snapshot>/   # directory link; printed by `model path --stable`
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

Hugging Face ships a built-in `hf-mirror` mirror (`https://hf-mirror.com`) and
ModelScope two official hosts; all of them take part in probe ranking.
**A built-in mirror never reaches the lock** -- the collapsing rule above
recognises built-in endpoints only. That is precisely why it has to be built in:
the same host added with `osdk source add` is merely `custom`, the rule does not
recognise it, and the mirror's hostname ends up in `[models.<name>].endpoint`,
pushing everyone who replays that lock through your mirror -- including people who
cannot reach it. A built-in mirror never receives the provider token.

```bash
osdk source list hf              # current candidates and their priority
osdk source test hf --model openai-community/gpt2@main   # measure the ranking
osdk source pin hf official      # stay on the official source, no file editing
osdk source unpin hf             # drop the pin and return to auto-selection
```

### Restoring from the lock

`osdk model sync` fetches a whole project's models with no arguments. It compares
each `[models]` declaration applicable to this platform against the lock: a
declaration the lock does not describe is pulled and locked; one whose `source`
(provider / repository / requested revision) or `variant` no longer matches the
lock is re-pulled and its entry rewritten; everything else is left to the replay
of the lock. So a `[models]` entry added or edited by hand is picked up here
without a separate `model pull`. `osdk install` deliberately does not fetch
models: weights are far too large to download as a side effect of installing
tools, so this is its own verb.

`include`/`exclude` are globs, and the lock stores only their expanded file list,
so changes to them do not participate in that comparison -- widening a selection
through `include` is still a `model pull`, the one operation that re-resolves the
remote file list.

```bash
osdk model sync                 # pull [models] entries added/changed vs the lock, replay the rest
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

`sync` also rebuilds consumer views from the `views` declarations recorded in the
lock (see below), so on another machine `osdk model sync` alone is enough; you do
not re-run `model view add`.

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
> [models.<name>].endpoint
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

The probe resolves repository metadata and then samples 64 KiB from a real
file. Anonymous and credential-bearing probes use different cache keys. See
[Sources and Supply-chain Security](./sources-security) for source behavior.

## Declaring models in `osdk.toml`

Instead of pulling first and locking afterwards, you can declare models directly
in the project `osdk.toml`. Declaring does not download anything; a later
`osdk model pull <name>` reads the matching declaration, while `osdk model sync`
pulls every applicable declaration the lock does not yet describe or describes
differently. Both paths record immutable results and consumer views into the lock
and render those views immediately:

```toml
[models.flux]
source   = "hf:black-forest-labs/FLUX.1-dev@main"
include  = ["*.safetensors", "*.json"]
exclude  = ["*.onnx"]
variant  = "fp16"
when     = { os = "windows" }          # optional; same shape as [tools] `when`

[models.flux.views.comfyui]
profile  = "desktop"                   # defaults to "default" when omitted
[models.flux.views.comfyui.map]
"unet/" = "diffusion_models"
"vae/"  = "vae"

[models.embedder.views.hf-cache]
# consumer table without a map: that consumer's default layout
```

The fields mirror the `model pull` flags (`source`/`include`/`exclude`/
`variant`/`when`/`endpoint`), plus `views` (consumer name -> that consumer's
`profile` and `map`). An explicit reference, `--include`, `--exclude`, `--variant`,
or `--endpoint` overrides the corresponding declaration field. A declaration whose
`when` does not match the current platform is ignored by a name-only pull and the
initial sync. A `map` key is a **repo-relative path prefix** (normalized to `/`) and
its value is a consumer category, using exactly the same rules as
`model view add --map`. A misspelled field is an error
(`deny_unknown_fields`), never silently ignored.

**Trust.** Merely declaring *what* to fetch (`source`/`include`/`variant`/
`when`/`views`) needs no trust, exactly like declaring an npm dependency; only
keys that change the **byte source** do -- an `endpoint`, a custom URL, an
`insecure` toggle. Model declarations **never block the shim**: ordinary tool
commands such as `cargo --version` keep working in a project that only declares
models, while the commands that actually fetch (`osdk model sync` / `pull`)
enforce the full check. An entry that carries an `endpoint` is pinned as a whole
(the same granularity as `tools.<name>.allow_builds`), so editing it re-prompts;
editing a different, endpoint-free model does not.

## Consumer views (model view)

Snapshots are laid out like the upstream repository (`unet/`, `vae/`,
`text_encoder/` side by side); consumers expect a different shape. `osdk model
view` renders a pulled snapshot into the consumer's shape with links (hardlinks
on the same volume, counted byte copies across volumes) back to the snapshot --
no weights are copied -- and marks view files read-only so a consumer writing in
place cannot corrupt the snapshot or CAS.

- `add <comfyui|hf-cache> <name>` adds a model and renders it. `comfyui`
  produces `<view>/<category>/<file>` (all 25 category dirs pre-created, files
  classified by the `unet/vae/text_encoder/loras` directory conventions);
  `hf-cache` produces `models--org--repo/{refs,blobs,snapshots}`. The model must
  be pulled first.
- `--map PREFIX=CATEGORY` (repeatable) maps a repo path prefix to a category,
  longest prefix wins. Files that cannot be classified are **never dumped into
  checkpoints**; they are skipped and listed by `view doctor`.
- `path` prints the stable view root; it does not change across pulls, so it is
  the path to write into the consumer's config.
- `export` prints the consumer config fragment. For ComfyUI it is an
  `extra_model_paths.yaml` section with a unique key and **no `is_default`**.
  With `--to` it merges idempotently into a source-edition yaml; without it the
  fragment is printed -- for Desktop, add the printed path once in its Storage
  UI (osdk never writes Desktop's `settings.json`).
- `remove` drops one model's view entries (shared category dirs and other models
  are untouched) or a whole profile; snapshots are not deleted.
- Two models rendering to the same consumer path make `add` fail loudly rather
  than silently overwriting.

```bash
osdk model pull flux hf:org/flux-GGUF --include 'unet/*' --include 'vae/*'
osdk model view add comfyui flux
osdk model view export comfyui --to extra_model_paths.yaml   # source edition
osdk model view path comfyui                                 # Desktop: paste this
```

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
MODEL_ENDPOINT=<chosen HF-compatible endpoint>  # llama.cpp reads this, not HF_ENDPOINT
LLAMA_CACHE=<...>/hub               # llama.cpp's own download-directory variable
```

**Why llama.cpp needs two variables of its own**: its `-hf` downloader reads the
Hugging Face-compatible endpoint from `MODEL_ENDPOINT` (not `HF_ENDPOINT`) and uses
`LLAMA_CACHE` to override the download directory (per upstream `docs/models.md`).
Exporting only the HF names left llama.cpp going straight to huggingface.co,
unaffected by the mirror. Modern llama.cpp stores `-hf` files in the standard HF
cache (`HF_HOME`/`HF_HUB_CACHE` take priority), so osdk points `LLAMA_CACHE` at the
same managed `hub` directory -- old and new llama.cpp then share one copy of a GGUF
instead of downloading it twice.

The ModelScope adapter exports:

```text
MODELSCOPE_ENDPOINT
MODELSCOPE_CACHE=<cache>/pkg/models/modelscope
```

ModelScope has no equivalent universal offline variable, so osdk does not invent
`MODELSCOPE_OFFLINE`. For a managed custom endpoint that cannot receive
credentials, osdk also clears relevant tokens, disables implicit Hugging Face
tokens, and uses an isolated anonymous home to prevent credential leakage.
