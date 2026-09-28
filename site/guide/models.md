# 模型快照

osdk 把 Hugging Face、ModelScope 仓库和精确 Civitai LoRA 版本作为不可变快照管理。模型文件
进入与 SDK 共用的 BLAKE3 CAS，但解析、manifest、当前快照和环境适配均是独立流程。

## 命令参考

```text
osdk model use NAME REFERENCE [--endpoint URL]
  [--include GLOB]... [--exclude GLOB]... [--variant LABEL]
  [--kind KIND] [--family FAMILY] [--derived-from REFERENCE]
  [--view <comfyui|hf-cache>] [--profile P] [--map PREFIX=CATEGORY]... [--sync]
osdk model import NAME PATH [--target-path PATH] [--variant LABEL]
  [--kind KIND] [--family FAMILY] [--derived-from REFERENCE]
  [--view comfyui] [--profile P] [--map PREFIX=CATEGORY]... [--json]
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

`use` 受管写入项目声明，默认不下载；`--sync` 立即物化该模型。`sync NAME` 只处理一个
模型，无参数时处理整个项目。`unuse` 撤销声明、lock 和视图并默认删除本地快照；
`--keep-snapshot` 保留本地字节。`remove` 只删除本地快照和视图，保留项目声明与 lock。

## 机器可读输出

`model import --json`、`model list/show/path/verify --json` 与 `model view list/path/doctor --json` 各自在 stdout
输出一个 `schema_version: 1` JSON 文档。模型文档包含 provider、repository、请求/不可变
revision、endpoint、variant、文件路径/大小/摘要、创建时间以及当前快照和稳定路径；view
文档还用 `stable_path_available` 报告稳定路径是否已可用，但不会为查询创建缺失链接。view 文档包含 consumer、profile、根路径、模型与映射，doctor 还包含 placed、unclassified 和
跨卷 copy 计数。绝对路径保留当前平台的原生分隔符，manifest 中的相对路径保持 `/`。

`model sync --jsonl` 每行输出一个独立的 `schema_version: 1` 事件。固定字段为
`event`、`status` 与 `dry_run`；事件按需增加 `model`、`action`、`revision`、`path`、
`reason`、`changed`。`--dry-run --jsonl` 同样只输出事件，不混入人类文本。

机器模式中 stdout 只承载 JSON/JSONL；警告和错误写 stderr，失败保持非零退出码。CLI
协议 schema 与 `.osdk-model.json`、`osdk.lock`、`.osdk-views.json` 的磁盘 schema 相互独立。

## Provider 引用

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

省略 revision 时，Hugging Face 默认 `main`，ModelScope 默认 `master`。二者的 repository
必须正好是 `owner/name` 两段，每段只允许 ASCII 字母、数字、`.`、`_`、`-`。Civitai
必须同时给出正整数 model ID 与 model version ID；OSDK 不负责搜索或猜选版本。

```bash
osdk model use qwen25 hf:Qwen/Qwen2.5-7B-Instruct@main
osdk model use qwen25-ms ms:Qwen/Qwen2.5-7B-Instruct@master
osdk model use character-lora civitai:456@123 --view comfyui --sync
osdk model use qwen25 hf:Qwen/Qwen2.5-7B-Instruct@main \
  --include '*.json' --include '*.safetensors' \
  --exclude 'original/*' --variant safetensors-fp16 --sync
```

Hugging Face 将 branch/tag 解析为不可变 commit SHA。ModelScope 文件 API 没有等价
commit 时，osdk 以请求 revision 和排序后的文件路径、大小、SHA-256 manifest 生成
`revision+manifest-<16 hex>` identity。Civitai 直接以 model version ID 作为不可变 revision，
从该版本的 `Model` 文件中按 SafeTensor、primary、响应顺序选择一个权重，要求合法 SHA-256，
并规范到快照内 `loras/<filename>`，因此 `--view comfyui` 可直接渲染；其有效 `kind` 默认是 `lora`。远端文件路径必须是安全相对路径。

## 语义元数据与血缘

`kind` 是稳定枚举：`checkpoint`、`lora`、`vae`、`text-encoder`、`diffusion-model`、`controlnet`、`upscaler`、`embedding`、`other`。`family` 记录架构/生态家族（如 `sdxl`、`flux`），`derived_from` 记录调用方确认的基础模型或上游引用。OSDK 不猜测后两者，也不解析 trigger word。

三字段进入快照身份、`.osdk-model.json`、`osdk.lock` 和 `--json` 输出；任一变化会产生新快照并触发 re-lock。值必须非空、去除首尾空白、无控制字符；`family` 最多 256 字节，`derived_from` 最多 2048 字节。

## 导入本地模型

`osdk model import NAME PATH` 接受单个文件或目录，把每个文件计算 SHA-256 后复制进 CAS 并发布不可变快照。目录保持原有相对布局；单文件可用 `--target-path` 指定快照内路径，或按 `--kind` 自动进入 `checkpoints/`、`loras/`、`vae/`、`text_encoders/`、`diffusion_models/`、`controlnet/`、`upscale_models/`、`embeddings/`。内容 revision 由排序后的相对路径、大小与 SHA-256 计算，修改源文件后二次导入会产生新快照，并自动刷新该逻辑名已有的 ComfyUI view。

```bash
osdk model import local-style C:\models\style.safetensors \
  --kind lora --family sdxl --derived-from hf:org/base@main \
  --view comfyui --json
osdk model import local-bundle C:\models\bundle --variant fp16
```

本地导入显示为 `provider: "local"`，但 `local:` 不是可用于 `model use` 的在线引用。OSDK 不把原始绝对路径写入 manifest，也不写 `osdk.toml` 或 `osdk.lock`：任意本地路径无法在另一台机器上可靠恢复，所以缺失后必须从原始字节重新导入。为避免状态冲突，同名项目声明或 lock 存在时导入会失败；本地导入也不支持 `hf-cache` view，因为它没有可诚实声明的 Hugging Face 仓库身份。

目录遍历不跟随链接，并拒绝 symlink、Windows junction/reparse point、特殊文件、非 UTF-8 或跨平台不安全的相对路径。`--json` 成功时 stdout 只输出现有 schema 1 模型文档，错误仍写 stderr 并返回非零。

## 下载、校验与本地布局

单个模型内部，文件按 `settings.jobs` 并发下载；`osdk model sync` 拉取多个模型时，
按 `sources.model_jobs`（默认 2）并发下载不同模型，可用 `--model-jobs` 覆盖。两者
相互独立且相乘，因此默认取较小值以免打爆连接数或触发来源限流；lock 写入始终串行。
模型文件默认尝试 6 次，按 1/2/4/8/8 秒退避并输出可见重试警告；使用 `osdk config set`
调整 `sources.model_download_attempts` 与 `sources.model_download_retry_base_ms`。下载
支持 Range/ETag 续传。若连接中途断流（长时间收不到字节），`sources.model_read_timeout_ms`
（默认 60000）会让该次请求超时失败，进而触发上述重试与续传，而不是永久挂起；它只约束
「无进展」时长，不限制总下载时间，大文件只要持续传输就不受影响。上游提供 SHA-256
时强制校验；未提供时仍计算并记录本地 SHA-256。`model verify` 同时检查 CAS BLAKE3 与
manifest SHA-256。快照和 `current.json` 都通过同目录临时路径再 rename 发布；这不
保证 fsync 持久性、跨平台替换原子性或不同 snapshot writer 之间的事务隔离。

```text
<data>/models/<name>/
├── current.json
├── current -> snapshots/<snapshot>/   # 目录链接；`model path --stable` 输出它
├── .locks/<snapshot>.lock
└── snapshots/<snapshot>/
    ├── .osdk-model.json
    ├── .osdk-manifest.json
    ├── .osdk-complete
    └── downloaded files...
```

`--offline` 下，metadata 与所有所选文件都必须已经缓存；满足时可重建已删除的物化
快照。自动 source failover 只发生在同一 provider 内，不会把 Hugging Face 仓库
隐式转换为 ModelScope 仓库。

## Lockfile

`model sync` 在 `osdk.lock` 顶层 `[models.<name>]` 记录：

- provider、repository、请求 revision 与不可变 revision；
- 实际 endpoint、可选 variant 与 `kind/family/derived_from`；
- 每个文件的路径、大小与 SHA-256。

token、cookie、临时签名下载 URL 和 ETag 不写入项目 lock。模型更新只合并同名
模型项并保留平台工具区段。完整 schema 见[可复现锁文件](./lockfiles)。

`endpoint` 记录 provider 的官方端点：模型的身份是 provider + repository + 不可变
revision，且每个文件的 SHA-256 都已入锁，主机不属于身份的一部分。因此镜像端点会被
折叠成官方端点，自定义端点原样保留。

Hugging Face 内置 `hf-mirror` 镜像（`https://hf-mirror.com`），ModelScope 内置两个
官方域名，都参与探测排序。**内置镜像不会进 lock**——上面那条折叠规则只认内置端点。
这也是它必须内置的原因：同一个域名用 `osdk source add` 加进来只是 `custom`，折叠规则
不认，于是镜像域名会被写进 `[models.<name>].endpoint`，其他人复现这份 lock 时都会被
推去走你的镜像，包括根本访问不到它的人。内置镜像不接收 provider token。

```bash
osdk source list hf              # 看当前候选与优先级
osdk source test hf --model openai-community/gpt2@main   # 实测排名
osdk source pin hf official      # 只走官方源，不必手改任何文件
osdk source unpin hf             # 取消固定，恢复自动选择
```

### 从 lock 还原

`osdk model sync` 无参数时处理整个项目，传入 `NAME` 时只处理一个逻辑模型。它比对当前
平台适用的 `[models]` 声明与 lock：新增声明会被下载并写入 lock；`source`、`variant`、
`kind`、`family`、`derived_from`、`include` 或 `exclude` 变化会重新解析并改写条目；其余按不可变 lock 复现。

```bash
osdk model sync qwen25          # 只同步一个模型
osdk model sync                 # 同步整个项目
osdk model sync --dry-run       # 只报告会做什么
osdk model sync --prune         # 删除 lock 不再声明的本地快照
```

还原时按 lock 中的不可变 revision 和文件 SHA-256 重建，而不是重新解释浮动分支。lock 同时
保存原始 `include`/`exclude` 与展开后的文件列表，因此选择器变化能触发重新解析。已存在且
校验通过的快照不会重复下载；校验失败时重新获取。消费者视图也会按 lock 自动重建。

`--prune` 仅适用于全项目同步，且默认关闭，因为它会删除重新获取代价较高的本地权重。

### `remove` 与 `unuse`

`osdk model remove <name>` 只删除本地快照和消费者视图，保留项目声明与 lock；之后
`model sync <name>` 可恢复它。要彻底撤销项目依赖，使用 `model unuse <name>`：它会移除
项目声明、lock、视图并默认删除快照，`--keep-snapshot` 可保留本机字节。
## Endpoint 与凭据

解析优先级：

```text
--endpoint
> [models.<name>].endpoint
> HF_ENDPOINT / MODELSCOPE_ENDPOINT / MODELSCOPE_DOMAIN / CIVITAI_ENDPOINT
> source pin、测速排名和内置 endpoint
```

Token 读取顺序：

| Provider | 环境变量，左侧优先 |
| --- | --- |
| Hugging Face | `OSDK_HF_TOKEN`、`HF_TOKEN`、`HUGGING_FACE_HUB_TOKEN` |
| ModelScope | `OSDK_MODELSCOPE_TOKEN`、`MODELSCOPE_API_TOKEN` |
| Civitai | `OSDK_CIVITAI_TOKEN`、`CIVITAI_API_TOKEN`、`CIVITAI_TOKEN` |

官方端点 `https://huggingface.co`、`https://modelscope.cn`、`https://www.modelscope.ai`，以及 Civitai 的两个官方内容入口 `https://civitai.com`（`official`）和 `https://civitai.red`（`official-red`）可接收对应凭据。Civitai auto 模式会针对精确版本探测两个入口并择优/回退；两者统一归一为 Civitai provider 的 `.com` lock 身份。API 返回的下载 URL 若属于 `.com` 或 `.red`，初始下载请求可携带 Bearer；跳转到其他 origin 时移除。自定义 source 或 `--endpoint` 默认匿名，
只有 `--forward-credentials` 或 source 的 `forward_credentials = true` 才转发。
ModelScope 会同时使用 Bearer header 与 `m_session_id` cookie。

模型 source 命令与 SDK 相同，但测试时必须指定仓库：

```text
osdk source list huggingface|modelscope|civitai
osdk source test huggingface|modelscope|civitai --model owner/repo[@revision]（Civitai 为 model-id@version-id）
osdk source add huggingface|modelscope|civitai --id ID --download-url URL
  [--index-url URL] [--forward-credentials]
osdk source remove huggingface|modelscope|civitai ID
osdk source pin huggingface|modelscope|civitai ID
osdk source unpin huggingface|modelscope|civitai
```

探测会先解析目标仓库 metadata，再对一个真实文件做 64 KiB 的 Range 下载；
匿名与带凭据模式使用不同缓存键。更多 source 规则见[下载源与供应链安全](./sources-security)。

## 在 `osdk.toml` 中声明模型

`osdk model use` 会受管写入项目 `osdk.toml`，声明本身不下载权重。加 `--sync` 会立即只
物化该模型，也可以随后用 `osdk model sync [name]` 处理单个或全部声明：

```toml
[models.flux]
source   = "hf:black-forest-labs/FLUX.1-dev@main"
include  = ["*.safetensors", "*.json"]
exclude  = ["*.onnx"]
variant  = "fp16"
kind     = "diffusion-model"
family   = "flux"
derived_from = "hf:black-forest-labs/FLUX.1-dev@main"
when     = { os = "windows" }

[models.flux.views.comfyui]
profile  = "desktop"
[models.flux.views.comfyui.map]
"unet/" = "diffusion_models"
"vae/"  = "vae"
```

`model use` 可写入 `source`、`include`、`exclude`、`variant`、`kind`、`family`、`derived_from`、`endpoint` 以及一个 consumer
view 的 `profile`/`map`；再次执行会替换同名声明。`when` 仍是直接配置字段。写错字段名会
直接报错，不会被静默忽略。

**信任（trust）语义**：模型配置在任何命令里都不要求信任，包括写了 `endpoint`
的条目。模型字节是内容，osdk 从不执行它们，下载仍按锁定摘要校验。
## 消费者视图（model view）

快照按上游仓库布局存放（`unet/`、`vae/`、`text_encoder/` 平级），消费者要的是
另一种形状。`osdk model view` 把已物化的快照**渲染成消费者形状的目录**，文件以
链接（同卷硬链接，跨卷退化为拷贝并明确计数）指回快照，不复制权重；视图文件设为
只读，避免消费者就地写入污染快照与 CAS。

- `add <comfyui|hf-cache> <name>`：把模型加入视图并渲染。`comfyui` 渲染成
  `<view>/<类别>/<文件>`（25 个类别目录预先建好，按 `unet/vae/text_encoder/loras`
  等目录约定归类）；`hf-cache` 渲染成 `models--org--repo/{refs,blobs,snapshots}`。
  模型必须先 `model sync`，否则报错并提示先 sync。
- `--map PREFIX=CATEGORY`（可重复）：显式指定仓库路径前缀到类别的映射，最长前缀
  优先。**无法归类的文件不会被兜底塞进 checkpoints**，而是跳过并由 `view doctor`
  列出。
- `path`：打印稳定的视图根，路径不随 sync/`--include` 变化——把它写进消费者配置。
- `export`：生成消费者配置片段。`comfyui` 是一段 `extra_model_paths.yaml`（唯一键、
  `base_path` 指向视图根，**不带 `is_default`**，避免悄悄改变消费者自己的模型根
  优先级）。带 `--to` 会以带标记的托管块幂等合并进源码版 ComfyUI 的 yaml；不带则
  只打印——Desktop 版请按打印的路径在 Storage 面板添加一次，osdk 不写 Desktop 的
  `settings.json`。
- `remove`：移除某模型在视图里的条目（只拆该模型的链接，共享类别目录里其它模型
  不受影响）或整个 profile；不删快照。
- 两个模型若渲染到同一消费者路径（同名文件且同类），`add` 会**报错拒绝**而不是
  静默后者覆盖前者。

```bash
osdk model use flux hf:org/flux-GGUF --include 'unet/*' --include 'vae/*' --sync
osdk model view add comfyui flux
osdk model view export comfyui --to extra_model_paths.yaml   # 源码版
osdk model view path comfyui                                 # Desktop：贴这个路径
```

## 全局模型环境

先把 activation 放入 shell 初始化文件，再启用 provider adapter：

```bash
eval "$(osdk activate bash)"
osdk model env enable                       # 两个 provider
osdk model env enable huggingface
osdk model env enable modelscope --force
osdk model env list
osdk model env disable huggingface
osdk model env disable                      # 两个 provider
```

`enable`/`disable` 只管理 Hugging Face 与 ModelScope 的原生环境适配器；Civitai 没有对应下游环境协议，显式传入会报错。`--force` 只属于 `enable`，表示覆盖用户已有
provider 变量。开关写用户全局配置，项目配置不能改变 `env`/`env_force`。已激活 shell
在下一次 prompt 刷新，新 activation 立即应用；`deactivate` 会恢复捕获的原值。

Hugging Face adapter 导出：

```text
HF_ENDPOINT
HF_HOME=<cache>/pkg/models/huggingface
HF_HUB_CACHE=<...>/hub
HF_XET_CACHE=<...>/xet
HF_ASSETS_CACHE=<...>/assets
HF_HUB_OFFLINE=1                    # 仅 osdk offline 时
MODEL_ENDPOINT=<选中的 HF 兼容端点>  # llama.cpp 读它，不读 HF_ENDPOINT
LLAMA_CACHE=<...>/hub               # llama.cpp 自己的下载目录变量
```

**为什么 llama.cpp 需要单独两个变量**：llama.cpp 的 `-hf` 下载器从
`MODEL_ENDPOINT`（而不是 `HF_ENDPOINT`）读取 Hugging Face 兼容端点，并以
`LLAMA_CACHE` 覆盖下载目录（依据上游 `docs/models.md`）。只导出 HF 的名字，llama.cpp
仍然直连 huggingface.co、镜像对它不生效。新版 llama.cpp 已把 `-hf` 文件放进标准 HF
缓存（`HF_HOME`/`HF_HUB_CACHE` 优先），所以 osdk 让 `LLAMA_CACHE` 与 `HF_HUB_CACHE`
指向同一个受管 `hub` 目录——新旧两版 llama.cpp 因此共用一份 GGUF，而不是各下一遍。

ModelScope adapter 导出：

```text
MODELSCOPE_ENDPOINT
MODELSCOPE_CACHE=<cache>/pkg/models/modelscope
```

ModelScope 客户端没有等价的全局 offline 变量，osdk 不会虚构
`MODELSCOPE_OFFLINE`。当 osdk 管理一个不允许转发凭据的自定义端点时，还会清空
相关 token、禁用 Hugging Face 隐式 token，并使用隔离的匿名 HOME，避免持久凭据泄露。
