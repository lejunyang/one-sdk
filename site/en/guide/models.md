# Model Snapshots

osdk manages Hugging Face and ModelScope repositories plus exact Civitai LoRA versions as immutable
snapshots. Model files enter the same BLAKE3 CAS as SDKs, while resolution,
manifests, current-snapshot pointers, and environment adapters remain separate.

## Command reference

```text
osdk model use NAME REFERENCE [--endpoint URL]
  [--include GLOB]... [--exclude GLOB]... [--variant LABEL]
  [--view <comfyui|hf-cache>] [--profile P] [--map PREFIX=CATEGORY]... [--sync]
osdk model unuse NAME [--keep-snapshot]
osdk model sync [NAME] [--prune] [--dry-run] [--jsonl]
osdk model list [--json]
osdk model show NAME [--json]
osdk model path NAME [--stable] [--json]
osdk model verify NAME [--json]
osdk model remove NAME

osdk model view list [--json]
osdk model view path <comfyui|hf-cache> [--profile P] [--json]
osdk model view doctor <comfyui|hf-cache> [--profile P] [--json]
```

`use` edits the project declaration and does not download by default; `--sync` materializes that
model immediately. `sync NAME` limits work to one model; no argument handles the project.
`unuse` removes declaration, lock, and views and normally deletes the snapshot; `--keep-snapshot`
retains bytes. `remove` deletes only local bytes and views while retaining project intent and lock.

## Machine-readable output

`model list/show/path/verify --json` and `model view list/path/doctor --json` each write one
`schema_version: 1` JSON document to stdout. Model documents include provider, repository,
requested and immutable revisions, endpoint, variant, file paths/sizes/digests, creation time, and
snapshot/stable paths plus `stable_path_available` without creating a missing link. View documents include consumer, profile, root, model mappings, and, for
`doctor`, placed/unclassified files plus cross-volume copy counts. Absolute paths retain native
separators; manifest-relative paths remain `/`-normalized.

`model sync --jsonl` writes one independent `schema_version: 1` event per line. Every event has
`event`, `status`, and `dry_run`; events add `model`, `action`, `revision`, `path`, `reason`, and
`changed` when applicable. `--dry-run --jsonl` remains machine-only as well.

In machine mode stdout contains only JSON/JSONL. Warnings and errors go to stderr and failures keep
a non-zero exit code. The CLI protocol schema is independent of the on-disk schemas for
`.osdk-model.json`, `osdk.lock`, and `.osdk-views.json`.

## Provider references

```text
hf:owner/repo@revision
huggingface:owner/repo@revision
hugging-face:owner/repo@revision

ms:owner/repo@revision
modelscope:owner/repo@revision
model-scope:owner/repo@revision

civitai:model-id@model-version-id
civi:model-id@model-version-id
```

Without a revision, Hugging Face defaults to `main` and ModelScope to `master`.
Their repositories must be exactly two `owner/name` segments, each using ASCII letters,
digits, `.`, `_`, or `-`. Civitai requires both positive model and model-version IDs;
osdk does not search for or guess a version.

```bash
osdk model use qwen25 hf:Qwen/Qwen2.5-7B-Instruct@main
osdk model use qwen25-ms ms:Qwen/Qwen2.5-7B-Instruct@master
osdk model use character-lora civitai:456@123 --view comfyui --sync
osdk model use qwen25 hf:Qwen/Qwen2.5-7B-Instruct@main \
  --include '*.json' --include '*.safetensors' \
  --exclude 'original/*' --variant safetensors-fp16 --sync
```

Hugging Face resolves a branch or tag to an immutable commit SHA. When
ModelScope's file API has no equivalent commit, osdk derives a
`revision+manifest-<16 hex>` identity from the requested revision and sorted
file paths, sizes, and SHA-256 values. Civitai uses the exact model-version ID as the
immutable revision, selects one `Model` weight by SafeTensor, primary, then response
order, requires SHA-256, and normalizes it to `loras/<filename>` for direct ComfyUI view
rendering. Remote paths must be safe relative paths.

## Downloads, verification, and local layout

Within one model, files download concurrently according to `settings.jobs`; when
`osdk model sync` fetches several models it downloads distinct models concurrently
up to `sources.model_jobs` (default 2, overridable with `--model-jobs`). The two
are independent and multiply, so the default is kept small to avoid exhausting
connections or tripping source rate limits; lock writes stay serial. Model files
default to six attempts with 1/2/4/8/8-second backoff and visible retry warnings.
Configure `sources.model_download_attempts` and `sources.model_download_retry_base_ms`
with `osdk config set`. Downloads support Range/ETag resume. If a connection stalls
mid-transfer (no bytes for a while), `sources.model_read_timeout_ms` (default 60000)
fails that request so the retry and resume above take over instead of hanging forever;
it bounds only the no-progress interval, not total download time, so a large file that
keeps progressing is unaffected. An upstream SHA-256 is
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

`model sync` writes `[models.<name>]` at the top level of `osdk.lock` with:

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

`osdk model sync` handles the whole project without an argument and only one logical model when
`NAME` is supplied. It compares applicable `[models]` declarations with the lock: new declarations
are downloaded and locked; changes to `source`, `variant`, `include`, or `exclude` are resolved again;
everything else is replayed from the immutable lock.

```bash
osdk model sync qwen25          # one model
osdk model sync                 # whole project
osdk model sync --dry-run       # report without applying
osdk model sync --prune         # remove snapshots no longer declared by the lock
```

Restore uses the lock's immutable revision and file SHA-256 values rather than reinterpreting a
floating branch. The lock stores the original selectors as well as the expanded file list, so a
selector change triggers resolution. An intact snapshot is not downloaded again; a corrupt one is.
Consumer views are rebuilt from the lock as part of synchronization.

`--prune` applies only to whole-project synchronization and is off by default because it deletes
weights that may be expensive to fetch again.

### `remove` and `unuse`

`osdk model remove <name>` removes only the local snapshot and consumer views while retaining the
project declaration and lock; `model sync <name>` restores it. Use `model unuse <name>` to remove
project intent, the lock entry, and views as well. It normally deletes the snapshot;
`--keep-snapshot` retains the local bytes.
## Endpoints and credentials

Resolution priority is:

```text
--endpoint
> [models.<name>].endpoint
> HF_ENDPOINT / MODELSCOPE_ENDPOINT / MODELSCOPE_DOMAIN / CIVITAI_ENDPOINT
> source pin, probe ranking, and built-in endpoint
```

| Provider | Token variables, highest priority first |
| --- | --- |
| Hugging Face | `OSDK_HF_TOKEN`, `HF_TOKEN`, `HUGGING_FACE_HUB_TOKEN` |
| ModelScope | `OSDK_MODELSCOPE_TOKEN`, `MODELSCOPE_API_TOKEN` |
| Civitai | `OSDK_CIVITAI_TOKEN`, `CIVITAI_API_TOKEN`, `CIVITAI_TOKEN` |

Official `https://huggingface.co`, `https://modelscope.cn`, `https://www.modelscope.ai`, and `https://civitai.com` endpoints may receive their provider credentials. Bearer credentials are removed when a Civitai download redirects to a cross-origin CDN. A
custom source or `--endpoint` is anonymous unless `--forward-credentials` or the
source's `forward_credentials = true` allows forwarding. ModelScope uses both a
Bearer header and `m_session_id` cookie.

Model providers use the source command surface, but testing requires a repository:

```text
osdk source list huggingface|modelscope|civitai
osdk source test huggingface|modelscope|civitai --model owner/repo[@revision] (Civitai: model-id@version-id)
osdk source add huggingface|modelscope|civitai --id ID --download-url URL
  [--index-url URL] [--forward-credentials]
osdk source remove huggingface|modelscope|civitai ID
osdk source pin huggingface|modelscope|civitai ID
osdk source unpin huggingface|modelscope|civitai
```

The probe resolves repository metadata and then samples 64 KiB from a real
file. Anonymous and credential-bearing probes use different cache keys. See
[Sources and Supply-chain Security](./sources-security) for source behavior.

## Declaring models in `osdk.toml`

`osdk model use` edits the project `osdk.toml` through osdk. A declaration does not download bytes;
add `--sync` to materialize that model immediately, or later run `osdk model sync [name]` for one or
all declarations:

```toml
[models.flux]
source   = "hf:black-forest-labs/FLUX.1-dev@main"
include  = ["*.safetensors", "*.json"]
exclude  = ["*.onnx"]
variant  = "fp16"
when     = { os = "windows" }

[models.flux.views.comfyui]
profile  = "desktop"
[models.flux.views.comfyui.map]
"unet/" = "diffusion_models"
"vae/"  = "vae"
```

`model use` writes `source`, `include`, `exclude`, `variant`, `endpoint`, and one consumer view's
`profile`/`map`; running it again replaces the same-name declaration. `when` remains a direct-config
field. Unknown fields fail loudly rather than being ignored.

**Trust.** Ordinary declarations require no trust; only fields that change the byte source, such as
`endpoint` or a custom URL, require review. Model declarations never block shims. Networked
`model sync` and `model use --sync` enforce the full trust check, while `model unuse` remains the
escape hatch for removing an untrusted declaration.

## Consumer views (model view)

Snapshots are laid out like the upstream repository (`unet/`, `vae/`,
`text_encoder/` side by side); consumers expect a different shape. `osdk model
view` renders a materialized snapshot into the consumer's shape with links (hardlinks
on the same volume, counted byte copies across volumes) back to the snapshot --
no weights are copied -- and marks view files read-only so a consumer writing in
place cannot corrupt the snapshot or CAS.

- `add <comfyui|hf-cache> <name>` adds a model and renders it. `comfyui`
  produces `<view>/<category>/<file>` (all 25 category dirs pre-created, files
  classified by the `unet/vae/text_encoder/loras` directory conventions);
  `hf-cache` produces `models--org--repo/{refs,blobs,snapshots}`. The model must
  be materialized first.
- `--map PREFIX=CATEGORY` (repeatable) maps a repo path prefix to a category,
  longest prefix wins. Files that cannot be classified are **never dumped into
  checkpoints**; they are skipped and listed by `view doctor`.
- `path` prints the stable view root; it does not change across synchronizations, so it is
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
osdk model use flux hf:org/flux-GGUF --include 'unet/*' --include 'vae/*' --sync
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

`enable`/`disable` manage only the native Hugging Face and ModelScope environment adapters. Civitai has no downstream environment protocol, so passing it explicitly fails. `--force` belongs only to
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
