# 模型快照

osdk 把 Hugging Face 与 ModelScope 仓库作为多文件、不可变快照管理。模型文件
进入与 SDK 共用的 BLAKE3 CAS，但解析、manifest、当前快照和环境适配均是独立流程。

## 命令参考

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

| `pull` 参数 | 作用 |
| --- | --- |
| `NAME` | 本地逻辑名；只允许 ASCII 字母、数字、`.`、`_`、`-` |
| `REFERENCE` | `PROVIDER:owner/repo@revision` |
| `--endpoint URL` | 覆盖 provider endpoint；优先于环境变量和 source 选择 |
| `--forward-credentials` | 允许这个显式自定义 endpoint 接收 provider token |
| `--include GLOB` | 可重复；至少匹配一个 include 时才下载 |
| `--exclude GLOB` | 可重复；在 include 结果上继续排除 |
| `--variant LABEL` | 记录到快照 identity、manifest 和 lock 的标签；**不会自动筛文件** |
| `--no-lock` | 不更新最近项目位置的 `osdk.lock` |

`list` 显示每个逻辑名的当前快照；`path` 输出当前路径；`verify` 校验当前快照所有
文件；`remove` 删除该逻辑名的全部快照并立即执行 CAS GC，当前不会请求确认。

`path --stable` 输出 `<data>/models/<name>/current`，这是一个指向当前快照的目录
链接（Windows 上是 junction，其他平台是符号链接）。快照目录名里含内容哈希，改
`--include`/`--exclude` 或换 revision 都会换目录，所以**要写进别处的路径请用
`--stable`**：ComfyUI 的 `extra_model_paths.yaml`、llama.cpp 的 `-m`、脚本里的
常量都属于这种情况。不带 `--stable` 时输出带哈希的真实快照路径，适合只用一次的
场合。

```bash
osdk model path qwen25            # …/snapshots/9f1c2a…
osdk model path qwen25 --stable   # …/qwen25/current  ← 下次 pull 后仍然有效
```

## Provider 引用

```text
hf:owner/repo@revision
huggingface:owner/repo@revision
hugging-face:owner/repo@revision

ms:owner/repo@revision
modelscope:owner/repo@revision
model-scope:owner/repo@revision
```

省略 revision 时，Hugging Face 默认 `main`，ModelScope 默认 `master`。repository
必须正好是 `owner/name` 两段，每段只允许 ASCII 字母、数字、`.`、`_`、`-`。

```bash
osdk model pull qwen25 hf:Qwen/Qwen2.5-7B-Instruct@main
osdk model pull qwen25-ms ms:Qwen/Qwen2.5-7B-Instruct@master
osdk model pull qwen25 hf:Qwen/Qwen2.5-7B-Instruct@main \
  --include '*.json' --include '*.safetensors' \
  --exclude 'original/*' --variant safetensors-fp16
```

Hugging Face 将 branch/tag 解析为不可变 commit SHA。ModelScope 文件 API 没有等价
commit 时，osdk 以请求 revision 和排序后的文件路径、大小、SHA-256 manifest 生成
`revision+manifest-<16 hex>` identity。远端文件路径必须是安全相对路径。

## 下载、校验与本地布局

文件按 `settings.jobs` 并发下载，支持 Range/ETag 续传。上游提供 SHA-256 时强制
校验；未提供时仍计算并记录本地 SHA-256。`model verify` 同时检查 CAS BLAKE3 与
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

默认 `pull` 在 `osdk.lock` 顶层 `[models.<name>]` 记录：

- provider、repository、请求 revision 与不可变 revision；
- 实际 endpoint 与可选 variant；
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

`osdk model sync` 是 `[models]` 段的读取方——`pull` 写、`sync` 复现，二者的关系与
工具的 `lock` / `install` 相同。`osdk install` 刻意不代拉模型：权重太大，不该作为
装工具的副作用被下载，所以这是一个独立动词。

```bash
osdk model sync                 # 还原 lock 声明的全部模型
osdk model sync --dry-run       # 只报告会做什么
osdk model sync --prune         # 同时删除 lock 不再声明的本地快照
osdk model sync --prune --dry-run
```

还原时按 lock 里的**不可变 revision** 重建引用，而不是 `requested_revision`：复现
一个分支名会解析到它当前所指，恰好与 lock 的目的相反。只拉 lock 列出的文件，因此
仓库在锁定后新增的文件不会让快照静默变大；还原完成后逐文件比对大小与 SHA-256，
不一致即失败——lock 的意义就是钉住内容。

已存在且校验通过的快照不会重新下载：lock 带有每个文件的摘要，「这是不是 lock 描述
的那份」可以本地回答，为此重下若干 GB 毫无意义。校验失败的快照会被重新拉取，因为
那时本地副本已不是提交进版本库的那份。

`--prune` 默认关闭：它删除的是已物化的权重，重新获取代价高，所以必须显式要求，而不
能作为 sync 的副作用发生。

### remove 与 lock 保持一致

`osdk model remove <name>` 同时删除本地快照与 lock 条目。此前只删快照，lock 仍声称
拥有它，于是下一次 `sync` 会忠实地把刚删掉的东西拉回来。

只想在本机删除而不改变项目声明时用 `--keep-lock`，之后 `sync` 会重新还原它。

## Endpoint 与凭据

解析优先级：

```text
--endpoint
> HF_ENDPOINT / MODELSCOPE_ENDPOINT / MODELSCOPE_DOMAIN
> source pin、测速排名和内置 endpoint
```

Token 读取顺序：

| Provider | 环境变量，左侧优先 |
| --- | --- |
| Hugging Face | `OSDK_HF_TOKEN`、`HF_TOKEN`、`HUGGING_FACE_HUB_TOKEN` |
| ModelScope | `OSDK_MODELSCOPE_TOKEN`、`MODELSCOPE_API_TOKEN` |

官方端点 `https://huggingface.co`、`https://modelscope.cn`、
`https://www.modelscope.ai` 可接收对应凭据。自定义 source 或 `--endpoint` 默认匿名，
只有 `--forward-credentials` 或 source 的 `forward_credentials = true` 才转发。
ModelScope 会同时使用 Bearer header 与 `m_session_id` cookie。

模型 source 命令与 SDK 相同，但测试时必须指定仓库：

```text
osdk source list huggingface|modelscope
osdk source test huggingface|modelscope --model owner/repo[@revision]
osdk source add huggingface|modelscope --id ID --download-url URL
  [--index-url URL] [--forward-credentials]
osdk source remove huggingface|modelscope ID
osdk source pin huggingface|modelscope ID
osdk source unpin huggingface|modelscope
```

探测会先解析目标仓库 metadata，再对一个真实文件做最多 1 MiB 的 Range 下载；
匿名与带凭据模式使用不同缓存键。更多 source 规则见[下载源与供应链安全](./sources-security)。

## 消费者视图（model view）

快照按上游仓库布局存放（`unet/`、`vae/`、`text_encoder/` 平级），消费者要的是
另一种形状。`osdk model view` 把已 pull 的快照**渲染成消费者形状的目录**，文件以
链接（同卷硬链接，跨卷退化为拷贝并明确计数）指回快照，不复制权重；视图文件设为
只读，避免消费者就地写入污染快照与 CAS。

- `add <comfyui|hf-cache> <name>`：把模型加入视图并渲染。`comfyui` 渲染成
  `<view>/<类别>/<文件>`（25 个类别目录预先建好，按 `unet/vae/text_encoder/loras`
  等目录约定归类）；`hf-cache` 渲染成 `models--org--repo/{refs,blobs,snapshots}`。
  模型必须先 `model pull`，否则报错并提示先 pull。
- `--map PREFIX=CATEGORY`（可重复）：显式指定仓库路径前缀到类别的映射，最长前缀
  优先。**无法归类的文件不会被兜底塞进 checkpoints**，而是跳过并由 `view doctor`
  列出。
- `path`：打印稳定的视图根，路径不随 pull/`--include` 变化——把它写进消费者配置。
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
osdk model pull flux hf:org/flux-GGUF --include 'unet/*' --include 'vae/*'
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

`enable`/`disable` 只接受可选 provider；`--force` 只属于 `enable`，表示覆盖用户已有
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
