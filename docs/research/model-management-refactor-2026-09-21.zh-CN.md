# osdk 模型管理重构调研：从「不可变快照」到「消费者可直接使用的模型环境」

日期：2026-09-21

状态：**调研 + 重构设计，未改动任何产品代码。** 本轮全部实测在 `%TEMP%\osdk-model-probe-20260921` 与临时 `OSDK_DATA_DIR/OSDK_CACHE_DIR/OSDK_CONFIG_DIR` 下完成，用户真实数据根 `E:\osdk-data` 未被触碰；仓库工作区在本文落盘前 `git status` 为 clean。

配套可视化：[`model-management-refactor-2026-09-21.visual.html`](./model-management-refactor-2026-09-21.visual.html)

外部事实核查基准日：2026-09-21。每条关于外部产品的机制断言都给出来源（上游源码 URL、官方文档 URL，或本机已安装产物的字节内容）；每个数字都给出测量命令与环境。**凡属推断而非查证的，本文一律显式标注为「推测」，实现时不得当作既定事实。**

> **2026-09-21 当日修订（第二轮）。** 初稿写作时本机 ComfyUI Desktop 尚未完成首次启动向导，ComfyUI 一族结论全部来自上游源码离线复现。此后用户完成了安装，本轮在**真实安装**上补做核实与端到端实跑，产生三处**推翻或实质修正**、三处**结论升级**：
>
> - **§3.2 推翻**：初稿说「用户在 Storage 面板点一下 Add Shared Directory」——那不是可程序化接口。真实接口是 `%APPDATA%\Comfy Desktop\settings.json` 的 **`modelsDirs` 数组**，Desktop 在**每次启动时**据它生成 `instance-model-paths\<id>.yaml`，再以 `--extra-model-paths-config` 传给 ComfyUI。完整链路由运行中进程的命令行证实（§3.2.1）。
> - **§3.2 修正**：Desktop 版布局是**三根分离**（`ComfyUI-Cache` / `ComfyUI-Installs` / `ComfyUI-Shared`），不是单一 ComfyUI 目录；`ComfyUI-Shared\models` 下已**预建 25 个类别子目录**。初稿据官方文档写的 `%LOCALAPPDATA%\Comfy-Desktop` 在本机不成立（实际在 `E:\Comfy-Desktop`，因装到了其他盘）。
> - **§6.1 修正**：`osdk model view` **不应**直接写 `settings.json`。Comfy Desktop 自带维护脚本原文写明「the launcher will overwrite settings.json / installations.json on its own save cycle」，本轮也实测到它在自己的保存周期重写 `installations.json` 并轮转 `.bak`（§3.2.3）。
> - **§5.9 新增（源码推导 → 实跑验证）**：用安装自带的 Python 3.13.12 启动真实 ComfyUI v0.34.0，osdk 渲染视图中三个类别的权重**全部出现在 `/object_info` 的下拉框数据里**，并带「渲染错 / 类别不匹配 / 未重扫」三路可区分的控制项。
> - **§5.8 升级**：torch+CUDA 从「索引页存在该 wheel」升级为**本机实跑**：`torch 2.12.1+cu130`、`torch.cuda.is_available() == True`（RTX 4080 Laptop，驱动 610.47）。
> - **§7 重写**：给出「源码版 vs Desktop 版」的**明确推荐**（初稿只并列三条路径，未下结论）。
>
> 另记一条**方法论教训**（§5.10）：监控安装进度的探针用 `Get-ChildItem | Measure-Object Length` 读大小，连续八分钟报 0 MB，几乎让我写下「下载卡住」这一错误结论——该文件当时已有 1.65 GB。**缺陷在测量工具里，不在被测对象里。**

---

## 执行摘要

用户的诉求是：osdk 能管理模型下载与缓存、能镜像加速、能按消费者要求把模型落到对的位置（配置文件或环境变量），并且达到「一个 ComfyUI 项目 `osdk install` 之后 python / uv / 依赖 / 模型 / CUDA 全部就绪，直接能跑」。核查与实测之后，有九条结论决定了整个重构的形状。

**一、现有 model 子系统做对了「获取与完整性」，完全没有做「交付」。** `model pull` 产出 `<data>/models/<name>/snapshots/<24 位 blake3 hex>/`，内容哈希、CAS 去重、SHA-256 逐文件入锁、offline 可重建——这套获取语义是扎实的，不该推翻。但它的输出**没有任何消费者能直接使用**：没有稳定入口（实测 `<data>/models/<name>/` 下 reparse point 数 = 0，只有需要自行解析的 `current.json`），改一次 `--include` 就换一个哈希目录（实测两次 pull 产生 `4d44c30c20f5efa903ebe9b0` 与 `5be8be92c71b9618dd17a615` 两个目录，`current.json` 改指向），而消费者要的是「一个稳定路径 / 一份它自己认识的目录形状」。**结论：不是重写获取层，而是新增一个「呈现层（view）」。**

**二、`model env` 与 `model pull` 是两条互不相通的路径，这是当前最大的功能断裂。** 实测 `model env list` 导出的 `HF_HOME`/`HF_HUB_CACHE` 指向 `<cache>/pkg/models/huggingface/...`，而 pull 的产物在 `<data>/models/...`。启用 env 不会让任何工具看见 pull 的成果；env 只影响「客户端库自己下载时落到哪」。这不是配置疏漏，而是两套设计从未对接。**修法有现成的、已实测可行的答案**：让 osdk 直接把快照**渲染成 Hugging Face 缓存布局**（`models--org--repo/{blobs,snapshots/<commit>,refs/<branch>}`），于是 `HF_HUB_CACHE` 一指，transformers / diffusers / vLLM / llama-cpp-python 全部立即可用。§5.2 用真实 `huggingface_hub 1.32.0` 完全离线验证了这条路径（7 项断言全过，3 个变异全部被捕获）。

**三、「一 repo 多类别」与「一目录一类别」的冲突，靠渲染视图解决，不靠让用户写配置。** 扩散仓库在同一 repo 内含 `unet/`、`vae/`、`text_encoder/`，分属 ComfyUI 的三个类别；而 `extra_model_paths.yaml` 的语义是「一目录 → 一类别」。实测（上游 `folder_paths.py` 原样逻辑）：整体映射为 `checkpoints` 会把三个权重塞进同一下拉框（`["text_encoder\\clip_l.safetensors","unet\\flux1-dev.safetensors","vae\\ae.safetensors"]`）。正确解法是 osdk 渲染一个 **ComfyUI 形状的 models 根**（`diffusion_models/`、`vae/`、`text_encoders/` …），用硬链接指回快照的同一批字节。**本轮已在真实 ComfyUI v0.34.0 上跑通**（§5.9）：三个类别的权重全部出现在 `/object_info` 返回的下拉框数据里。

**四、Desktop 版的对接接口是 `settings.json` 的 `modelsDirs` 数组，但 osdk 不应该直接写它。** 真实链路（由运行中进程的命令行证实）：`modelsDirs` → 启动时 `ensureModelPathsConfig()` 生成 `%APPDATA%\Comfy Desktop\instance-model-paths\<id>.yaml` → `--extra-model-paths-config <yaml>` 传给 ComfyUI。**数组每个元素成为 YAML 里一个独立的 `comfy.desktop_<i>:` 段，各带自己的 `base_path`**，所以「追加一个 osdk 目录」在语义上就是「增加一个模型根」，与内置根平级（§5.11 实测：追加根的权重可见、用户原有根不被顶掉、同名文件取注册顺序中第一个**实际存在**的，`is_default` 会把该根提到最前并因此反转同名冲突的胜者）。但 Desktop 自带维护脚本写明它会在自己的保存周期覆盖 `settings.json`，本轮也实测到它重写 `installations.json` 并轮转 `.bak`——**osdk 直接写 = 与另一个进程抢一个它不拥有的文件**。正确做法见结论九与 §6.1。

**五、`[models]` 声明段今天加进 `osdk.toml` 会直接让项目不可用，这是必须在设计里正面处理的既有约束。** `trust.rs` 对无法识别的顶层表 fail-closed 归为 `ExecutesCode`，`affects_tool_dispatch` 对未列名的表返回 `true`。实测对照组：不含 `[models]` 的项目 `osdk current` exit=0；仅多一段 `[models.flux]` 的项目 exit=1，报「project config contains trust-required fields and is not trusted」。**所以「把 `[models]` 从 lock-only 变成 osdk.toml 可声明」这一步必须同时给出 trust 分类**，否则每个声明模型的项目会连 `cargo --version` 都跑不了（AGENTS.md 里 `[syspkg]` 踩过的同一个坑）。

**六、镜像加速这件事，osdk 已有一半机制，但另一半是坏的，而且坏得很安静。** 实测：`osdk source add huggingface --id hf-mirror --download-url https://hf-mirror.com` + `source pin` 后 `model pull` 成功（2.3 s）。但**这次 pull 把镜像写进了 lock**：`endpoint = "https://hf-mirror.com"`，快照 manifest 同样记 `https://hf-mirror.com`。`canonical_provider_endpoint` 只折叠 provider 的**内置** endpoint，自定义源原样保留——注释里写明了这是刻意的（osdk 无法断言一个自定义域名服务的是谁的内容），但后果是：**镜像是自定义源，于是「用镜像」必然等于「把本机的便利写进所有人的锁」**。真正的修法是把常用镜像升格为 provider 的**内置候选**并带上「等价于官方」的标记，而不是让用户自己 `source add`。另注：Desktop 的 `useChineseMirrors: true` **只覆盖 Git（gitcode.com）与 PyPI**，不覆盖模型权重下载（§3.2.4），因此与 osdk 的模型镜像**互补而非重复**。

**七、模型源的自动测速在本机 100% 失效，原因是预算而非网络，且用户无法调高预算。** `osdk source test huggingface --model openai-community/gpt2` 对 official 与 hf-mirror 都报 unreachable，而同一时刻 `model pull` 正常。拆开测量：metadata 589 ms + 最大文件 1 MiB Range 下载 1779 ms = 2368 ms，而 `sources.probe_timeout_ms` 默认 1500 ms 覆盖的是**两段之和**。更要命的是 `osdk config set sources.probe_timeout_ms` 报 unknown setting——这个键不在可配置列表里。结论：模型源测速对大仓库（gpt2 最大文件 `rust_model.ot` 702 MB）系统性超时，排名退化为配置顺序，而用户没有 CLI 手段修正。

**八、下载缓存与 CAS 的双份拷贝是真实的空间浪费，模型场景下代价是「每个权重两份」。** 实测：快照文件与 CAS 对象共享 inode（`fsutil hardlink list` 显示 3 条链接 = 两个快照 + 一个 CAS 对象），但 `<cache>/downloads/models/...` 下那份是独立的 1 链接文件，`CAS_BYTES=1042966`、`DOWNLOAD_CACHE_FILES=2 BYTES=1042966`——两份完全相同的字节。在 gpt2 这个尺度上无所谓，在 FLUX / Qwen-72B 这个尺度上是几十 GB。而且它只由 `cache clean` 回收，`gc_roots` 覆盖不到。

**九、「install 后直接能跑」：对 ComfyUI 应当推荐 Desktop 版，osdk 不接管其运行时，只接管模型。** 这是本轮最重要的方向性结论，取代初稿里并列三条路径的写法。理由是实测出来的：Desktop 的 standalone 变体是一个**完全钉死的环境**（`manifest.json` 记 Python 3.13.12 / torch 2.12.1+cu130 / torchvision 0.27.1 / uv 0.12.9 / `comfyui_commit 12d5279…`），本机实跑 `torch.cuda.is_available() == True`。osdk 去接管它，等于用 osdk 的 Python 与 torch 替换一个上游已验证的组合，收益为负、风险为正。**做不了的仍是 NVIDIA 显卡驱动**——内核态组件，不能由用户态包管理器装；osdk 只能检测并在安装前报错。完整推荐与代价见 §7。

由此得到主线：**保留获取层，新增呈现层，把「消费者适配」做成 osdk 的一等能力（`osdk model view`）；对 ComfyUI 采取「osdk 只供模型、不接管运行时」的分工，视图路径由 osdk 渲染并打印、由用户在 Desktop UI 里添加一次；把镜像从「用户自己加的自定义源」升格为「provider 的内置等价端点」；把 `[models]` 引入 `osdk.toml` 并同时给出 trust 分类。**

---

## 1. 现状台账（全部已核查）

### 1.1 代码位置与命令面

| 项目 | 事实 | 核查位置 |
| --- | --- | --- |
| 核心模块 | `crates/osdk-core/src/model/`：`mod.rs`(ModelStore/SnapshotManifest/snapshot_key)、`pull.rs`、`env.rs`、`source.rs`、`provider/{huggingface,modelscope}.rs` | 文件列表，7 个文件共 85,463 B |
| CLI 定义 | `crates/osdk-cli/src/cli.rs` 的 `ModelCommand` / `ModelEnvCommand` | `cli.rs` `pub enum ModelCommand` |
| CLI 实现 | `crates/osdk-cli/src/commands.rs` 的 `pub async fn model(...)`、`model_sync`、`model_env` | `commands.rs:6125` 起、`model_env` 在 `:6491` |
| lock 读写 | `crates/osdk-cli/src/lockfile.rs`：`LockedModel`/`LockedModelFile`、`merge_model`、`locked_models`、`remove_model` | `lockfile.rs:327/339/1658/462/476` |
| 现有命令 | `model pull \| sync \| list \| path \| verify \| remove \| env {enable,disable,list}` | `osdk model --help` |
| Provider | 仅 HuggingFace 与 ModelScope，`ProviderId` 是**闭合枚举**而非可扩展注册表 | `model/mod.rs:26` |
| 文档 | `site/guide/models.md`、`site/en/guide/models.md`、`site/guide/implementation/backends-models.md`（及 en 版） | 文件存在 |

**一个结构性观察**：osdk 的工具侧有 `DYNAMIC_NAMESPACES` 这种「单一真相表 + schema」的可扩展机制（`tool.rs:549`，7 个命名空间），而 model 侧的 `ProviderId` 是硬编码两值枚举。要加 civitai / 私有 S3 / 本地目录导入，今天必须改枚举、改 `FromStr`、改 `default_sources`、改 `source::provider()` 四处——这正是 `backend_discovery.rs` 文件头警告过的「一个地方加了另一个地方忘了」的形状。

### 1.2 磁盘布局（实测，非引自文档）

```text
<data>/models/<name>/
├── current.json                 # {"snapshot": "<24 hex>"}
├── .locks/<snapshot>.lock
└── snapshots/<24 hex>/
    ├── .osdk-model.json         # SnapshotManifest
    ├── .osdk-manifest.json      # CAS Manifest
    ├── .osdk-complete           # 完成标记
    └── <仓库原样相对路径>
```

实测命令（临时 OSDK 根，见附录 A.4）：

```text
SNAPSHOT_COUNT_AFTER_FIRST_PULL=1
SNAPSHOT=4d44c30c20f5efa903ebe9b0
CURRENT_JSON_BODY={ "snapshot": "4d44c30c20f5efa903ebe9b0" }
REPARSE_POINTS_IN_MODEL_ROOT=0
SNAPSHOT_COUNT_AFTER_SECOND_PULL=2
CURRENT_JSON_BODY_2={ "snapshot": "5be8be92c71b9618dd17a615" }
```

第二次 pull 只是多了一个 `--include vocab.json`，snapshot 目录名就变了——因为 `snapshot_key()` 把**每个文件的 path/size/sha256/etag** 都喂进 blake3（`model/mod.rs:441`）。这是刻意的（`file_selection_is_part_of_snapshot_identity` 测试明确断言），代价是外部配置写死的路径会在用户改一次选择后静默失效。

### 1.3 空间占用（实测）

```text
SNAPSHOT_FILE_LINK_COUNT=3
  LINK=...\data\models\probe\snapshots\4d44c30c20f5efa903ebe9b0\config.json
  LINK=...\data\store\23\e4\23e4471d412e06128072b559c031207de920b8a56d7108879d4b487c079a310c
  LINK=...\data\models\probe\snapshots\5be8be92c71b9618dd17a615\config.json
DOWNLOAD_CACHE_FILES=2 BYTES=1042966
  DLFILE=\cache\downloads\models\...\config.json links=1
  DLFILE=\cache\downloads\models\...\vocab.json links=1
CAS_BYTES=1042966
```

读法：两个快照与 CAS 对象是**同一个 inode**（链接数 3），这一层去重是有效的；但下载缓存那份是独立的（links=1），与 CAS 字节数相等 ⇒ 磁盘上确实是两份。模型场景下这是按 GB 计的浪费。

### 1.4 `model env` 导出的内容（实测）

```text
HF_ASSETS_CACHE=<cache>\pkg\models\huggingface\assets
HF_ENDPOINT=https://huggingface.co
HF_HOME=<cache>\pkg\models\huggingface
HF_HUB_CACHE=<cache>\pkg\models\huggingface\hub
HF_XET_CACHE=<cache>\pkg\models\huggingface\xet
```

注意全部在 `<cache>` 下，而 pull 的产物在 `<data>` 下。两者没有任何交集。`configured_env` 还有一条值得保留的安全逻辑：当端点是不转发凭据的自定义源时，清空 `HF_TOKEN`/`HUGGING_FACE_HUB_TOKEN`、置 `HF_HUB_DISABLE_IMPLICIT_TOKEN=1`、切到隔离的 `anonymous-home`（`model/env.rs:68-76`）。重构必须保留这段。

### 1.5 lock 形状（实测产物）

```toml
schema = 4

[platforms]

[models.probe]
provider = "huggingface"
repository = "openai-community/gpt2"
requested_revision = "main"
revision = "607a30d783dfa663caf39e06633721c8d4cfcd7e"
endpoint = "https://huggingface.co"

[[models.probe.files]]
path = "config.json"
size = 665
sha256 = "0daed7749b4f02b8f76240d5444551d7b08712dab4d0adb8239c56ba823bb7b4"
```

`[models]` 是 lock 的顶层段（与 `[platforms]` 平级，不分平台——正确，模型与平台无关）。**目前没有 `osdk.toml` 侧的对应声明**：`ConfigFile`（`config/mod.rs:826`）只有 `settings/sources/registries/containers/syspkg/tools/aliases/tasks/task_config`。所以今天模型只能「先 pull 才有 lock」，不能「先声明再 install」。

### 1.6 trust 现状与 `[models]` 的碰撞（实测）

`trust.rs:294` 对未识别顶层表 `.unwrap_or(TrustReason::ExecutesCode)`；`affects_tool_dispatch`（`trust.rs:238-247`）只把 `syspkg`/`task_config` 排除在 shim 门禁之外，其余 `_ => true`。

实测（带对照组，确认不是环境问题）：

```text
--- control-no-models: `osdk current` exit=0
      python 3.12 (project ...\control-no-models\osdk.toml)
--- with-models: `osdk current` exit=1
      error: project config contains trust-required fields and is not trusted: ...\with-models\osdk.toml
```

两个项目唯一的差别是后者多了：

```toml
[models.flux]
source = "hf:black-forest-labs/FLUX.1-dev"
```

---

## 2. 六个结构性冲突（正面回答，不回避）

### 冲突 1：内容哈希快照 vs 消费者要求稳定路径

**冲突是真的**，且不能靠「让哈希更稳定」化解：哈希包含文件选择，而文件选择本来就是身份的一部分（去掉这一点会让两次不同 `--include` 的 pull 互相覆盖，这正是现有测试在防的）。

**解法：不动快照，增加稳定入口。** `<data>/models/<name>/current` 做成指向当前快照的目录级链接（Windows junction / Unix symlink），`current.json` 保留为机器可读记录。实测（§5.3）junction 在**非管理员**下可创建、可读穿、可原地重定向，且删除 junction 不影响目标内容。ComfyUI 的 `recursive_search` 用 `os.walk(..., followlinks=True)`（上游 `folder_paths.py`），实测能穿过 junction 正常列目录，重定向后文件名缓存也不残留（§5.1）。

### 冲突 2：一 repo 多类别 vs 一目录一类别

**冲突是真的**，且在 ComfyUI 侧无法用单条配置表达。

**解法：渲染视图。** osdk 按「模型 → 消费者类别」的映射，在 `<data>/views/<consumer>/<profile>/` 下建出消费者形状的目录树，每个文件是指向快照文件的硬链接。消费者配置因此退化成一条 `base_path`。§5.4 已实测这条路径在真实 `folder_paths.py` 下完全成立。

映射从哪来？三档，从自动到手动：
1. **约定推断**：仓库内 `unet/`→`diffusion_models`、`vae/`→`vae`、`text_encoder(_2)/`→`text_encoders`、`*.gguf`→`unet`/`diffusion_models`、根部单一 `.safetensors`→`checkpoints`。
2. **osdk 内置映射表**：按 `provider:repo` 收录已知仓库的精确映射（与 `python_catalog.rs` / `python_releases.rs` 同构的做法）。
3. **项目显式声明**：`[models.<name>.comfyui]` 逐条指定（见 §6.3 schema）。

**必须 fail-closed**：推断不出类别的文件**不进视图**并明确报告，而不是丢进 `checkpoints` 了事。理由与 AGENTS.md「前缀匹配过宽比过窄更危险」同构——错误归类会让用户在下拉框里选到一个加载不了的权重，而这个失败离原因很远。

### 冲突 3：CAS 去重 vs 消费者要求真实文件

**这个冲突比看上去小。** 硬链接对消费者而言**就是**真实文件：同卷、无需提权（§5.3 实测）、`open()` 行为无差别。真正的边界是：
- **跨卷不可硬链接**（实测：`系统无法将文件移到不同的磁盘驱动器`）。跨卷时视图只能退化为 junction（目录级，可跨卷——实测通过）或真实拷贝。
- **HF 缓存布局**官方实现用符号链接（blobs ← snapshots），但 `huggingface_hub` 有 `HF_HUB_DISABLE_SYMLINKS` 开关，且实测（§5.2）**用普通拷贝/硬链接填 snapshots 目录，客户端一样能解析**——因为解析走的是目录结构与 `refs/`，不是链接类型。这条实测很关键：它意味着 osdk 不必在 Windows 上依赖符号链接权限。

### 冲突 4：不可变快照 vs 消费者可能就地写入

**这个冲突是真的，而且是硬链接方案最危险的一面。** 若消费者在视图里就地改写一个硬链接文件，改的是**同一批字节**，快照和 CAS 会一起被污染，`model verify` 随后失败。

**解法分三层**：
1. 视图文件设为**只读属性**（Windows `FILE_ATTRIBUTE_READONLY` / Unix 去掉 w 位），让就地写入变成显式错误而不是静默污染。
2. `osdk model verify` 扩展为覆盖视图：不仅校验快照，也校验视图文件仍与快照同 inode 且摘要一致。
3. 对**已知会写入自身模型目录**的消费者（ComfyUI 的 `.desk*` 临时下载目录就是一例，见 §3.1），视图的**写入区**与**只读区**分开：osdk 只接管只读区，把消费者自己下载的东西留在它原来的位置。

**明确不做**：不要试图用 copy-on-write 拦截写入。reflink 只在部分文件系统可用，且用户态无法拦截 `open(O_WRONLY)`。

### 冲突 5：镜像便利 vs lock 的可移植性

现状实测：自定义镜像源会把镜像域名写进 lock（`endpoint = "https://hf-mirror.com"`）。`source.rs:39` 的 `canonical_provider_endpoint` 注释解释了为什么不折叠自定义源——osdk 无法知道一个任意域名服务的是谁的内容，硬折叠等于往提交文件里写一个虚构来源。这个理由成立。

**解法不是放宽折叠规则，而是缩小「自定义」的范围**：把社区公认的 provider 镜像做成**内置源**（`default_sources` 里多一条，`kind = Mirror`，带 `equivalent_to = Official` 标记），于是它天然落入「内置端点折叠为官方」的既有分支。用户仍可 `source add` 任意私有端点，那种端点继续原样入锁——这是正确的。

**可用镜像的实测状态（2026-09-21，本机经代理）**：

| 端点 | `/api/models/<id>` | `/api/models/<id>/revision/<rev>?blobs=true` | 说明 |
| --- | --- | --- | --- |
| `https://huggingface.co` | 200 / 521 ms | 200 / 653 ms | 官方 |
| `https://hf-mirror.com` | **308 → huggingface.co** | 200 / 1198 ms（直接返回） | 部分路径回跳官方 |
| `https://beta.hf-mirror.com` | 200 / 853 ms | 200 / 853 ms | 未观察到回跳 |
| `https://modelscope.cn` | 200 / 232 ms | —（走 ModelScope 自有 API） | 已内置 |
| `https://www.modelscope.ai` | 200 / 473 ms | — | 已内置 |

**`hf-mirror.com` 的 308 回跳是一个必须在设计里处理的安全点。** osdk 的共享 client 是 `redirect::Policy::limited(10)`（`http/mod.rs:14`）。reqwest 在**跨 host** 重定向时会移除 `AUTHORIZATION`/`COOKIE`（来源：reqwest `redirect.rs` 的 `remove_sensitive_headers`，`cross_host` 分支移除 `AUTHORIZATION, COOKIE, cookie2, PROXY_AUTHORIZATION, WWW_AUTHENTICATE`）。所以「镜像 308 回跳官方」不会泄露 token——但也意味着**匿名镜像请求在回跳后变成匿名官方请求**，即镜像没有起到加速作用，而用户看到的是「配了镜像但还是慢」。设计里应当：探测阶段检测首跳是否 3xx 且跨 host，若是则在 `source list` 中标注「该路径回跳上游」。

### 冲突 6：`[models]` 声明 vs trust 门禁

已在 §1.6 实测。**解法**：在 `TRUST_REQUIRING_TABLES` 中显式收录 `models`，理由按 `sources` 同级 —— 它决定从哪里取字节。但**必须同时**在 `affects_tool_dispatch` 中把 `models` 排除（返回 `false`）：shim 从不读模型声明，让 `[models]` 阻断 `cargo --version` 会精确重演 AGENTS.md 记载的 `[syspkg]` 事故。

更细一层：`[models]` 里真正「决定字节来源」的只有 `endpoint` / 自定义 source 字段；单纯声明 `source = "hf:owner/repo@sha"` 与声明一个 npm 包同级（`trust.rs:79-84` 已论证过「声明装什么」本身不该要求 trust）。**建议按 key 检查而非整表 gate**：把 `models` 加入 `INSPECTED_TABLES`，只有出现 `endpoint` / `insecure` / 自定义 URL 时才标 `WeakensVerification`。

---

## 3. 消费者调研矩阵

每个消费者回答四个问题：**如何发现模型 / 要求什么目录形状 / 能否接受链接 / 是否有类别划分**。来源分「已查证（附 URL 或本机字节）」与「推测」。

### 3.1 ComfyUI（上游 core）

**发现方式（已查证，上游源码）**：`folder_paths.py` 维护全局 `folder_names_and_paths: dict[str, (list[path], set[ext])]`，每类一条。默认根为 `models_dir = <base>/models`，可用 `--base-directory` / `--models-directory` 覆盖。额外路径由 `utils/extra_config.py: load_extra_path_config(yaml)` 注入，通过 `add_model_folder_path(type, abs_path, is_default)` 追加。
来源：`https://raw.githubusercontent.com/comfyanonymous/ComfyUI/master/folder_paths.py`、`.../utils/extra_config.py`（2026-09-21 抓取）。

**目录形状**：一条 YAML section 有一个可选 `base_path`，其余每个 key 是**类别名**，值是相对（或绝对）目录，多行字符串表示多个目录。`base_path` 支持 `expandvars`/`expanduser`，相对路径按 YAML 文件所在目录解析。`is_default: true` 让该目录插到列表首位并作为下载默认位置。

**类别划分（已查证，`folder_paths.py` 顶部）**：`checkpoints, configs, loras, vae, text_encoders(+clip), diffusion_models(+unet), clip_vision, style_models, embeddings, diffusers, vae_approx, controlnet(+t2i_adapter), gligen, upscale_models, latent_upscale_models, custom_nodes, datasets, hypernetworks, photomaker, classifiers, model_patches, audio_encoders, background_removal, frame_interpolation, geometry_estimation, optical_flow, detection`。`map_legacy` 把 `unet→diffusion_models`、`clip→text_encoders`。扩展名白名单 `supported_pt_extensions = {'.ckpt','.pt','.pt2','.bin','.pth','.safetensors','.pkl','.sft'}`；`configs` 只认 `.yaml`；`diffusers` 值为 `["folder"]`；`classifiers` 为 `{""}`。

**链接可接受性（已查证 + 实测）**：`recursive_search` 用 `os.walk(directory, followlinks=True, topdown=True)`，显式跟随链接。`get_full_path` 对 `os.path.islink(full_path)` 但不是文件的情况会打 WARNING 并跳过（即断链会被识别而不是崩溃）。本机实测：junction 与硬链接都被正常发现与读取（§5.1、§5.4）。

**对 osdk 的附带好消息（实测）**：`.osdk-complete` / `.osdk-model.json` / `.osdk-manifest.json` 不在扩展名白名单内，不会污染下拉框；但若改名成 `.safetensors` 就会（变异 `metadata_named_as_weights` 验证了这条断言不是空的）。

### 3.2 ComfyUI Desktop（本机真实安装，与上游 core 不同）

本节的事实分两类来源：**本机已安装字节**（`E:\comfyui\Comfy Desktop\resources\app.asar`，61,022,494 B）与**本机运行时观测**（进程命令行、真实生成的文件）。Desktop 是 Electron 壳，内含 `resources\bootstrap-python\python.exe`（Python 3.13）与 **`uv.exe`（67,758,944 B）**。

#### 3.2.1 真实链路：`modelsDirs` → 生成 YAML → CLI 参数（已由运行中进程证实）

这是本轮**推翻初稿**的地方。初稿以为对接方式是「用户在 Storage 面板点一下」，实际存在一条完全可程序化的链路：

```text
settings.json 的 modelsDirs 数组
  └─ 启动时 resolveLauncherModelDirs(inst, sharedModelsDirs)
       └─ syncCustomModelFolders(installPath, modelDirsForLaunch, …)
            └─ ensureModelPathsConfig(modelsDirs, options)
                 └─ buildYaml(resolved, extraFolders, resolvedPrimary)
                      └─ 写 %APPDATA%\Comfy Desktop\instance-model-paths\<id>.yaml
                           └─ launchCmd.args.push('--extra-model-paths-config', config.yamlPath)
```

**运行中进程的完整命令行（本机实测，非推断）**：

```text
E:\Comfy-Desktop\ComfyUI-Installs\ComfyUI\ComfyUI\.venv\Scripts\python.exe -s ComfyUI\main.py
  --feature-flag show_signin_button=true --feature-flag enable_telemetry=true
  --enable-manager
  --extra-model-paths-config "C:\Users\...\Comfy Desktop\instance-model-paths\inst-1789929692790.yaml"
  --input-directory  E:\Comfy-Desktop\ComfyUI-Shared\input
  --output-directory E:\Comfy-Desktop\ComfyUI-Shared\output
```

它真实生成的 YAML（节选，本机文件原文）：

```yaml
# Generated by Comfy Desktop — do not edit manually.
# When ComfyUI supports all_model_folders, this file will be simplified to:
#   comfy.desktop:
#     base_path: '...'
#     is_default: true
#     all_model_folders: true

comfy.desktop_0:
  base_path: 'E:\Comfy-Desktop\ComfyUI-Shared\models'
  is_default: true
  'checkpoints': 'checkpoints/'
  ...
  'controlnet': |-
    controlnet/
    t2i_adapter/
  ...
  'clip': 'clip/'
  'unet': 'unet/'
```

**关键语义**：`modelsDirs` 的**每个元素**成为一个独立的 `comfy.desktop_<i>:` 段，各带自己的 `base_path`。所以「往数组里追加一个 osdk 渲染的目录」= 「给 ComfyUI 增加一个与内置根平级的模型根」。多根语义已实测（§5.11）。

本轮用探针 `gen_desktop_yaml.py` 复刻了 `buildYaml()`，并用 `compare_yaml.py` 与 Desktop 真实生成的文件逐项比对：段命名、27 个 key 的**顺序**、`controlnet` 的次级目录、`clip`/`unet` 别名在末尾、canonical 先于 legacy——7 项断言全过（含一条「杜撰的 key 在两边都不存在」的反向控制）。**这保证后续所有基于复刻 YAML 的实测都不是在测我自己写的东西。**

#### 3.2.2 真实磁盘布局：三根分离（修正初稿）

初稿据官方文档写 `%LOCALAPPDATA%\Comfy-Desktop`。**本机实际不是**——安装到了其他盘，于是大数据落在 `E:\Comfy-Desktop`（官方文档确实提到「装到其他盘时大数据落 `<drive>\Comfy-Desktop\`」，初稿漏读了这一句的适用性）：

```text
E:\Comfy-Desktop\
├── ComfyUI-Cache\download-cache\v0.34.0-env1_win-nvidia\   # 独立环境包 .7z（2256.2 MB）
├── ComfyUI-Installs\ComfyUI\
│   ├── ComfyUI\            # 源码树，含 .venv、models（26 子目录）、custom_nodes
│   ├── standalone-env\     # 自带 Python 3.13 + uv.exe
│   ├── manifest.json
│   └── requirements-nvidia.txt
└── ComfyUI-Shared\{models,input,output}\
    └── models\             # Desktop 预建 25 个类别子目录
```

`%APPDATA%\Comfy Desktop`（配置与状态）：`settings.json`、`installations.json`、各自的 `.bak`、`instance-model-paths\<id>.yaml`、`logs\`。

`settings.json` 实测内容：

```json
{
  "cacheDir": "E:\\Comfy-Desktop\\ComfyUI-Cache\\download-cache",
  "maxCachedDownloads": 1,
  "onAppClose": "quit",
  "modelsDirs": ["E:\\Comfy-Desktop\\ComfyUI-Shared\\models"],
  "inputDir":  "E:\\Comfy-Desktop\\ComfyUI-Shared\\input",
  "outputDir": "E:\\Comfy-Desktop\\ComfyUI-Shared\\output",
  "installDir":"E:\\Comfy-Desktop\\ComfyUI-Installs",
  "telemetryEnabled": true,
  "useChineseMirrors": true,
  "chineseMirrorsPrompted": true,
  "firstUseCompleted": true
}
```

**注意：至今不存在任何 `extra_model_paths.yaml`**（全目录递归确认）。Desktop 完全以 `instance-model-paths\<id>.yaml` 取代了它，且该文件每次启动重新生成。

#### 3.2.3 osdk 不应直接写 `settings.json`（判定与证据）

三条独立证据指向同一个判断：

1. **上游自己这么说。** Desktop 随包附带的维护脚本（asar 内，PowerShell 与 sh 两版）原文：
   > Close the app before any mutating action — **the launcher will overwrite settings.json / installations.json on its own save cycle.**
2. **本轮实测到了这个保存周期。** 端到端测试前后对 `%APPDATA%\Comfy Desktop` 取指纹：`installations.json` 的 SHA-256 变了，且 `installations.json.bak` 恰好变成了变更前的内容——即**它自己轮转了备份**。diff 出的差异是纯增量字段 `lastLaunchedAt: 1789931393146`（= 03:09:53）与 `lastLaunchedAtByCategory`，与我的操作无关：Desktop 在安装完成后于 03:07:06 **自动启动了它自己的 ComfyUI**（进程命令行已捕获），这是它写的。
3. **`.bak` 只在第一次写。** 同一脚本注释说明 legacy 的 `installations.json.bak` / `settings.json.bak`「only written the first time, so the very first/original snapshot is preserved」——意味着 osdk 若写坏了 `settings.json`，用户的原始快照可能**已经被消耗掉**，无法回滚。

**结论**：`settings.json` 是 Comfy Desktop 拥有的运行时状态，不是公开配置接口。osdk 直接写会与另一个进程竞争同一个文件，而且没有可靠的回滚点。这与用户「不手写 osdk 管控目录下的配置」的偏好是同一条原则的两面——**谁拥有，谁写**。

**替代方案（设计采纳）**：`osdk model view path comfyui` 打印视图根，并给出一句可照做的指引（Desktop 设置 → Storage → Add Shared Directory）。这是**一次性**动作：此后 osdk 重建视图、切换快照，路径不变，用户无需再动。若将来 Desktop 提供了受支持的 CLI/IPC 注入途径，再补一条 `osdk model view attach comfyui`；**本轮未找到这样的途径**（asar 内 `setSetting` 是 renderer 经 `window.api` 的 IPC，没有对外的命令行入口）。

#### 3.2.4 `maxCachedDownloads` 与 `useChineseMirrors`（回答「是否与 osdk 冲突」）

- **`maxCachedDownloads: 1`**：管的是 `ComfyUI-Cache\download-cache` 下**环境包 `.7z`** 的保留份数（本机那份 2256.2 MB），与模型权重无关。osdk 的 CAS / 下载缓存在 `<osdk-data>` 与 `<osdk-cache>` 下，两者**目录不相交、语义不相干**，不冲突。
- **`useChineseMirrors: true`**：asar 内该设置的描述原文是「Git repositories clone from **gitcode.com** instead of github.com」，tooltip 与字段描述均只提 **Git 与 PyPI**。它影响的是 custom node 的 clone 与 pip 安装，**不影响模型权重下载**（模型下载由用户在 ComfyUI 内部发起，走 HF/ModelScope）。所以与 osdk 的模型镜像**互补**：Desktop 管它自己的依赖获取，osdk 管模型权重获取。用户实际会同时得到两套，各管一段。
- 附带实测：环境包本体 `desktop-assets.comfy.org` 在本机**直连可达**（Range 请求 206，1737 ms），并未走任何镜像。

#### 3.2.5 类别常量（引自 bundle，与上游 core 对齐）

`MODEL_FOLDER_TYPES` 25 项；`LEGACY_FOLDER_ALIASES = [{key:"clip",dir:"clip"},{key:"unet",dir:"unet"}]`；`SECONDARY_TYPE_DIRS = { controlnet: ["t2i_adapter"] }`。bundle 注释原文要求它「must stay in sync with」上游 `folder_paths.py`。本机 `ComfyUI-Shared\models` 下已按这 25 项预建目录。

来源：本机 `app.asar`；官方数据位置文档 `https://docs.comfy.org/installation/desktop/usage/settings`。

### 3.3 Hugging Face 生态（transformers / diffusers / peft / vLLM 共用）

**发现方式（已查证，`huggingface_hub/constants.py`）**：
```python
HF_HOME = os.getenv("HF_HOME", os.path.join(os.getenv("XDG_CACHE_HOME", ~/.cache), "huggingface"))
default_cache_path = os.path.join(HF_HOME, "hub")
HF_HUB_CACHE = os.getenv("HF_HUB_CACHE", HUGGINGFACE_HUB_CACHE)   # 后者读 legacy HUGGINGFACE_HUB_CACHE
HF_ASSETS_CACHE = ... ; HF_XET_CACHE = os.getenv("HF_XET_CACHE", HF_HOME/"xet")
ENDPOINT = os.getenv("HF_ENDPOINT", "https://huggingface.co").rstrip("/")
HF_HUB_OFFLINE = _is_true(HF_HUB_OFFLINE or TRANSFORMERS_OFFLINE)
HF_HUB_DISABLE_SYMLINKS / HF_HUB_DISABLE_SYMLINKS_WARNING / HF_HUB_DISABLE_IMPLICIT_TOKEN
HF_TOKEN_PATH = HF_HOME/"token"
```
来源：`https://raw.githubusercontent.com/huggingface/huggingface_hub/main/src/huggingface_hub/constants.py`（2026-09-21）。

**目录形状（已查证 + 实测）**：
```text
<HF_HUB_CACHE>/models--<org>--<name>/
├── refs/<branch>        # 文本文件，内容是 commit sha
├── blobs/<digest>       # 真实字节
└── snapshots/<commit>/  # 仓库树形，默认是指向 blobs 的符号链接
```
`REPO_ID_SEPARATOR = "--"`（constants.py）。

**链接可接受性（本轮实测，§5.2）**：osdk 用**普通文件**（非符号链接）填 `snapshots/` 后，`huggingface_hub 1.32.0` 在 `HF_HUB_OFFLINE=1` 下：`try_to_load_from_cache` 按 commit 命中、按 branch 经 `refs/` 命中、嵌套路径命中、`snapshot_download(local_files_only=True)` 成功返回目录、`scan_cache_dir` 正确识别 repo。三个变异（删 refs、错的 repo 目录名、扁平无 snapshots 层）全部被捕获。

**类别划分**：无。HF 缓存是「按 repo 组织」，类别概念由上层框架（diffusers 的 subfolder、ComfyUI 的类别）自行处理。

**vLLM（已查证）**：`--model` 接受本地路径或 HF repo id；本地路径存在时直接从该路径读 config（`https://docs.vllm.ai/en/v0.6.6.post1/design/huggingface_integration.html`）。`--download-dir` 覆盖下载目录，默认「HF 的默认缓存目录」（`https://docs.vllm.ai/en/latest/cli/bench/startup/`）。改下载路径的官方建议就是设 `HF_HOME`（`https://docs.vllm.ai/en/latest/models/supported_models/`）。另有 `VLLM_CACHE_ROOT`（默认 `~/.cache/vllm`）存 torch.compile 产物，与权重无关但值得一并纳管（`https://docs.vllm.ai/en/stable/deployment/docker/`）。
⇒ **vLLM 不需要 osdk 做任何专门适配**：HF 缓存视图 + `HF_HOME` 即可；要固定到某个具体快照时用 `--model <稳定路径>`。

### 3.4 llama.cpp / GGUF 单文件族

**发现方式（已查证，上游 `common/common.cpp`）**：`fs_get_cache_directory()` 顺序为 `$LLAMA_CACHE` → 平台默认：Linux/BSD `$XDG_CACHE_HOME` 或 `$HOME/.cache/`，macOS `$HOME/Library/Caches/`，**Windows `%LOCALAPPDATA%`**，然后追加 `llama.cpp` 并补尾斜杠。
来源：`https://cdn.jsdelivr.net/gh/ggml-org/llama.cpp@master/common/common.cpp`（2026-09-21 抓取，`fs_get_cache_directory` 原文）。

**CLI 面（已查证）**：`-m <file.gguf>` 直接指文件；`-hf <user>/<model>[:quant]`（env `LLAMA_ARG_HF_REPO`）从 HF 拉取，quant 默认 `Q4_K_M`，会自动附带 mmproj，`--no-mmproj` 关闭；`-cl/--cache-list` 列缓存中的模型。
来源：`https://raw.githubusercontent.com/ggml-org/llama.cpp/master/tools/cli/README.md`。

**镜像（已查证）**：`common.cpp` 中存在 `std::string endpoint = common_get_env("MODEL_ENDPOINT");`——即 llama.cpp 用 `MODEL_ENDPOINT` 而非 `HF_ENDPOINT` 切换下载源，官方 README 举的例子就是 `MODEL_ENDPOINT=https://www.modelscope.cn/`。**这是一个 osdk `model env` 目前完全没有导出的变量。**

**目录形状**：无结构要求——单个 `.gguf` 文件路径即可。**分片 GGUF** 保持原始分片文件名。

**链接可接受性**：未实测。llama.cpp 用标准 `fopen`/mmap 读文件，**推测**硬链接与符号链接均无差别；本文不将其作为既定事实。

**类别划分**：无。

⇒ **osdk 适配成本最低的一类**：给出稳定的单文件路径即可。视图形态是「`<view>/<name>.gguf` 硬链接」，外加可选地把 `LLAMA_CACHE` 指到 osdk 管控目录。

### 3.5 Ollama

**发现方式（已查证，官方 FAQ）**：模型存于 macOS `~/.ollama/models`、Linux `/usr/share/ollama/.ollama/models`、Windows `C:\Users\%username%\.ollama\models`；用 `OLLAMA_MODELS` 改位置。
来源：`https://docs.ollama.com/faq`。

**目录形状（已查证，多方一致）**：自有 blob store：
```text
models/
├── manifests/registry.ollama.ai/library/<model>/<tag>   # JSON 清单
└── blobs/sha256-<digest>                                 # 内容寻址的层
```
来源：`https://docs.ollama.com/faq`（位置）、`https://deepwiki.com/mann1x/osync/8.4-local-filesystem-layout`、arXiv 2603.23996v1 的取证分析（blob 命名与 manifest 校验关系）。

**导入路径（已查证）**：`FROM /path/to/file.gguf`（绝对路径或相对 Modelfile）；分片用 `FROM /path/to/model-*.gguf` 通配或多条 `FROM`；随后 `ollama create <name> -f Modelfile`。
来源：`https://docs.ollama.com/import`、`https://docs.ollama.com/modelfile`。

**关键判断：Ollama 不应由 osdk 直接写盘。** 它的 blob store 由 `ollama` 守护进程管理，manifest 与 blob 的对应关系是它的内部契约；osdk 往里塞文件属于伪造另一个产品的内部状态，与 AGENTS.md 里 Android `package.xml` 那种「上游明确规定的公开格式」不是一回事（后者有 `source.properties` 这个上游自带的真相源，前者没有）。

⇒ **正确适配面是 `ollama create`**：osdk 把 GGUF 落成稳定路径，生成 Modelfile，调用 `ollama create` 让 Ollama 自己入库。代价是磁盘上多一份（Ollama 会拷进自己的 blob store）；**这一点必须在文档里讲明**，不要假装能零拷贝。

### 3.6 sd-webui / AUTOMATIC1111

**发现方式（已查证，上游 `modules/cmd_args.py`）**：全部通过 CLI 参数，每个类别一个：
```text
--data-dir           # 所有用户数据的基路径
--models-dir         # 覆盖 --data-dir 下的模型根
--ckpt-dir, --vae-dir, --embeddings-dir, --hypernetwork-dir, --clip-models-path
--codeformer-models-path, --gfpgan-models-path, --esrgan-models-path,
--bsrgan-models-path, --realesrgan-models-path, --dat-models-path
--ckpt <file>, --vae-path <file>
--no-download-sd-model
```
来源：`https://raw.githubusercontent.com/AUTOMATIC1111/stable-diffusion-webui/master/modules/cmd_args.py`（2026-09-21）。

**目录形状**：与 ComfyUI 同类但类别名不同（`Stable-diffusion` / `VAE` / `Lora` / `ESRGAN` …，见 ComfyUI 官方 `extra_model_paths.yaml.example` 的 `a111:` 段，它正是为共享 A1111 目录而写的）。

**链接可接受性**：未实测，**推测**可行（Python 侧 `os.walk` + `open`）。

**类别划分**：有，与 ComfyUI 不同名 ⇒ 需要一张独立的映射表。

⇒ 适配面是 `--ckpt-dir` 等参数或与 ComfyUI 共用一个物理目录（这正是上游 example 的 `a111:` 段的用途）。

### 3.7 ModelScope

**发现方式（已查证，官方文档）**：默认缓存 `~/.cache/modelscope/hub`，`MODELSCOPE_CACHE` 覆盖。
来源：`https://modelscope.ai/docs/Models/Download-Model`。

**目录形状（已查证）**：`<cache>/hub/models/{owner}/{name}/` 下是仓库原样文件 + `.msc`/`.mdl`/`.mv` 三个元数据文件。
来源：`https://deepwiki.com/modelscope/modelscope/3-modelscope-hub`。注意：这是**扁平的仓库树**，没有 HF 那种 blobs/snapshots 分层 ⇒ osdk 渲染 ModelScope 缓存视图比渲染 HF 缓存更简单。

### 3.8 矩阵汇总

| 消费者 | 发现机制 | 目录形状 | 类别 | 链接 | osdk 适配面 | 证据强度 |
| --- | --- | --- | --- | --- | --- | --- |
| ComfyUI core | `extra_model_paths.yaml` + CLI | models 根 + 类型子目录 | 25+ 类 | junction/hardlink 实测可用 | 渲染视图 + 生成 YAML | 源码 + 本轮实测 |
| Comfy Desktop | **`settings.json` 的 `modelsDirs` 数组** → 启动时生成 `instance-model-paths\<id>.yaml` → `--extra-model-paths-config` | 每个数组元素 = 一个 `comfy.desktop_<i>:` 段 + 类型子目录 | 同上 + `clip`/`unet` 别名 | hardlink 实测可用（真实 ComfyUI） | 渲染视图 + **打印路径供用户一次性添加**（不代写 settings.json） | **真实安装 + 端到端实跑** |
| transformers / diffusers | `HF_HUB_CACHE` / `HF_HOME` | `models--org--repo/{refs,blobs,snapshots}` | 无 | 普通文件即可（实测） | 渲染 HF 缓存布局 | 源码 + 本轮实测 |
| vLLM | 同 HF；`--model` 可为本地路径；`--download-dir` | 同 HF 或任意目录 | 无 | 同上 | 复用 HF 视图 | 官方文档 |
| llama.cpp | `-m <file>` / `-hf`；`LLAMA_CACHE`；`MODEL_ENDPOINT` | 单文件 | 无 | 推测可用（未测） | 稳定 `.gguf` 路径 | 源码 + 官方 README |
| Ollama | 自有 blob store，`OLLAMA_MODELS` | `manifests/` + `blobs/sha256-*` | 无 | 不适用 | `ollama create` + Modelfile | 官方文档 |
| sd-webui | 逐类别 CLI 参数 | 类别目录 | 有（异名） | 推测可用（未测） | 渲染视图 + 生成启动参数 | 源码 |
| ModelScope 客户端 | `MODELSCOPE_CACHE` | `hub/models/{owner}/{name}/` 扁平 | 无 | 推测可用（未测） | 渲染缓存视图 | 官方文档 + DeepWiki |

---

## 4. 目标架构：三层分离

```text
┌─ 获取层 Acquire ──────────────────────────────────────────┐
│ provider registry（可扩展，取代闭合 enum）                  │
│ hf / modelscope / http / local / (civitai / s3 …)         │
│ 镜像作为 provider 的内置等价端点，测速排名，失败切换         │
│ 产出：DownloadedModelFile[]（已校验 SHA-256）              │
└───────────────────────────────┬───────────────────────────┘
                                │
┌─ 存储层 Store（基本保留现状）──▼───────────────────────────┐
│ CAS（blake3）+ 不可变快照 <data>/models/<n>/snapshots/<h>/ │
│ 新增：<data>/models/<n>/current  稳定入口（junction/symlink）│
│ 新增：下载缓存直接 ingest 进 CAS，消除双份拷贝               │
└───────────────────────────────┬───────────────────────────┘
                                │
┌─ 呈现层 View（全新）──────────▼───────────────────────────┐
│ <data>/views/<consumer>/<profile>/  消费者形状的目录树      │
│   comfyui  → models 根 + 类型子目录（硬链接，只读）          │
│   hf-cache → models--org--repo/{refs,blobs,snapshots}      │
│   gguf     → 扁平 *.gguf                                   │
│   a1111    → Stable-diffusion/ VAE/ Lora/ …                │
│ 交付方式二选一或并用：                                       │
│   ① 生成消费者配置文件（ComfyUI YAML / sd-webui 参数）       │
│   ② 导出环境变量（HF_HUB_CACHE / LLAMA_CACHE / MODELSCOPE_…）│
└───────────────────────────────────────────────────────────┘
```

三层分离带来的性质：
- 快照仍然不可变、内容寻址、可 `verify`——**现有语义一条不丢**。
- 视图是**派生数据**，可以随时重建、随时丢弃；`model view rebuild` 是幂等的。
- 同一份字节可以同时被多个消费者看到（硬链接），不额外占空间。
- 换 `--include` 导致快照哈希变化时，视图重建即可，**消费者侧的配置路径不变**。

---

## 5. 实测记录

全部探针位于 `%TEMP%\osdk-model-probe-20260921\`，每个探针都配一个变异 harness。**判据遵循 AGENTS.md「验证失效模式」：在相信一个 PASS 之前，先注入它应当捕获的缺陷，确认它变红。** 探针里刻意不复刻被测判据——ComfyUI 侧直接 import 上游 `folder_paths.py`，HF 侧直接 import 真实 `huggingface_hub`。

### 5.1 ComfyUI 能否穿过 junction 看到内容哈希快照

探针 `probe_comfy.py`（Python 3.11.0，上游 `folder_paths.py` + `utils/extra_config.py` 原样下载）。12 项断言全过：

```text
PASS junction created without elevation
PASS junction resolves to snapshot A
PASS diffusion_models sees exactly its own weight :: ["flux1-dev.safetensors"]
PASS vae sees exactly its own weight :: ["ae.safetensors"]
PASS text_encoders sees exactly its own weight :: ["clip_l.safetensors"]
PASS get_full_path resolves through the junction
PASS osdk metadata files stay out of the dropdowns :: []
PASS a single whole-snapshot mapping does lump categories together ::
     ["text_encoder\\clip_l.safetensors","unet\\flux1-dev.safetensors","vae\\ae.safetensors"]
PASS junction can be retargeted in place
PASS retargeted junction serves the new snapshot's bytes :: b'weights-v2'
PASS filename cache does not go stale across a retarget :: ["flux1-dev-fp8.safetensors"]
PASS get_full_path after retarget finds the new file
PASS get_full_path after retarget reads the new snapshot :: b'weights-v2'
```

变异 harness `probe_comfy_mutations.py`：

```text
baseline exit=0 failures=[]
MUTATION one_entry_for_whole_snapshot exit=1 caught=True
MUTATION no_retarget                  exit=1 caught=True
MUTATION metadata_named_as_weights    exit=1 caught=True
SURVIVORS: []
```

**一个过程教训，按 AGENTS.md 要求记录下来**：第一版探针里，重定向前后的 unet 文件同名，「filename cache 不陈旧」这条断言即使不重定向也成立；第二版把重定向后的文件改名为 `flux1-dev-fp8.safetensors`，`no_retarget` 变异才从存活变为被捕获。**探针的第一版是空的，而它是绿的。**

第二个教训：`no_retarget` 变异最初以 traceback 结束而非 FAIL 行，harness 因此判定「未捕获」。修法是把裸 `open()` 换成 `read_or_none()`，让缺文件表现为该断言的 FAIL 而不是终止整个探针——**一个会崩溃的探针，其失败信号无法归因到具体断言**。

### 5.2 osdk 能否直接产出 Hugging Face 缓存布局

探针 `probe_hf_cache.py`，隔离 venv，`huggingface_hub 1.32.0`，`HF_HUB_OFFLINE=1`，snapshots 层用**普通拷贝**而非符号链接：

```text
huggingface_hub 1.32.0
PASS client honours the HF_HUB_CACHE osdk exported
PASS try_to_load_from_cache finds a file osdk placed by commit
PASS a branch name resolves through refs/ that osdk wrote
PASS a nested path inside the repo resolves too
PASS snapshot_download works fully offline against osdk's cache
PASS scan_cache_dir recognizes the repo osdk wrote :: ["osdk-probe/demo"]
PASS an absent repo does not resolve :: None
```

变异：

```text
MUTATION no_refs_file             exit=1 caught=True
MUTATION wrong_repo_dir_name      exit=1 caught=True
MUTATION flat_layout_no_snapshots exit=1 caught=True
SURVIVORS: []
```

**这条实测是整个重构最重要的一块地基**：它证明「让 `model env` 与 `model pull` 合流」不需要 HF 客户端的任何配合，也不需要 Windows 符号链接权限。

### 5.3 链接原语在本机的可用性

探针 `probe_links.ps1`（pwsh 7.6.6.0，非管理员）：

```text
FAIL running elevated :: IsInRole(Administrator)      ← 这是前提，不是缺陷
PASS hardlink without elevation (same volume)
PASS hardlink really shares one object :: fsutil reports 2 link(s)
PASS junction without elevation
PASS junction reads through to snapshot A
PASS removing the junction leaves the target intact
PASS retargeted junction reads snapshot B
PASS directory symlink without elevation :: created
PASS cross-volume hardlink is rejected (as expected) :: 系统无法将文件移到不同的磁盘驱动器。
PASS cross-volume junction works and reads through
```

**「目录符号链接免提权成功」这一条不能推广。** 本机 `HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\AppModelUnlock\AllowDevelopmentWithoutDevLicense = 1`，即**开发者模式已开启**；`whoami /priv` 中不含 `SeCreateSymbolicLinkPrivilege`。也就是说符号链接能建，靠的是开发者模式而非权限，**普通用户机器上会失败**。这直接决定选型：

> **稳定入口与跨卷视图用 junction（目录）+ hardlink（文件），不用符号链接。** Unix 侧用 symlink（目录）+ hardlink（文件）。`LinkMode::Symlink` 的既有注释（`store/link.rs:5-8`）已经给出同样的判断，重构应与之一致。

### 5.4 「渲染消费者视图」方案的端到端验证

探针 `probe_view.py`：osdk 快照保持仓库形状，视图按 ComfyUI 形状建硬链接，配置按 Comfy Desktop `buildYaml` 的格式生成（单 `base_path` + 每类一行 + `clip`/`unet` 别名 + `is_default`）：

```text
PASS diffusion_models shows only the unet weight :: ["flux1-dev.safetensors"]
PASS vae shows only the vae weight :: ["ae.safetensors"]
PASS text_encoders shows only the encoder weight :: ["clip_l.safetensors"]
PASS checkpoints stays empty -- nothing was miscategorized :: []
PASS get_full_path resolves inside the view
PASS view file shares its bytes with the snapshot (no second copy) :: fsutil link count = 2
```

变异：

```text
MUTATION copy_instead_of_hardlink     exit=1 caught=True
MUTATION flat_view_no_type_subfolders exit=1 caught=True
SURVIVORS: []
```

`checkpoints stays empty` 这条是**反向控制**：它保证前三条不是「什么都能匹配」的结果。

### 5.5 osdk 不读取 Windows 系统代理

三段式探针 `probe_proxy.ps1`，同一 URL、同一时刻：

```text
LEG1_DOTNET_NO_PROXY_ENV=ok ms=710 status=200
LEG2-NO-PROXY-ENV: exit=1 ms=16572
     WARN model source failed ... error=network timeout: https://huggingface.co/api/models/...
LEG3-PROXY-ENV: exit=0 ms=2321
```

系统代理为 `HKCU:\...\Internet Settings` 的 `ProxyEnable=1, ProxyServer=127.0.0.1:7897`。.NET 读它，osdk（reqwest）不读它，设 `HTTPS_PROXY`/`HTTP_PROXY` 后立刻成功。

**这不是模型子系统的 bug，是 osdk 整体的 HTTP 行为**，但在模型场景下后果最重：用户在浏览器里能打开 huggingface.co，`osdk model pull` 却超时，而错误信息不指向代理。**建议单列一条改进（不属于模型重构，但应在同一轮交付中提出）**：`osdk doctor` 增加「检测到系统代理但 osdk 未使用」的诊断项。

### 5.6 模型源测速为何总是失败

```text
--- probe_timeout_ms configured = 1500/5000/20000（三次）
      probing sources for huggingface ...
        -. official     unreachable
```

三个「配置值」其实都没生效——`osdk config set sources.probe_timeout_ms` 返回：

```text
error: unknown setting `sources.probe_timeout_ms`
  known: jobs, offline, yes, verify_signatures, require_checksums, attestations,
         prerelease, link_mode, lang, shims.include, shims.expose, shims.exclude,
         registries.python.urls, registries.npm.urls
```

于是分腿测量真实耗时：

```text
METADATA_MS=589 status=200
LARGEST_FILE=rust_model.ot SIZE=702517648
RANGE_1MIB_MS=1779 status=206 bytes=1048576
```

589 + 1779 = 2368 ms > 1500 ms。`probe_one`（`model/source.rs:188`）在**一个 `tokio::time::timeout` 内**做完 resolve + 1 MiB Range，超时即整体判失败。`probe_file` 选的是**最大**文件（`source.rs:235`），对大模型仓库这是最坏选择。

**三条修法**（按代价排序）：① `probe_timeout_ms` 纳入 `config set` 白名单；② metadata 与 Range 各自计时，而不是共享一个预算；③ `probe_file` 不选最大文件，选一个「够大到能测吞吐、又不至于让首字节延迟主导」的中等文件，或直接对固定大小做 Range 而不依赖文件本身尺寸。

### 5.7 镜像源会把自己写进 lock

```text
=== source add hf-mirror  → added custom source hf-mirror for huggingface
=== source list huggingface
    official     official https://huggingface.co
    hf-mirror    custom   https://hf-mirror.com
=== source pin huggingface hf-mirror; model pull viamirror ...  → 成功
=== what endpoint did the lock record?
  endpoint = "https://hf-mirror.com"
  MANIFEST_ENDPOINT=https://hf-mirror.com
```

与 `site/guide/models.md` 中「镜像端点会被折叠成官方端点」的表述相比，**实际行为是：只有内置端点会折叠**。文档没有说错（它下一句就说「自定义端点原样保留」），但用户按文档最自然的操作（`source add` 一个镜像）落进的恰好是不折叠的那一支。

### 5.8 PyTorch + CUDA 组合的真实可用性

```text
cu124: win_amd64=46  python_tags=cp38..cp313   sample: torch-2.6.0+cu124-cp312-cp312-win_amd64.whl
cu126: win_amd64=156 python_tags=cp39..cp315   sample: torch-2.9.1+cu126-cp312-cp312-win_amd64.whl
cu128: win_amd64=92  python_tags=cp39..cp314   sample: torch-2.9.1+cu128-cp312-cp312-win_amd64.whl
cu129: win_amd64=26  python_tags=cp39..cp314   sample: torch-2.9.0+cu129-cp312-cp312-win_amd64.whl
cu130: win_amd64=110 python_tags=cp39..cp315   sample: torch-2.9.1+cu130-cp312-cp312-win_amd64.whl
```

读法：`osdk.toml` 里写 `python = "3.12"` + cu126/cu128/cu130 任一，在 Windows x64 上都有现成 wheel；cu124 已停在 torch 2.6.0，不应作为新项目默认。**这条数据是为了不让设计给出「无法实际安装的组合」**（验收标准明确要求）。

**本轮升级为实跑（不再只是索引页存在）**：ComfyUI Desktop 装完后，用它自带的解释器直接验证：

```text
python 3.13.12
torch 2.12.1+cu130
cuda_available True
cuda_ver 13.0
```

GPU 与驱动：`NVIDIA GeForce RTX 4080 Laptop GPU, 610.47`。**这条实跑同时给出两个事实**：① cu130 这一档在真实机器上确实能跑，不只是索引上有；② 驱动 610.47 足以支撑 CUDA 13.0 运行时——也就是 §7 里「驱动是硬前提」那句话在本机是满足的，但在别人的机器上仍需检测。

### 5.9 真实 ComfyUI 端到端（本轮新增，解除初稿最大的局限）

初稿的 §5.1 / §5.4 都是用上游 `folder_paths.py` 离线复现的。本轮 Desktop 装完（`status: "installed"`，环境包 2256.2 MB 下载并解压完成）后，做了一次**真实 ComfyUI 进程**的验证。

**被测对象**：安装自带的 `.venv\Scripts\python.exe`（Python 3.13.12）启动 `ComfyUI\main.py`（v0.34.0，`comfyui_commit 12d5279…`），参数用 Desktop 自己那一套：`--extra-model-paths-config <osdk 生成的 yaml> --port 8199 --cpu --disable-auto-launch`。查询 `/object_info`，读的是**节点输入的 combo 选项**——即 UI 下拉框渲染的同一份数据。

**osdk 侧**：先造一个内容哈希快照（仓库原样布局 `unet/` `vae/` `text_encoder/`），再用 `build_view.py` 渲染成 ComfyUI 形状的 models 根，文件是指回快照的**硬链接**（`fsutil hardlink list` 确认 2 条链接）。YAML 两个根：用户原有的 `ComfyUI-Shared\models` + osdk 视图根。

```text
PASS ComfyUI server started and answers /system_stats
PASS CONTROL: the install's own models dir is readable by this server :: ["osdk-e2e-control.safetensors"]
PASS osdk view weight visible in UNETLoader.unet_name  :: server=["osdk-e2e-unet.safetensors"]  disk=["osdk-e2e-unet.safetensors"]
PASS osdk view weight visible in VAELoader.vae_name    :: server=["osdk-e2e-vae.safetensors"]   disk=["osdk-e2e-vae.safetensors"]
PASS osdk view weight visible in CLIPLoader.clip_name  :: server=["osdk-e2e-clip.safetensors"]  disk=["osdk-e2e-clip.safetensors"]
PASS a file added after startup appears without restart (so 'absent' means absent, not stale cache)
PASS a never-written name does not appear (negative control)
FAILURES: []
```

**「下拉框为空」的三路归因是按任务要求构造进探针的，不是事后解释**：

| 可能原因 | 探针如何把它单独分离出来 |
| --- | --- |
| (a) osdk 渲染错了 | 同一次运行里往**安装自身的** `models\loras` 写一个 control 权重。它不经过 osdk 的任何渲染逻辑；若它也看不见，问题在探针或服务器，与 osdk 无关 |
| (b) 类别映射不匹配 | 每个类别同时报告**服务器返回的选项**与**磁盘上的实际文件**。「磁盘有而服务器没有」⇒ 映射/YAML 问题；「磁盘也没有」⇒ 渲染问题。两种情况分别打印不同的 DIAGNOSIS 行 |
| (c) 服务器还没重扫 | 启动**之后**再写一个新文件，不重启直接再查 `/object_info`；它若出现，说明「没出现 = 真的不存在」而非缓存陈旧。这条不过，上面所有「空」的结论都不成立 |

外加一条反向控制：一个从未写过的文件名必须**不**出现——否则「能看见」可能只是列表什么都返回。

**环境影响与还原（已验证，非声明）**：全程**没有写 `settings.json`**，走的是与 Desktop 相同的 `--extra-model-paths-config` 机制。测试前后对 `%APPDATA%\Comfy Desktop` 与两个 models 根取指纹：

```text
settings.json              SAME   (sha256 0129DE98…5460 前后一致)
settings.json.bak          SAME
sharedModelsInventory      SAME (0 files)
installModelsInventory     SAME (36 files)
installations.json         CHANGED  ← 已归因：Desktop 自己写的，非本测试
```

`installations.json` 的变化已逐行 diff 并归因：新增 `lastLaunchedAt: 1789931393146`（03:09:53）与 `lastLaunchedAtByCategory`，来自 Desktop 在 03:07:06 自动启动的它自己的 ComfyUI 实例（进程命令行已捕获）；我的脚本对该文件只读不写（`grep` 全部探针确认）。control 权重已删除，`E:\Comfy-Desktop` 下 `osdk-e2e*` 残留为 0。

### 5.10 一条方法论教训：测量工具本身是错的

监控安装进度的 watcher 用 `Get-ChildItem | Measure-Object -Property Length -Sum` 读 `.7z` 大小，**连续八分钟报告 0 MB**，而进程持有代理连接。当时几乎要写下「下载卡住」并去查网络。实际情况是：

```text
t=0  Get-ChildItem=1747484099  Get-Item=1801671300  FileInfo=1801671300  OpenStream=1801671300
t=1  Get-ChildItem=1801671300  Get-Item=1811413643  FileInfo=1811413643  OpenStream=1811479107
t=2  Get-ChildItem=1811479107  Get-Item=1818159757  FileInfo=1818159757  OpenStream=1818159757
```

**目录枚举读的是 NTFS 目录项，另一个进程正持续写入时它不刷新**；打开文件才拿到活的大小。更说明问题的是：`Get-ChildItem` 在 t=N 的值恰好等于 `OpenStream` 在 t=N−1 的值——**我自己探针的那次 open 触发了元数据刷新**，于是枚举的结果永远滞后一轮。

修法是让 watcher 打开文件取 `Stream.Length`，改后立刻报出 1820.2 MB 并持续增长到 2256.2 MB 完成。

这正是 AGENTS.md「验证失效模式」的形状：**缺陷不在被测对象里，在验证代码里**，而且症状是「一个看起来很确定的数字」。记在这里是因为它会重演——任何监控 osdk 自己下载进度的代码都会踩到同一个坑。

### 5.11 多根语义：追加一个 osdk 根到底意味着什么

探针 `probe_multiroot.py`（上游 `folder_paths.py`），模拟「用户原有 shared 根（25 个类别齐全）+ osdk 追加根」两根并存，并在两个根里放**同名** `loras/collide.safetensors`：

```text
PASS appended root's weights are visible at all :: ["flux1-dev.safetensors"]
PASS the user's own root is NOT displaced by the appended one :: ["user-own.safetensors"]
PASS vae and text_encoders resolve from the appended root
PASS a name present in both roots is listed once, not twice :: ["collide.safetensors"]
PASS collision resolves to the first registered root that has the file
PASS an appended root does NOT override a same-named file in an earlier root :: b'FROM-SHARED'
PASS a read-only weight file is still discovered
PASS without is_default the osdk root is appended, not promoted :: index=4
PASS a category with no directory in either root is simply empty, not an error
PASS an unfilled category stays empty (negative control)
```

变异 harness：

```text
MUTATION osdk_root_promoted_to_default    exit=1 caught=True
MUTATION only_the_filled_category_dirs    exit=0 EXPECTED-SURVIVE ok=True
PROBLEMS: []
```

**逐条回答任务提出的四个问题**：

1. **追加根是否与内置根等价？** 是。它的权重正常出现在对应类别，且**不会顶掉**用户原有根的内容（两条断言分别验证了「能看见新的」与「没弄丢旧的」）。
2. **同名文件怎么办？** `get_full_path` 按注册顺序返回**第一个实际存在该文件**的根。注意不是「第一个注册的根」——ComfyUI 内置的 `<comfyDir>/models/<cat>` 排在所有配置根之前但通常不含该文件。`get_filename_list` 对同名只列一次，所以 UI 上看不出有两份。**后追加的根不会覆盖先前的同名文件**；若加 `is_default: true`，该根被提到最前，胜者随之反转（变异实测确认）。
3. **类别子目录必须齐全吗？** 不必须。`only_the_filled_category_dirs` 变异（只建实际要填的那几个目录）被标记为 **EXPECTED-SURVIVE 并确实全绿**——缺失的类别只是空列表，不是错误。**但建议仍然全建 25 个**：Desktop 的 `syncCustomModelFolders` 本来就会往每个 `modelsDirs` 元素里 `mkdirSync` 全套类别目录，osdk 预先建好可避免它在 osdk 的只读视图里尝试创建目录。
4. **要求可写吗？** 不要求。把权重文件设为只读后仍被正常发现与列出——这对 §冲突 4 的「视图只读」防线是必要前提，本轮已验证。

> **一个被捕获的自身错误**：该探针第一版断言「冲突取 `registered[0]`」，结果 FAIL——因为 `registered[0]` 是 ComfyUI 内置根，两个文件都不在那儿。**失败的是我的断言，不是被测机制**。改成「按顺序找第一个真正存在该文件的根」后通过，并额外加了一条把结论讲明白的断言（后追加的根不覆盖先前的）。这比直接改成硬编码的胜者要好：后者会在 `is_default` 改变行为时静默继续通过。

---

## 6. 重构设计

### 6.1 CLI 面貌

**保留且语义不变**：`model pull / sync / list / path / verify / remove`。

**变更**：

| 命令 | 变更 | 理由 |
| --- | --- | --- |
| `model path <name>` | 新增 `--stable`，打印 `<data>/models/<name>/current`（稳定入口）而非哈希目录 | 让「写进外部配置」的路径不随 `--include` 变化 |
| `model pull` | 新增 `--as <consumer>[:<profile>]`（可重复），pull 完成后自动建/更新视图 | 「下载完还得手动建视图」是必然被忘掉的一步 |
| `model verify` | 同时校验视图文件仍与快照同一对象且摘要一致 | 冲突 4 的第二层防线 |
| `model remove` | 先拆视图再删快照，GC roots 加入 views | 否则视图变成悬空硬链接，占着 CAS 不放 |

**新增 `osdk model view`**：

```text
osdk model view add <consumer> --model <name> [--profile <p>]
                               [--map <repo-path>=<category>]...
                               [--link-mode hardlink|copy|auto]
osdk model view list [--consumer <c>]
osdk model view path <consumer> [--profile <p>]      # 打印视图根，供用户贴进消费者设置
osdk model view rebuild [<consumer>] [--prune]       # 幂等重建
osdk model view remove <consumer> [--profile <p>]
osdk model view export <consumer> [--to <path>]      # 生成消费者配置文件
osdk model view doctor <consumer>                    # 报告未归类文件、悬空链接、跨卷退化
```

`<consumer>` 取值：`comfyui` | `hf-cache` | `gguf` | `a1111` | `modelscope-cache`（可扩展，见 §6.5）。

**`view doctor` 是设计的一部分而不是附属品**：冲突 2 要求 fail-closed，那就必须有一个命令能回答「哪些文件没能进视图、为什么」。

**关于「自动挂进消费者」的明确边界（§3.2.3 的设计落点）**：

- **不提供** `osdk model view attach comfyui` 这类直接改写 `settings.json` 的命令。Comfy Desktop 会在自己的保存周期覆盖该文件，且 `.bak` 只保留首次快照，osdk 写进去既可能被覆盖、又可能消耗掉用户唯一的回滚点。
- **提供** `osdk model view path comfyui`：打印视图根，并附一行可照做的指引。这是**一次性**动作——此后 osdk 重建视图、切换快照、改 `--include`，这个路径都不变（稳定入口的意义正在于此）。
- `view export comfyui --to <path>` 仍然有用，但它的定位是**给源码版 ComfyUI / sd-webui 用**（那里 YAML 或 CLI 参数由用户自己掌握），不是给 Desktop 用。
- 若将来 Desktop 提供受支持的 CLI / IPC 注入途径，再补 attach 子命令。**本轮在 asar 内未找到这样的途径**：`setSetting` 是 renderer 经 `window.api` 的 IPC，没有对外命令行入口。

`comfyui` 渲染器的具体形状（由 §5.9 / §5.11 实测确定）：

- 渲染成**一个 models 根**，其下按 Desktop 的 `MODEL_FOLDER_TYPES` 建**全部 25 个类别子目录**（缺失并不报错，但预先建好可避免 Desktop 的 `syncCustomModelFolders` 往只读视图里创建目录）。
- 文件用**硬链接**（同卷）指回快照；跨卷退化为 junction（目录级）或拷贝，并明确报告。
- 视图文件设**只读**——§5.11 已验证只读文件仍被正常发现。
- **不要**给 osdk 的视图根加 `is_default`：那会把它提到搜索顺序最前，反转同名文件的胜者，等于悄悄改变用户原有根的优先级（§5.11 变异实测）。默认位置该由用户决定。

**新增 `osdk model env` 的覆盖面**（在既有 HF/ModelScope 之外）：

```text
LLAMA_CACHE            → <data>/views/gguf/<profile>        # llama.cpp（已查证）
MODEL_ENDPOINT         → 当前 provider 端点                   # llama.cpp 的镜像开关（已查证）
HF_HUB_CACHE           → <data>/views/hf-cache/<profile>     # 改为指向视图，而不是空缓存目录
```

保留 `insert_if_allowed` 的「不覆盖用户已设值，除非 `--force`」语义。

**`model source` 相关**：不新增命令，但 `default_sources(HuggingFace)` 增加内置镜像候选（§6.6）。

### 6.2 `osdk.toml` schema

新增顶层 `[models]`：

```toml
[models.flux-dev]
source   = "hf:black-forest-labs/FLUX.1-dev@main"
include  = ["*.safetensors", "*.json", "text_encoder*/**"]
exclude  = ["*.onnx"]
variant  = "fp16"
# 可选：平台过滤，与 [tools] 的 `when` 同构
when     = { os = "windows" }

# 消费者视图声明：一个模型可以同时出现在多个视图里
[models.flux-dev.views.comfyui]
profile = "default"
map     = { "unet/" = "diffusion_models", "vae/" = "vae", "text_encoder/" = "text_encoders" }

[models.flux-dev.views.hf-cache]
profile = "default"

[models.qwen-gguf]
source = "hf:Qwen/Qwen2.5-7B-Instruct-GGUF@main"
include = ["*Q4_K_M.gguf"]
[models.qwen-gguf.views.gguf]
profile = "default"

# 全局视图设置（与 [settings] 平级或作为其子表，二选一；建议子表以减少顶层表数量）
[settings.models]
link_mode   = "auto"        # auto|hardlink|copy；跨卷自动退化并报告
readonly    = true          # 视图文件设只读属性，防止就地写入污染 CAS
auto_view   = true          # install 时自动建视图
```

**字段设计上的三条硬约束**：

1. **`map` 的 key 是仓库内相对路径前缀，value 是消费者类别名。** 路径按 AGENTS.md「跨平台路径」一节处理：写进产物的相对路径**归一成 `/`**，读取侧同时接受 `/` 与 `\`。模型清单与视图 manifest 会直接踩到这族坑。
2. **未被 `map` 覆盖、也无法由约定推断的文件不进视图**，`view doctor` 列出它们。不做「兜底归到 checkpoints」。
3. **`[models]` 必须进 trust 分类**（§6.4）。

### 6.3 lock 的影响

`LockedModel` 增加两个字段，均 `skip_serializing_if` 以保持旧 lock 兼容：

```rust
pub struct LockedModel {
    // 既有字段不变
    pub provider: ProviderId,
    pub repository: String,
    pub requested_revision: String,
    pub revision: String,
    pub endpoint: String,
    pub variant: Option<String>,
    pub files: Vec<LockedModelFile>,
    // 新增
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub views: BTreeMap<String, LockedModelView>,   // consumer -> 视图声明
}

pub struct LockedModelView {
    pub profile: String,
    /// 仓库相对路径前缀 -> 消费者类别；路径一律以 `/` 归一后写入
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub map: BTreeMap<String, String>,
}
```

**不入 lock 的东西**：视图的绝对路径（那是本机位置）、link mode 实际取值（那是本机能力）、镜像端点（见 §6.6 后 endpoint 会折叠为官方）。理由与既有 `merge_model` 的注释完全一致——lock 记录的是身份，不是某台机器怎么够到它。

`schema` 版本：`views` 是可选新增字段，旧 osdk 读新 lock 时 serde 会因未知字段……**需要核查**：`Lockfile` 及 `LockedModel` 未标 `deny_unknown_fields`（`lockfile.rs:46/326` 只有 derive，无该属性），所以旧版本读新 lock 会忽略 `views` 而非报错，`schema` 可以不升。**但这一点本轮只做了源码阅读，未做旧二进制读新 lock 的实测**，列入 §9 诚实清单。

### 6.4 trust 分类

```rust
// trust.rs
const INSPECTED_TABLES: &[&str] = &["tools", "aliases", "settings", "tasks", "models"];

// collect_requirements 的 match 增加：
"models" => collect_models_requirements(value, &mut found),
```

`collect_models_requirements` 的判据：
- 仅有 `source` / `include` / `exclude` / `variant` / `when` / `views` ⇒ **不要求 trust**（与「声明装什么不该要求 trust」一致，`trust.rs:79-84`）。
- 出现 `endpoint`、任何自定义 URL、或 `insecure`-类字段 ⇒ `WeakensVerification`（与 `sources` 同级）。

`affects_tool_dispatch` 增加 `"models" => false`：shim 从不读模型声明。**这一条必须配一个会失败的测试**——把 `models` 从该分支移除后，「带 `[models]` 的项目里 `cargo --version` 仍然可用」的测试要变红。

### 6.5 provider 可扩展化

把闭合 `enum ProviderId` 换成与 `DYNAMIC_NAMESPACES` 同构的静态表：

```rust
pub struct ProviderSchema {
    pub id: &'static str,
    pub aliases: &'static [&'static str],
    pub default_revision: &'static str,
    pub default_sources: fn() -> Vec<Source>,
    pub make: fn(allow_auth: bool) -> Box<dyn ModelProvider>,
    /// 该 provider 的客户端库读哪些环境变量（endpoint / cache / offline / token）
    pub env: ProviderEnvSchema,
}
pub static MODEL_PROVIDERS: &[&ProviderSchema] = &[&HUGGINGFACE, &MODELSCOPE, &HTTP, &LOCAL];
```

`ProviderId` 保留为 newtype（`&'static str` 或索引）以维持序列化兼容（`#[serde(rename = "huggingface")]` 等价于表里的 `id`）。

新增两个 provider：
- **`local:`**：把本机已有目录导入为快照（用户已经下过一堆权重，不想重下）。这是 ComfyUI 用户最常见的起点。
- **`http:`**：单文件直链 + 必填 sha256（与工具侧 `http:` backend 的 fail-closed 策略一致）。

**同一个警告适用**：新增 provider 时，其目录名必须能从「视图/清单扫描」的那张表到达，否则会重演 `is_dynamic_install_directory` 漏名字的事故。

### 6.6 镜像与源的统一

1. `default_sources(HuggingFace)` 增加内置候选：
   ```rust
   vec![
       Source::official("official", "https://huggingface.co"),
       { let mut m = Source::mirror("hf-mirror", "https://hf-mirror.com", 10);
         m.forward_credentials = false; m },
   ]
   ```
   `priority = 10` 让它默认排在官方之后，由测速决定实际顺序。`forward_credentials = false` 是必须的：镜像不该拿到用户的 HF token。

2. `canonical_provider_endpoint` 因此天然把它折叠为官方 —— 无需改逻辑，只需该端点进入 `default_sources` 的返回值。**这一步要配一个会失败的测试**：删掉内置镜像条目后，「经镜像 pull 的 lock 仍记录官方端点」的测试要变红。

3. 修复 §5.6 的测速预算问题，否则新增镜像对排名毫无影响。

4. `MODEL_ENDPOINT`（llama.cpp）纳入 `model env` 导出。

5. **不做**：不引入 osdk 自己的模型代理/缓存服务器。与 Android SDK 那条约束同理（`android/mod.rs:11-16`）——osdk 直接从上游取，不再分发。

### 6.7 消除下载缓存的双份拷贝

`pull.rs:137` 的 `download_path` 把文件落到 `<cache>/downloads/models/...`，随后 `publish` 再 `cas.ingest_preserve`。改法：

- 下载完成、校验通过后，**直接 ingest 进 CAS 并把下载目录里的那份替换为指向 CAS 对象的硬链接**（同卷时零成本），或直接删除。
- 断点续传仍在 `<cache>/downloads` 里进行（那里是半成品，本来就不该进 CAS）。
- 相应地，`cache clean` 与 `gc_roots` 的职责边界变清晰：半成品归 `cache clean`，完成品全部由 CAS GC 管理。

**验证方式**：同一个 `probe_osdk_model.ps1` 再跑一次，断言 `DOWNLOAD_CACHE_FILES` 中每个完成文件的 `fsutil hardlink list` 计数 ≥ 2，或该文件已不存在；且 `CAS_BYTES` 不因此翻倍。注入验证：把 ingest 后的替换步骤去掉，该断言必须变红。

---

## 7. 「install 后直接能跑」的可行性判断

### 7.1 拆成三件事

| 组成 | osdk 今天能否做 | 说明 |
| --- | --- | --- |
| Python 解释器 | ✅ 已有 `python` backend | `[tools] python = "3.12"` |
| uv | ✅ 已有（pypi backend 内部依赖 uv；也可 `pypi:uv` 显式装） | Comfy Desktop 自己也带一个 67 MB 的 uv |
| PyPI 依赖（含 torch+CUDA） | ✅ 已有 `pypi:` backend + `UV_DEFAULT_INDEX` | 需要 extra-index（`download.pytorch.org/whl/cu128`），见 §7.3 |
| CUDA 工具链（nvcc 等） | ✅ 已有 `conda:cuda-toolkit` / `conda:cuda-nvcc`（`conda.rs` 已实测 win-64 需 `nvidia` channel） | 只有编译自定义算子时才需要 |
| 模型权重 | ⚠️ 有获取、缺交付 | 本文的主体 |
| **NVIDIA 显卡驱动** | ❌ **做不到，也不该做** | 内核态组件，需管理员与重启 |
| ComfyUI 本体与 custom nodes | ⚠️ 部分 | 见 §7.2 |

### 7.2 源码版 vs Desktop 版：明确推荐

**推荐：对 ComfyUI 使用 Desktop 版（standalone 变体），osdk 不接管其运行时，只接管模型。**

初稿在这里只并列了三条路径没有下结论。本轮拿到真实安装后，结论是清楚的——理由不是偏好，是 Desktop 装完后 `manifest.json` 里那份**钉死的清单**：

```json
{
  "id": "win-nvidia",  "version": "v0.34.0-env1",
  "comfyui_ref": "v0.34.0",  "comfyui_commit": "12d5279438bfefc058a269eae805ceab6047777f",
  "python_version": "3.13.12",
  "torch_version": "2.12.1+cu130",
  "torchvision_version": "0.27.1+cu130",
  "torchaudio_version": "2.11.0+cu130",
  "uv_version": "0.12.9",
  "vendor_requirements_content": "--extra-index-url https://download.pytorch.org/whl/cu130\ntorch==2.12.1+cu130\n…"
}
```

本机实跑 `torch.cuda.is_available() == True`（§5.8）。这是一个**上游已验证、版本全钉死、GPU 可用**的组合。

| 维度 | Desktop（standalone） | osdk 托管源码版 |
| --- | --- | --- |
| Python | 3.13.12，随包 | osdk `python` backend 装 |
| torch / CUDA | 2.12.1+cu130 钉死，**已验证 GPU 可用** | 需自行选 cu126/cu128/cu130 组合（§5.8 索引已核对，但组合正确性由项目自负） |
| ComfyUI 本体 | 钉到具体 commit，`autoUpdateComfyUI` 自动更新 | osdk 需按 `requirements.txt` 装应用环境——**当前 `pypi:` backend 不支持**（§7.3） |
| custom node 依赖 | `--enable-manager` + ComfyUI-Manager | osdk 无对应能力 |
| 首次成本 | 下载 2.3 GB 环境包 | 分别装 Python / torch / 依赖，网络与失败面更大 |
| osdk 的价值 | **模型权重的获取、去重、镜像、视图** | 同左，外加一份重复且更脆弱的运行时管理 |

**为什么不推荐 osdk 接管运行时**：那等于用 osdk 解析出的 Python + torch 去替换一个上游已经验证过的组合。收益是「统一管理」，代价是把一个 works-by-construction 的环境变成一个需要自己维持正确性的环境——而 torch/CUDA/Python ABI 的组合恰恰是最容易出错的一类。**这与 osdk 对 Android SDK 的既有立场一致**（`android/mod.rs`：osdk 不重新分发、不自己造一套），也与 `pypi:` backend「把解析交给 uv」的判断同源。

**张力必须说清楚（不回避）**：

1. **`autoUpdateComfyUI: true` 会自行更新 ComfyUI 本体**，osdk 无法锁定它。所以 osdk 的 `osdk.lock` 只能保证**模型**可复现，**不能**保证 ComfyUI 版本可复现。若用户需要后者，应在 Desktop 设置里关掉自动更新——这是 Desktop 的开关，不是 osdk 的。文档必须写明这条边界，否则「osdk 让它可复现」会是一句空话。
2. **Desktop 自带 uv 0.12.9，osdk 也管 uv**。两者各自独立、互不干扰（Desktop 的在 `standalone-env\uv.exe`，osdk 的在自己的 installs 下）。**不要**试图让它们共用一个 uv 缓存：Desktop 的环境是钉死的，osdk 改它的缓存位置只会制造难查的问题。
3. **模型下载有两条路**：ComfyUI-Manager / 前端也能下模型，会落到 `is_default` 标记的那个根（当前是用户的 `ComfyUI-Shared\models`）。osdk 的视图是**另一个根且只读**。这是好事——**两者不会互相污染**，osdk 管的那部分保持可校验，用户随手下的那部分留在它自己的地方。这也是 §6.1 里「不要给 osdk 视图加 `is_default`」的实际理由。

**什么时候才该考虑 osdk 托管源码版**：需要钉死 ComfyUI commit 做可复现实验、需要非 NVIDIA 后端（Desktop 变体是 `win-nvidia`）、或需要在 CI/无 GUI 环境里跑。这三种情况下走 §7.3 的 B 路径，且前置是给 `pypi:` backend 增加「应用环境」模式。

### 7.3 ComfyUI 本体的三条路径（供 §7.2 之外的场景参考）

- **A. 用户已装 Comfy Desktop（本机情形，推荐）**：osdk 不碰本体，只渲染视图并打印路径。**第一阶段只做这条。**
- **B. `github:comfyanonymous/ComfyUI` + `pypi:` 依赖**：osdk 完整托管。ComfyUI 没有发 release wheel，需要按源码树装 `requirements.txt`。osdk 的 `pypi:` backend 面向「装 CLI 工具到独立 venv」，**不面向「按 requirements.txt 装一个应用环境」**（`grep requirements.txt` 在 `crates/` 下 0 命中）。这是新能力，属较大改动（P3-3）。
- **C. `[tasks]` 编排**：用已有 task 机制把 clone / 装依赖 / 起服务写成任务。零新代码，但「install 后直接能跑」变成「install 后 `osdk run comfy` 能跑」。

### 7.4 CUDA / PyTorch 的具体可行组合（基于 §5.8 实测）

```toml
[tools]
python = "3.12"

[tools.torch]
backend = "pypi"
version = "2.9.1"
# PyTorch CUDA wheel 不在 PyPI 主索引上，需要额外索引
index = "https://download.pytorch.org/whl/cu128"
```

**注意事项（必须写进未来的用户文档）**：
- cu124 的 torch 停在 2.6.0；新项目用 cu126 / cu128 / cu130。
- Python 3.12（cp312）在 cu126/cu128/cu130 下都有 win_amd64 wheel（实测）。
- `+cu128` 这类 local version 标签在 URL 里以 `%2B` 转义出现（实测索引页两种写法都在）；解析器必须处理。
- **驱动版本**是硬前提，osdk 只能检测与报告，不能安装。建议 `osdk doctor` 增加一项：读 `nvidia-smi` 报告驱动版本与其支持的最高 CUDA runtime，与将要安装的 wheel 比对，不匹配时**在安装前**报错而不是让用户在运行期撞上。

### 7.5 一次 install 的目标形态

```toml
# osdk.toml —— 一个 ComfyUI 项目（Desktop 版已装，osdk 只管模型）
[models.flux-dev]
source = "hf:black-forest-labs/FLUX.1-dev@main"
include = ["*.safetensors", "*.json"]

[models.flux-dev.views.comfyui]
profile = "default"

[settings.models]
auto_view = true
```

```bash
osdk install                      # 拉模型 + 建视图
osdk model view path comfyui
# → <data>/views/comfyui/default/models
#   一次性贴进 Desktop 设置 → Storage → Add Shared Directory
```

注意这份 `osdk.toml` 里**没有 `[tools] python` 与 torch**——这正是 §7.2 的结论：运行时归 Desktop，osdk 只管模型。需要 osdk 托管运行时的场景（§7.2 末尾三种）才加回工具段，形如：

```toml
[tools]
python = "3.12"
[tools.torch]
backend = "pypi"
version = "2.9.1"
index = "https://download.pytorch.org/whl/cu128"
```

**`osdk install` 是否应该自动拉模型？** 现有文档明确写了「`osdk install` 刻意不代拉模型：权重太大」。这条判断在**没有声明**的时代是对的。有了 `[models]` 显式声明之后，用户写下那一段本身就是授权。**建议**：`install` 默认拉 `[models]` 中声明的模型，但
- 预估总字节数并在超过阈值（如 5 GiB）时要求确认（`-y` 跳过）；
- 提供 `--no-models` 与 `settings.models.auto_install = false`；
- `osdk install <tool>` 带显式 operand 时不拉模型（与现有「显式 operand 跳过 lock replay」的语义一致）。

---

## 8. 落地路线（按「立即可做」到「较大改动」排序）

### P0：低成本高收益，不改变任何现有语义

| # | 内容 | 验证方式（含注入验证） |
| --- | --- | --- |
| P0-1 | `<data>/models/<name>/current` 稳定入口（Windows junction / Unix symlink），`model path --stable` | 复跑 §5.1 探针换成真实 osdk 产出；注入：不重定向 current，断言必须变红 |
| P0-2 | `probe_timeout_ms` 纳入 `config set` 白名单；metadata 与 Range 分别计时 | 复跑 §5.6：预算调到 5000 后 `source test --model` 必须变为可达；注入：把两段合回一个预算，必须重新 unreachable |
| P0-3 | `default_sources(HuggingFace)` 增加内置 `hf-mirror`（不转发凭据） | 经镜像 pull 后 lock 中 `endpoint` 必须是官方；注入：删掉内置条目，该断言变红 |
| P0-4 | `model env` 导出 `LLAMA_CACHE` 与 `MODEL_ENDPOINT` | 断言变量出现且值正确；注入：拼错变量名必须被发现（用 llama.cpp 源码里的常量名做对照，不要在测试里复刻一份） |
| P0-5 | `osdk doctor` 增加「系统代理已配置但 osdk 未使用」诊断 | 复跑 §5.5 三段式；注入：关掉诊断，必须不再报告 |

P0 全部不涉及新目录布局，风险最低。

### P1：呈现层（本重构的核心）

| # | 内容 | 验证方式 |
| --- | --- | --- |
| P1-1 | `ModelView` 抽象 + `comfyui` / `hf-cache` / `gguf` 三个渲染器 | §5.2 / §5.4 / **§5.9 端到端**的探针改为跑真实 osdk 输出；变异 harness 原样复用 |
| P1-2 | `osdk model view {add,list,path,rebuild,remove,export,doctor}` | 每个子命令一条端到端；`doctor` 必须能报出「未归类文件」——注入一个类别未知的文件，必须被列出 |
| P1-3 | 类别推断（约定 + 内置映射表）与 fail-closed | 用 FLUX 的目录形状做 fixture；注入：把推断改成「兜底 checkpoints」，`checkpoints stays empty` 断言必须变红 |
| P1-4 | 视图文件只读 + `model verify` 覆盖视图 | 就地改写视图文件后 `verify` 必须失败；注入：去掉只读设置，改写必须成功（确认只读确实生效）。**只读不影响 ComfyUI 发现文件已由 §5.11 验证** |
| P1-5 | 跨卷退化策略（hardlink → junction → copy）与明确报告 | 在两个卷上各跑一次（本机 C/D/E 三卷可用）；断言退化路径被报告而不是静默 |
| P1-6 | GC roots 加入 views；`model remove` 先拆视图 | 删模型后 CAS 对象必须被回收；注入：不把 views 加进 roots，「删除后仍被引用」的断言必须变红 |
| P1-7 | **`comfyui` 渲染器预建全部 25 个类别目录，且不加 `is_default`** | 断言生成的视图根含 25 个子目录；断言 osdk 不写 `is_default`——注入 `is_default` 后，「用户原有根的同名文件仍然胜出」这条必须变红（§5.11 变异已证明该断言有牙） |

### P2：声明式 `[models]` 与一次 install

| # | 内容 | 验证方式 |
| --- | --- | --- |
| P2-1 | `ConfigFile` 增加 `models`；`[models]` 解析与合并（项目/全局层次与 `[tools]` 同构） | 层次合并测试；`when` 平台过滤测试 |
| P2-2 | trust 分类（`INSPECTED_TABLES` + `affects_tool_dispatch` 返回 false） | 复跑 §1.6 的对照实验，`with-models` 必须变为 exit=0；注入：从 `affects_tool_dispatch` 移除 `models`，必须重新 exit=1 |
| P2-3 | `LockedModel.views` 与 `merge_model` / `locked_models` 双向读写 | AGENTS.md 明确警告过「只写不读」：新增字段必须同时有读取路径的测试 |
| P2-4 | `osdk install` 拉取声明模型 + 建视图；`--no-models`；大小确认阈值 | 端到端；注入：把 install 的模型分支去掉，端到端必须变红 |
| P2-5 | `osdk model sync` 与视图联动（sync 后视图自动更新） | 改 `--include` 重新 sync，视图路径不变、内容更新 |

### P3：较大改动，单独立项

| # | 内容 | 说明 |
| --- | --- | --- |
| P3-1 | provider 注册表化（取代闭合 enum），新增 `local:` / `http:` provider | 与 `DYNAMIC_NAMESPACES` 同构；`local:` 对已有大量权重的用户价值最高 |
| P3-2 | 下载缓存与 CAS 合流（§6.7） | 涉及 `pipeline::download` 的落盘约定 |
| P3-3 | `pypi:` backend 的「应用环境」模式（requirements.txt / pyproject 驱动） | 是「osdk 托管 ComfyUI 本体」的前置；本身是独立课题 |
| P3-4 | `a1111` / `modelscope-cache` 渲染器；`ollama` 通过 `ollama create` 适配 | 优先级低于前三个消费者 |
| P3-5 | `osdk doctor` 的 CUDA 驱动 / runtime 兼容性检查 | 依赖 `nvidia-smi` 解析 |

---

## 9. 本文未做的核查（诚实清单）

按 AGENTS.md「一个你注意到的风险就是你欠下的一步」，以下都是本文**注意到但没有做**的验证，实现时必须补，不得当作已知：

1. ~~没有做真实 ComfyUI 的端到端。~~ **第二轮已完成**（§5.9）：真实 ComfyUI v0.34.0 + 自带 Python 3.13.12，三个类别全部在 `/object_info` 可见，含三路归因控制与反向控制。**仍未验证的部分**：① 浏览器前端的**视觉**渲染（本轮读的是 `/object_info` 的 combo 数据，它是下拉框的数据源，但没有真的去点开 UI 截图比对）；② custom node 对 `folder_paths` 的额外用法（本机未装任何 custom node）；③ **通过 Desktop UI 把视图加进 `modelsDirs` 的那一步没有实操**——本轮走的是与 Desktop 等价的 `--extra-model-paths-config` 机制，因为实操会改用户的 `settings.json`（§3.2.3 已论证不该写）。换言之「机制可行」已验证，「Desktop 的 Add Shared Directory 按钮接受这个目录」未验证。
2. **没有验证 llama.cpp / sd-webui / ModelScope 客户端对硬链接的接受度**。三处均标为「推测」。
3. **没有验证旧版 osdk 读取含 `views` 字段的新 lock**。§6.3 的兼容性结论来自源码阅读（未见 `deny_unknown_fields`），未做二进制实测。
4. **没有验证 Ollama 的 `ollama create` 路径**。本机未安装 Ollama。磁盘多一份拷贝的结论来自其 blob store 设计，属合理推断而非实测。
5. ~~没有实际安装 torch+CUDA。~~ **第二轮已实跑**（§5.8）：`torch 2.12.1+cu130`、`cuda_available True`、RTX 4080 Laptop / 驱动 610.47。**但这是 Desktop 自带的组合**；osdk 自己经 `pypi:` + extra-index 装出 cu126/cu128 的路径**仍未实测**。
6. **没有测量重构对二进制体积与 `hook-env` 延迟的影响**。渲染器与 provider 注册表都会进 `osdk-cli`；`model env` 的新增变量会进 `hook-env` 热路径。AGENTS.md 要求两者都实测，本轮未做（尚无代码可测）。**实现时必须按 AGENTS.md 的命令分两次构建实测，并跑 `cargo bench -p osdk-core`。**
7. ~~没有核查视图目录是否会被 `inventory` 扫描误当作工具安装。~~ **已补查（§9.1）**，结论是不会，但仍需在实现时加一条守护测试。
8. **`hf-mirror.com` 的 308 回跳只测了两个路径**（`/api/models/<id>` 与 `/api/models/<id>/revision/<rev>`），未穷举 `resolve` 路径在带 Range 时的行为。
9. **没有验证 Desktop 的 `autoUpdateComfyUI` 更新后视图是否仍然有效**（§7.2 张力 1）。推测有效——视图是外部目录，与 ComfyUI 版本无关；但 `MODEL_FOLDER_TYPES` 若在新版增删类别，osdk 的类别表需要跟进。**这是一条会随时间失效的断言，实现时应加版本探测而非硬编码。**
10. **没有测试多个 osdk 视图根同时挂载**。§5.11 只测了「用户根 + 一个 osdk 根」。两个 osdk 视图（例如 comfyui 与 a1111 profile 各一）并存时的同名冲突未验证。

### 9.1 补查：`<data>/views/` 不会被 inventory 扫描（已核查）

`grep` 全仓库 `scan_installs(` / `scan_installs_for_tool(` 共 6 处生产调用点，扫描根**全部**是 `dirs.installs`（`ctx.dirs.installs` 或 `dirs.installs`）：

```text
lockfile.rs:1365      &dirs.installs
lockfile.rs:1427      &dirs.installs
commands.rs:1171      &app.ctx.dirs.installs  (scan_installs_for_tool)
cargo_package.rs:285  &ctx.dirs.installs
github.rs:966         &ctx.dirs.installs      (scan_installs_for_tool)
github.rs:1101        &ctx.dirs.installs
```

`dirs.views()` 会是 `<data>/views`，与 `dirs.installs` 平级而非其子目录（现有 `<data>/models` 同理，今天也不被扫描）。所以视图目录天然在扫描范围之外，**不会**重演 `is_dynamic_install_directory` 那族坑。

**但这不等于可以不写测试。** 这个结论依赖「扫描根永远是 installs」这一事实，而它没有任何东西在守护——将来某处改成从 `dirs.data` 起扫，视图里成千上万个硬链接会瞬间进入每次 `reshim` 的遍历量。实现时应加一条断言：视图目录下放一个诱饵 manifest，`scan_installs(&dirs.installs, ...)` 必须扫不到它。注入验证：把扫描根改成 `dirs.data`，该断言必须变红。


---

## 10. 与 AGENTS.md 硬约束的对账

| AGENTS.md 要求 | 本设计的对应 |
| --- | --- |
| 二进制体积：改动超 10% 要解释 | 渲染器是纯文件系统逻辑，无新重依赖；provider 注册表是静态表。**但必须实测**（§9-6）。视图/渲染代码应在 `install` feature 之后——shim 从不渲染视图 |
| `Backend` trait 新增方法的代价 | 本设计不动 `Backend` trait；`ModelProvider` 是独立 trait，已在 `install` 侧 |
| 交互延迟：`hook-env` 每个提示符执行 | `model env` 新增两个变量，不增加文件系统扫描；`configured_env` 仍是纯计算。**须跑 `cargo bench -p osdk-core` 对照** |
| 扫描深度与 `is_dynamic_install_directory` | `<data>/views/` 与 `dirs.installs` 平级，6 处扫描调用点的根全部是 `dirs.installs`（§9.1 已核查），因此不在扫描范围内；实现时仍需加守护测试 |
| 跨平台路径：分隔符由数据来源决定 | `map` 的 key、视图 manifest 的相对路径一律归一为 `/`；读取侧用 `split(['/', '\\'])` 而非 `Path::components()`。视图根这类「本机位置」保留原生分隔符 |
| 验证失效模式 | 本轮每个探针都配变异 harness，四组共 10 个变异全部按预期（9 个被捕获 + 1 个显式标注 EXPECTED-SURVIVE）；§8 每一步都写明注入验证。另记录了三次**自身**的验证缺陷：空断言（§5.1）、错误的期望（§5.11）、错误的测量工具（§5.10） |
| 权威任务清单是 `osdk task list` | 本文未新增 CI 检查；若实现时新增，须同时进 `osdk.toml` 的 `[tasks]` |
| 文档同步 | 见 §11 |
| Windows 用 pwsh 7 | 本轮所有 PowerShell 脚本均以 `pwsh -NoProfile -File` 执行（`C:\Program Files\PowerShell\7\pwsh.exe`，7.6.6.0） |

---

## 11. 实现时会牵动的文档

本轮只交设计，不改这些；但实现每一步都必须同步：

| 文件 | 变更 |
| --- | --- |
| `README.md` / `README.zh-CN.md` | 只加用法：`osdk model view` 的一句话与一个例子。架构不进 README |
| `site/guide/models.md` / `site/en/guide/models.md` | 新增「消费者视图」章节；更新 `model path --stable`、`model env` 的变量表、镜像章节（内置 hf-mirror 与折叠规则）；修正「镜像折叠」的表述让 `source add` 的情形不再误导；**新增 ComfyUI Desktop 对接说明：`modelsDirs` 一次性添加，osdk 不代写 `settings.json`，以及「osdk 管模型、Desktop 管运行时」的分工与 `autoUpdateComfyUI` 的边界** |
| `site/guide/implementation/backends-models.md`（及 en） | 三层架构、视图渲染、类别推断与 fail-closed、链接选型（junction vs symlink 的权限差异）、下载缓存与 CAS 合流 |
| `site/guide/lockfiles.md`（及 en，若存在） | `[models.<name>.views]` 的 schema |
| `site/guide/sources-security.md`（及 en，若存在） | 内置镜像不转发凭据；跨 host 重定向剥离 Authorization 的说明 |
| `site/.vitepress/config.mts` | 若新增页面则同步两种语言的侧边栏，并跑生产构建让失效链接暴露 |
| 仓库 `AGENTS.md` | 若实现中发现新的失效模式（例如视图与 inventory 扫描的交互），按既有体例补记 |

---

## 附录 A：实测环境与命令

**环境**：Windows x64；PowerShell 7.6.6.0（`C:\Program Files\PowerShell\7\pwsh.exe`，全部以 `-NoProfile` 执行）；Python 3.11.0（`C:\Python311\python.exe`）用于离线复现探针；隔离 venv 内 `huggingface_hub 1.32.0`；**真实 ComfyUI 端到端用安装自带的 Python 3.13.12**；osdk 为仓库内 `target\release\osdk.exe`；系统代理 `127.0.0.1:7897`（WinINET，`ProxyEnable=1`）。GPU：RTX 4080 Laptop，驱动 610.47。卷：C/D/E 三个 NTFS。开发者模式已开启（`AllowDevelopmentWithoutDevLicense=1`）——这一点影响符号链接结论，见 §5.3。

**被观测的 ComfyUI Desktop**：程序在 `E:\comfyui\Comfy Desktop`，数据在 `E:\Comfy-Desktop`（三根分离），配置在 `%APPDATA%\Comfy Desktop`。安装 `inst-1789929692790`，standalone / win-nvidia / v0.34.0-env1，环境包 2,365,760,769 B，本轮从 `installing` 观测到 `installed`（03:07:26 完成）。

**探针清单**（均在 `%TEMP%\osdk-model-probe-20260921\`）：

| 文件 | 作用 | 变异 harness |
| --- | --- | --- |
| `probe_comfy.py` | ComfyUI 穿 junction 读快照、类别隔离、元数据不污染 | `probe_comfy_mutations.py`（3 个变异） |
| `probe_hf_cache.py` | 手工构造 HF 缓存布局，真实客户端离线解析 | `probe_hf_cache_mutations.py`（3 个变异） |
| `probe_view.py` | 渲染消费者视图 + 硬链接共享字节 | `probe_view_mutations.py`（2 个变异） |
| `probe_multiroot.py` | **多根语义**：追加根是否平级、同名冲突、类别是否必须齐全、只读可读性、`is_default` 的作用 | `probe_multiroot_mutations.py`（1 捕获 + 1 预期存活） |
| `e2e_comfy.py` | **真实 ComfyUI 端到端**，含三路归因与反向控制 | 三路归因即内建对照 |
| `build_view.py` / `gen_desktop_yaml.py` | 渲染 osdk 视图 / 复刻 Desktop `buildYaml()` | — |
| `compare_yaml.py` | 复刻的 YAML 与 Desktop **真实生成**的文件逐项比对 | 含「杜撰 key」反向控制 |
| `fingerprint_desktop.ps1` | 端到端前后对用户环境取指纹，用于证明已还原 | 前后 diff 即验证 |
| `watch_install.ps1` | 监控 Desktop 安装进度（**已修正错误的大小读法**，见 §5.10） | — |
| `probe_filesize_truth.ps1` | 判定哪种 API 在并发写入时报告真实文件大小 | 四种读法互为对照 |
| `probe_links.ps1` | hardlink / junction / symlink / 跨卷 | 无（每条断言自带反向控制） |
| `probe_osdk_model.ps1` | 真实 `osdk model pull` 的产物解剖 | 无 |
| `probe_proxy.ps1` | 三段式对照：.NET vs osdk 无代理 vs osdk 带代理 | 三段本身即对照 |
| `probe_mirror_source.ps1` | 镜像源 add/pin/pull 与 lock 记录 | 无 |
| `probe_source_timeout.ps1` / `probe_source_budget.ps1` | 测速失败的归因 | 分腿测量即归因 |
| `probe_revision_api.ps1` / `probe_mirror.ps1` | 端点可达性与 308 回跳 | 无 |
| `probe_torch_index.ps1` | PyTorch CUDA wheel 矩阵 | 无 |
| `probe_desktop_asset.ps1` | Desktop 环境包直连可达性 | 有/无代理两段对照 |
| `extract_asar_strings.py` / `dump_asar_region.py` | 从本机 Comfy Desktop bundle 提取其模型路径实现 | 只读 |

**对用户 ComfyUI 环境的影响与还原（已验证）**：端到端测试**未写** `settings.json`（走与 Desktop 相同的 `--extra-model-paths-config`）。前后指纹比对：`settings.json` SHA-256 一致、`modelsDirs` 不变、shared models 0 文件不变、install models 36 文件不变、`E:\Comfy-Desktop` 下 `osdk-e2e*` 残留为 0。唯一变化的 `installations.json` 已逐行 diff 并归因为 Desktop 自身的保存周期（新增 `lastLaunchedAt`，来自它在 03:07:06 自动启动的实例），本轮脚本对该文件只读不写。

**清理**：探针目录保留 20 个可复用脚本（共约 76 KB，`%TEMP%\osdk-model-probe-20260921\`），一次性产物（venv、各次运行的临时根、变异副本）已删除；跨卷测试目录 `E:\osdk-model-probe-20260921-links` 已移除。用户真实数据根 `E:\osdk-data` 全程未被写入（所有 osdk 调用都显式设置了 `OSDK_DATA_DIR`/`OSDK_CACHE_DIR`/`OSDK_CONFIG_DIR`；核查：其 `cache`/`config`/`data` 三个子目录的 `LastWriteTime` 分别为 2026-09-20 18:24、2026-09-17 04:16、2026-09-13 21:05，均早于本轮）。

**可视化自检**：`check_visual.py` 对 `.visual.html` 做 19 项断言（锚点可达、唯一非空 `<title>`、8 类标签配平、根容器无 `100vh`/`height:100%`、**HTML 中引用的每个实测数字都能在本 md 中找到**、无占位数字、torch wheel 计数逐行比对），全部通过。其中「根容器高度」一项第一版写得过宽（把固定高度进度条内部的 `height:100%` 也算违规），收窄后<b>另加了一条反向控制</b>：向 `.fig` 注入 `height:100%` 必须被该检查捕获——否则收窄就把它变成了一个什么都不测的检查。


## 附录 B：外部来源清单

- ComfyUI `folder_paths.py`：`https://raw.githubusercontent.com/comfyanonymous/ComfyUI/master/folder_paths.py`
- ComfyUI `utils/extra_config.py`：`https://raw.githubusercontent.com/comfyanonymous/ComfyUI/master/utils/extra_config.py`
- ComfyUI `extra_model_paths.yaml.example`：`https://raw.githubusercontent.com/comfyanonymous/ComfyUI/master/extra_model_paths.yaml.example`
- Comfy Desktop 数据位置：`https://docs.comfy.org/installation/desktop/usage/settings`
- Comfy Desktop 实现：本机 `E:\comfyui\Comfy Desktop\resources\app.asar`
- huggingface_hub 常量：`https://raw.githubusercontent.com/huggingface/huggingface_hub/main/src/huggingface_hub/constants.py`
- HF 缓存布局说明：`https://huggingface.co/docs/huggingface_hub/guides/manage-cache`
- llama.cpp `common/common.cpp`：`https://cdn.jsdelivr.net/gh/ggml-org/llama.cpp@master/common/common.cpp`
- llama.cpp CLI 参数：`https://raw.githubusercontent.com/ggml-org/llama.cpp/master/tools/cli/README.md`
- Ollama 模型位置：`https://docs.ollama.com/faq`
- Ollama 导入：`https://docs.ollama.com/import`、`https://docs.ollama.com/modelfile`
- vLLM 引擎参数：`https://docs.vllm.ai/en/latest/cli/bench/startup/`
- vLLM 与 HF 集成：`https://docs.vllm.ai/en/v0.6.6.post1/design/huggingface_integration.html`
- vLLM 缓存根：`https://docs.vllm.ai/en/stable/deployment/docker/`
- sd-webui 参数：`https://raw.githubusercontent.com/AUTOMATIC1111/stable-diffusion-webui/master/modules/cmd_args.py`
- ModelScope 下载与缓存：`https://modelscope.ai/docs/Models/Download-Model`
- ModelScope 缓存结构：`https://deepwiki.com/modelscope/modelscope/3-modelscope-hub`
- HF-Mirror：`https://beta.hf-mirror.com/`
- reqwest 重定向剥离敏感头：`https://docs.rs/reqwest/latest/reqwest/struct.ClientBuilder.html` 与 `redirect.rs` 的 `remove_sensitive_headers`
- PyTorch wheel 索引：`https://download.pytorch.org/whl/cu126/torch/` 等
