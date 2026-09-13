# Python 能力对齐 uv：镜像加速与缓存统一管理

日期：2026-09-13

项目：github.com/lejunyang/one-sdk

状态：**设计提案，尚未实现。** 上游事实核查基准日 2026-09-13（第三轮 pip 路径实测为 2026-09-14）；本机实测环境为 Windows x64、`osdk 0.0.2`（`E:\osdk-bin\osdk.exe`）、osdk 数据根 `E:\osdk-data`、被测 uv 版本 `0.12.13 (0ebbd9274 2026-09-10 x86_64-pc-windows-msvc)`、被测 mise 版本 `2026.9.6 windows-x64 (2026-09-12)`（源码固定在 commit `acbbdee0b150f5eeb14eb287198b11625ea35472`，即 tag `v2026.9.6`）。**第三轮实测的前提是本机 `uv` 未安装**（`Get-Command uv` → NOT FOUND），解释器为 Python 3.14.7。

本文标注四类事实来源，请按标注判读可信度：**【官方】** 上游官方文档或规范原文已明示；**【源码】** 上游仓库源码在指定 commit 的实际实现（附文件与行号）；**【一方/社区】** 镜像站自述、社区经验或第三方文档；**【实测】** 本次在上述环境实机测量，含测量方法。凡未能确证的一律标注「待验证」并给出验证方法，**没有任何估算被写成实测**。

> **修订记录**
>
> - **2026-09-13 第二轮**：追加 §4.5「旁证：mise 的同题选择」，并据其修正了能力矩阵第 9/11/25/27/38/43 项、§4.3 的约束表述、§6.1 与 §7.3 的相关论证、§7.4 验收清单（新增第 25–27 项）、§9（新增第 7/8 项待验证）、§10（新增第二轮实测子表）、以及 P0/P1/P4 阶段内容。其中**第 38/43 项从 P1 前移到 P0** 是唯一影响路线图结构的改动（依据 §4.5.6：实测 mise 并不纳管 uv 缓存，该能力是 osdk 的差异化且成本仅一行）。三档判定（已具备/部分具备/缺失）一项未变。
> - **2026-09-14 第三轮（结论反转）**：**§4.5.7 的推荐由「不提供 stdlib venv 回退」改为「提供且受管的回退」。** 依据本轮本机实测——原第 2 条理由「回退会静默绕过全部门禁」**不成立**：pip 侧存在等价的哈希/索引/离线门禁表面（`--require-hashes`、`PIP_INDEX_URL`、`PIP_NO_INDEX` 均已实测生效且 fail-closed）。同步改动：矩阵第 9/11 项、§6.7（拆分「系统 pip」与「venv 内 pip」两个被原文混同的概念）、新增 §7.1.1（pip 侧等价门禁清单）与 §7.2.1（pip 哈希验证 + 假阴性方法学教训）、§7.3（pip 无 `--index-strategy` 的后果与对策）、§7.4（改写第 7/26/27 项，新增第 28–34 项）、P1 阶段、§10（新增第三轮实测子表）。三档判定仍一项未变。**原判断的错因已在 §4.5.7 中留痕，未删除。**

---

## 执行摘要

**一、推荐方案是把 uv 作为 osdk 管理的受管子进程后端，而不是在 osdk 内自研 Python 包管理器。** 这不是「省事」的选择，而是被三组实测数字迫使的选择：uv 的 Windows x64 二进制解压后 **39.63 MB**（实测），是 `osdk` 整体 **12.28 MB** 的 3.2 倍；其中包含一个完整的 PubGrub 求解器、PEP 517 构建后端、平台标签模型与 wheel 安装器。自研意味着 osdk 体积至少翻两倍，而 AGENTS.md 规定单个二进制增长超过 10% 就要给理由。更关键的是**正确性不对称**：依赖求解的错误不是性能问题而是装错包，而 uv 已在真实生态里被验证。详见 §4。

**二、osdk 的 Python 现状是「只有解释器层，没有包层」，且这个判定逐条来自读码。** `crates/osdk-core/src/backend/python.rs` 完整实现了解释器的下载、校验、多源镜像与别名，但整个 workspace 里搜索 `venv|pyproject|requirements.txt|uv` 只在 `cache/mod.rs:40` 命中一个 `PIP_CACHE_DIR` 重定向——**没有任何 UV_* 变量**。也就是说 osdk 今天已经在统一 pip 的缓存，却没有统一 uv 的缓存，而 uv 正是用户实际会用的那个。详见 §3。

**三、国内 PyPI 镜像的真实瓶颈不是「制品下载」而是「索引元数据」，且两者的安全性质完全不同。** 实测八个镜像对 `numpy`/`pandas` 共 6,625 个文件条目做逐文件 sha256 比对，**mismatch 为 0、缺失为 0、最新版本完全一致**——制品侧哈希天然可校验，镜像不构成信任扩大。但索引侧差异巨大：只有 4/8 个镜像支持 PEP 691 JSON API，**没有任何一个国内镜像在 JSON 响应里提供 PEP 658 `core-metadata`**（pypi.org 提供）。而延迟差异达 29 倍（bfsu 82 ms vs aliyun 2420 ms）。详见 §5。

**四、缓存统一管理的正确落点是「让 uv 的缓存目录归 osdk 管」，而不是让 osdk 复制一份 CAS。** osdk 已有的 `store/`（blake3 CAS + hardlink/reflink/copy）服务的是「SDK 归档解包后的文件」，而 uv 的 `archive-v0/` 服务的是「wheel 解包后的 site-packages 树」。实测确认 uv 已在做正确的事：同卷下三个 venv 的 `cacert.pem` 与缓存对象是**同一个 inode**（`fsutil hardlink list` 返回 4 条路径、`LinkType=HardLink`）。重复实现只会得到两个都不完整的 CAS。详见 §6。

**五、第二轮用 mise 检验后，推荐方案不变，且得到四处印证、一处反例、三处修正。** mise `2026.9.6` 在求解、wheel 安装、venv 创建、Python 工具安装四个环节**全部委托**，自研为零——即便它自己的二进制已达 98.52 MB（实测）。它的 `mise.lock` 对 Python 依赖图零认知，只把 `uv.lock` 当项目标记，印证了 §4.4 的 lock 分层。**反例在索引**：mise 设 `UV_INDEX`（额外索引，优先级高于默认索引）而非 `UV_DEFAULT_INDEX`，正是 §5.6 论证要避免的依赖混淆形状，因此 §7.3 规则 1 升格为必须有测试守护的硬约束。**两处 osdk 明确领先**：解释器镜像上 mise 把 PBS URL 硬编码为 github.com、且为 Go 和 Zig 做了镜像却没为 Python 做；缓存上 mise 完全不纳管 `UV_CACHE_DIR`（实测其 `cache/uv/` 只有自己的元数据），使 P0 从「补齐」变成「差异化」。详见 §4.5。

**六、第三轮实测推翻了第二轮的一处推荐：stdlib venv 回退应当提供，但必须受管。** 第二轮以「回退会静默绕过全部门禁」为主要理由主张 uv 缺失时 fail-closed，本轮实测证明该理由**不成立**——pip 有完整的等价门禁表面：`--require-hashes` 同样 fail-closed（错误哈希 exit=1 并列出 Expected/Got）、`PIP_INDEX_URL` 可由 osdk 注入并已验证生效、`PIP_NO_INDEX=1` 可强制离线。错因是把「uv 的参数表面」当成门禁的唯一载体。而因为**本机当前 uv 未安装**，原设计等于让 Python 功能在默认环境下直接不可用。修正后的方案是「默认自动回退 + 显式告知能力差异 + 仅 `--require-uv` 时 fail-closed」，并新增 creator 记录与分派、`--seed` 语义归一化、`--relocatable` 在 pip 路径报错。同时澄清了一个被原论证混同的概念：禁止的是**系统 pip**（污染全局），而 **osdk 创建的 venv 内自带的 pip** 是隔离且可受门禁约束的。另有一条方法学教训：本轮首次测哈希门禁时因复用 venv 拿到 exit=0 的假阴性，与 AGENTS.md 记录的「12 ms 假结果」同类。详见 §4.5.7、§6.7、§7.1.1、§7.2.1。

---

## 1. 问题陈述与非目标

### 1.1 问题

osdk 今天能把 CPython 装好、切换好、shim 好，但用户拿到解释器之后要做的每一件事都得离开 osdk：建虚拟环境、装依赖、锁定版本、跑工具。这带来两个具体后果：

第一，**镜像加速在 Python 上是断裂的**。osdk 为解释器下载提供了三档源与自动测速（§3.1），但用户 `pip install` 时走的是 pip 自己的默认源，osdk 的镜像策略完全不生效。用户的长期偏好是「源与配置一律通过 osdk 命令管理」，而现状强迫他去手改 `pip.conf`。

第二，**缓存统一在 Python 上是半截的**。`cache/mod.rs` 重定向了 `PIP_CACHE_DIR`，但没有 `UV_CACHE_DIR`。用 uv 的用户得到的是「osdk 声称统一了缓存，实际上我的缓存在 `%LOCALAPPDATA%\uv\cache`」。

### 1.2 明确的非目标

以下几项**显式列为非目标**，理由随项列出，不在路线图任何阶段中出现：

| 非目标 | 理由 |
| --- | --- |
| 在 osdk 内自研依赖求解器 | §4 详述。求解错误是正确性问题，不是性能问题；uv 的 PubGrub 实现已被生态验证 |
| 自研 wheel 安装器与 `RECORD` 处理 | 同上。且涉及 PEP 427/376 的大量边缘情形 |
| 自研 PEP 517 构建前端与构建隔离 | 需要维护 sdist 构建、build backend 交互、build 依赖求解三条路径 |
| 复刻 `uv pip` 的全部 pip 兼容细节 | uv 官方文档自己列了 24 条与 pip 的差异【官方】，逐条复刻等于承接 pip 的历史包袱 |
| 在 osdk 中重新实现一份 wheel 级 CAS | §6.2 详述。与 uv 的 `archive-v0` 语义重叠且必然更差 |
| 代理 `uv publish` / `uv build` 的上传凭据 | 发布是低频、强凭据操作，osdk 介入只增加凭据暴露面而无加速收益 |
| 让 osdk 读写 `uv.toml` / `pip.conf` | 违背用户偏好，且 uv 官方明确拒绝读 pip 配置的五条理由同样适用于反向【官方】 |
| 支持 conda 生态的 Python 包（非解释器） | osdk 已有独立的 `conda:` backend（`backend/conda.rs`），两条路径不应混合 |

---

## 2. 外部事实基线：uv 能力逐项穷举

本节按 uv 的实际命令表面穷举，不依赖印象。命令清单来自**本机 `uv --help` 实测**，语义来自官方文档。

### 2.1 顶层命令全集【实测】

`uv 0.12.13 --help` 输出的顶层子命令共 22 个：

```
auth  run  init  add  remove  version  sync  lock  export  tree  format
check  audit  tool  python  pip  venv  build  publish  workspace  cache  self
```

值得注意的是，其中 `auth`、`format`、`check`、`audit`、`workspace`、`export` 六个在多数中文资料里都没有提及——**`uv audit`（OSV 漏洞审计，支持 `--output-format sarif`）与 `uv auth`（凭据管理，含 `login`/`logout`/`token`/`dir`）是较新的表面**，做能力对齐时不能漏。

### 2.2 逐子命令展开【实测】

| 命令族 | 子命令 |
| --- | --- |
| `uv python` | `list` / `install` / `upgrade` / `find` / `pin` / `dir` / `uninstall` / `update-shell` |
| `uv pip` | `compile` / `sync` / `install` / `uninstall` / `freeze` / `list` / `show` / `tree` / `check` |
| `uv tool` | `run` / `install` / `upgrade` / `list` / `audit` / `uninstall` / `update-shell` / `dir` |
| `uv cache` | `clean` / `prune` / `dir` / `size`（`size` 标注为 experimental，需 `--preview-features cache-size`） |
| `uv workspace` | `metadata` / `dir` / `list` |
| `uv auth` | `login` / `logout` / `token` / `dir` |
| `uv self` | `update` / `version` |

### 2.3 关键语义要点

**解释器管理**。uv 从 Astral 的 python-build-standalone 下载托管 CPython；`UV_PYTHON_INSTALL_MIRROR` 用于替换 `https://github.com/astral-sh/python-build-standalone/releases/download` 前缀，`UV_PYPY_INSTALL_MIRROR` 替换 `https://downloads.python.org/pypy`【官方，https://docs.astral.sh/uv/reference/environment/】。本机实测目录：`uv python dir` = `C:\Users\LJY\AppData\Roaming\uv\python`，`uv tool dir` = `...\uv\tools`，`uv cache dir` = `C:\Users\LJY\AppData\Local\uv\cache`。

**pip 兼容层的差异**。uv 官方专文列出与 pip 的差异【官方，https://docs.astral.sh/uv/pip/compatibility/】，其中对本提案有直接影响的：

- **不读 `pip.conf` / `PIP_INDEX_URL`**，只读自己的 `UV_*` 与 `uv.toml`。官方给了五条理由（需 bug-for-bug 兼容、上游格式变更锁定、版本歧义、阻碍新增设置、用户困惑）。这条**直接决定了 osdk 不能靠已有的 `PIP_CACHE_DIR` 机制覆盖 uv**。
- **默认要求虚拟环境**：`uv pip install` 会装进当前激活的 venv，或向上查找名为 `.venv` 的目录；装进系统 Python 必须显式 `--system` 或 `--python /path/to/python`。这与 pip「无 venv 时装进全局」相反。
- **默认不做字节码编译**，需 `--compile-bytecode` 或 `UV_COMPILE_BYTECODE=1`。
- **默认 PEP 517 构建隔离**，逃生舱是 `--no-build-isolation`。
- **`--constraint` 不作用于构建依赖**，构建约束要用 `--build-constraint` / `UV_BUILD_CONSTRAINT`。
- **`requires-python` 只看下界，忽略上界**（`>=3.8,<4` 视作 `>=3.8`）。
- **不支持 `--user`**；`--keyring-provider` 只支持 `subprocess`，不支持 pip 的 `auto`/`import`；且**不等到 401 才附凭据**，对任何有凭据的主机都直接附上。
- **默认拒绝文件名与内部元数据不一致的 wheel**（逃生舱 `UV_SKIP_WHEEL_FILENAME_CHECK=1`），比 pip 严格。
- 包名一律按 PEP 503 规范化后输出。

**索引策略与依赖混淆**。uv 默认 `--index-strategy first-index`：跨索引查找但只取「第一个含该包的索引」的候选版本集，且 `--extra-index-url` 优先于默认索引。官方明确说明这是为防依赖混淆攻击，并点名 2022 年 12 月的 `torchtriton` 事件【官方】。另两档 `unsafe-first-match` 与 `unsafe-best-match` 名字里带 `unsafe` 就是警告，`unsafe-best-match` 最接近 pip 行为也最危险。uv 另支持把包钉到专属索引（`[[tool.uv.index]]` + `explicit = true`）。

**锁定与可复现**。`uv.lock` 是跨平台的 universal resolution【官方，https://docs.astral.sh/uv/concepts/resolution/】。相关开关：`--frozen`（不重新锁定，直接用现有 lock）、`--locked`（断言 lock 不会变，会变则失败）、`--offline`（禁网）、`--exclude-newer`（排除某时间点后发布的版本）、`--require-hashes` / `--no-verify-hashes`。`uv export` 可导出为 `requirements.txt` / `pylock.toml` / `cyclonedx1.5`（实测 `--format` 三个可选值）。

**缓存与链接模式**。`--link-mode` 四档：`clone`（reflink/CoW）/ `copy` / `hardlink` / `symlink`，环境变量 `UV_LINK_MODE`【实测 `uv venv --help`】。缓存目录内部按用途分版本化子目录（§6.1 有实测布局）。

**PEP 723 单文件脚本**、**workspace**、**build/publish**、**`uvx` = `uv tool run`**（官方明确二者完全等价【官方，https://docs.astral.sh/uv/concepts/tools/】）。`uvx <name>` 近似等于 `uv run --no-project --with <name> -- <name>`，差别是包名从命令名推断、临时环境缓存在专用位置、且已安装工具会被优先使用。

**许可**：uv 采用 Apache-2.0 或 MIT 双许可，由使用者选择【一方，https://uv.doczh.com/reference/policies/license/ 及仓库 LICENSE-APACHE / LICENSE-MIT】。这对「作为受管二进制分发」是宽松许可，无 copyleft 传染风险。

---

## 3. osdk 现状逐条判定

**判定方法**：全部来自本次读码，每条给出文件与符号。判定分三档——**已具备**（osdk 自身已实现等价能力）、**部分具备**（有可复用基础设施但未接到 Python）、**缺失**（无对应实现）。

### 3.1 解释器层：已具备

`crates/osdk-core/src/backend/python.rs` 实现 `Backend` trait：

- `default_sources()`（python.rs:67）返回三档源：`astral` = `https://releases.astral.sh/github/python-build-standalone/releases/download`（Official，`with_index("https://releases.astral.sh")`）、`gh-proxy` = `https://gh-proxy.com/https://github.com/...`（Mirror, priority 10）、`github` = `https://github.com/astral-sh/...`（Mirror, priority 20）。
- `probe_url()`（python.rs:89）用 `SHA256SUMS` 作探测目标，因此测速与真实下载路径同源。
- `fetch_catalog_for_tag()`（python.rs:363）逐源尝试拉取该 release 的 `SHA256SUMS`；`parse_sha256sums()`（python.rs:506）解析 `<hex>  <filename>`。注释（python.rs:284）明确「Checksum comes straight from SHA256SUMS — no extra request」，即**校验和与资产选择共用一次请求，不依赖 GitHub API**。
- `asset_matches()`（python.rs:47）默认排除 `freethreaded` 变体；`bin_names()`（python.rs:330-340）识别 `+freethreaded` 后缀并给出 `python3t`；`select_installed()`（python.rs:21）支持显式请求变体。
- `version_available_on_platform()`（python.rs:447）+ `minimum_for_minor()`（python.rs:498）处理平台可用性下界。
- 版本→release tag 索引在 `backend/python_releases.rs`：`RELEASES`（:7）与 `tag_for()`（:108），模块注释说明由 uv 的下载元数据生成。
- 可覆盖的目录/变体目录在 `backend/python_catalog.rs`；配置侧 `config/mod.rs:170` 的 `PythonSettings { catalog_url, catalog_sha256 }`——**注意 catalog 要求同时给 SHA-256**，这是既有的 fail-closed 设计先例。

CLI 侧 `osdk python find` 已实测可用，输出区分 `managed` / `path` / `system` 三类来源。shim 侧 `crates/osdk-shim/src/main.rs` 对 python backend 有专门的 `select_installed` 分支。

### 3.2 通用基础设施：可直接复用（判为「部分具备」的依据）

这些是本提案的地基，**不应重新设计**：

| 基础设施 | 位置与符号 | 与本提案的关系 |
| --- | --- | --- |
| 多源模型 | `source/mod.rs`：`Source`（:28）、`SourceKind{Official,Mirror,Custom}`（:17）、`Source::official/mirror/with_index`（:73/:86/:99）、`candidate_fingerprint()`（:110，header 值经 blake3 哈希后入指纹，secret-safe） | PyPI 索引镜像应建模为 `Source`，而非新造一套 |
| 源选择与测速 | `source/select.rs`：`effective_sources*()`（:56/:79/:90）、`active_source()`（:113）、`ranked_source_candidates*()`（:136/:154/:172）、`probe_all*()`（:284/:290/:312）、`refresh*()`（:424/:440/:459） | 索引测速直接走这里；`syspkg/mirror.rs` 的模块注释（:7-13）明确说过「不要另建第二套探测机制，那正是 drift」 |
| 环境镜像变量 | `source/env.rs`：`SourceMode`（:37）、`validate_https_endpoint()`（:85）、`EnvMirror`（:117）、`read_env_source()`（:151）、`apply_env_source()`（:200） | `UV_DEFAULT_INDEX` / `UV_INDEX` 应作为 candidate 参与排序，而非无条件服从——这个取舍模块注释（:10-22）已论证过 |
| 下游缓存重定向 | `cache/mod.rs`：`downstream_root()`（:17，`<cache>/pkg`）、`cache_env()`（:23）、`manager_env()`（:58）、`manager_exec_env()`（:77）、`describe()`（:86）、`variable_is_available()`（:81） | **这是 uv 缓存统一的唯一正确落点**。`variable_is_available` 的语义是「用户已设的值不覆盖，除非是上一次 osdk 自己设的（凭 `OSDK_ORIG_<key>_SET` 判断）」 |
| CAS 与链接模式 | `store/mod.rs`：`Cas`（:23）、`MaterializeReport`（:28）、`hash_file()`（:306，blake3）；`store/link.rs`：`LinkMode{Auto,Hardlink,Reflink,Copy}`（:19）、`same_filesystem()`（:62）、`materialize()`（:132）、`try_hardlink()`（:186）、`try_reflink()`（:192，`reflink_copy`） | 解释器安装继续用它；**wheel 层不复制这套**（§6.2） |
| 下载/解压/校验 | `pipeline/verify.rs`：`HashAlgo`（:12）、`hash_file()`（:53）、`verify_file()`（:105）、`find_shasum()`（:120）、`parse_sha256_token()`（:141）、`parse_sri()`（:153）、`verify_minisign()`（:224）、`trusted_key()`（:252）；另有 `hashing_crates_are_pinned_to_a_fast_opt_level()`（:320）守 profile | 索引与制品校验复用；`parse_sri` 已支持 SRI 形式 |
| 镜像「加速哪一半」的建模先例 | `syspkg/mirror.rs`：`Acceleration`（:47）、`MirrorCandidate`（:63）、`WINGET_MIRRORS`（:89）、`acceleration_of()`（:148） | 模块注释（:15-28）正是本提案 §5 的先例：winget 镜像只加速「找包」不加速「下包」，报告必须说清是哪一半。**PyPI 恰好相反，两半都能加速，但性质不同** |
| npm 注册表预检先例 | `package_registry.rs`：`NPMMIRROR`(:20)/`NPMJS`(:21)、`RegistryProbe`（:71）、`RegistryPlan`（:80）、`registry_env()`（:141）、`should_plan()`（:154）、`explicit_registry_env()`（:645）、`native_registry_candidates()`（:714） | 「一次性、匿名探测、只返回一个 env 覆盖给单个子进程」——这是 PyPI 索引选择应当照抄的形状 |
| 受管子进程工具的生命周期 | `backend/native_tool.rs`：`NativeToolLifecycle`、`NativeToolReceipt`（:47）、`NATIVE_TOOL_RECEIPT_FILE`（:25）、`LOCKED_NATIVE_RUNTIME_OPTION`（:26）、seal 机制（`NATIVE_TOOL_SEAL_SUFFIX` :31）；`backend/cargo_package.rs` / `go_package.rs` 是两个已落地实例 | **uv-as-backend 的直接模板**：identity 限定的锁与安装根、sibling staging、绑定 provider/runtime/产物字节的 receipt、fail-closed 复用 |
| 动态命名空间与扫描可见性 | `tool.rs`：`DYNAMIC_NAMESPACES`（:549，现为 npm/github/http/cargo/go/conda 六个）、`DERIVED_INSTALL_DIRECTORIES`（:576，现为 `npm-global`）、`is_dynamic_install_directory()`（:590）、`namespace_schema()`（:560） | 新增 Python 工具命名空间**必须**从这里可达，否则安装对 inventory 扫描隐形。注释（:586-589）已把这个坑写明 |
| 锁文件 | `osdk-cli/src/lockfile.rs`：`Lockfile{schema}`（:33）、`LockedTool`（:48）、`LockedNativeTool`（:63）、`LockedNativeLock`（:122）、`LockedNpmMetadata`（:110）、`LOCKFILE_NAME="osdk.lock"`（:19）、`MAX_LOCKFILE_BYTES`（:28） | `LockedNativeLock` + `LockedNpmMetadata` 已建立「osdk.lock 记录外部锁文件的 kind/format/sha256，而不复制其内容」的先例——`uv.lock` 应照此处理 |
| 全局设置 | `config/mod.rs`：`Settings`（:52）含 `link_mode`/`jobs`/`verify_signatures`/`require_checksums`/`attestations`/`offline`；`SourcesConfig`（:248）含 `selection`/`mode`/`probe_timeout_ms`/`cache_ttl`/`per_tool`/`registries`；`RegistriesConfig`（:299）**目前只有 `npm`**（:300） | `RegistriesConfig` 加 `python` 字段是最小侵入的配置扩展点 |
| 交互延迟基准 | `benches/interactive_latency.rs`：`measure_tree()`（:29）、`write_decoy_manifest()`（:267，诱饵 manifest 让裁剪失效时 fail-closed 扫描报错） | 路线图每阶段的验收都要跑它 |

### 3.3 包层：缺失

**判定依据**：在 `crates/` 全树对 `venv|virtualenv|pyproject|requirements\.txt|PIP_INDEX|pip install|uv\b|PYTHONPATH|VIRTUAL_ENV` 做检索，**命中总数 8 条，全部集中在 `cache/mod.rs`**（:40 的 `set_if_unset("PIP_CACHE_DIR", root.join("pip"))` 及其 7 条测试引用）。对 `resolvo|pubgrub|PubGrub` 检索**零命中**。对 `UV_` 前缀检索**零命中**。

因此：虚拟环境、依赖求解、lock、wheel 安装、PyPI 索引配置、Python 工具安装、PEP 723、build/publish 在 osdk 中**均无实现**，且**无 uv 集成**。

### 3.4 能力对齐矩阵

「是否列入目标」一列的取舍论证见 §4.3。

| # | uv 能力 | osdk 现状判定 | 判定依据（文件:符号） | 列入目标 |
| --- | --- | --- | --- | --- |
| 1 | `uv python list` / `install` / `uninstall` | **已具备** | `backend/python.rs:98 list_remote_versions`、`:176 install`；CLI `install`/`list-remote`/`uninstall` | 是（保持自有实现） |
| 2 | `uv python find` | **已具备** | CLI `osdk python find`（实测输出 managed/path/system 三类） | 是（已完成） |
| 3 | `uv python pin` | **已具备** | `config_edit.rs:27 set_project_tool`、`:14 set_global_tool`；`osdk use` | 是（已完成） |
| 4 | `uv python dir` | **已具备** | `dirs.rs:312 install_path` | 是（已完成） |
| 5 | `uv python upgrade` | **部分具备** | `osdk upgrade` 存在但语义是「按 lock 升级工具」，非「原地升 patch 保留 minor 固定」 | 是（P3，语义对齐） |
| 6 | `uv python update-shell` | **已具备** | `activate/mod.rs`、`osdk activate` / `deactivate` | 是（已完成） |
| 7 | 解释器镜像源（含 PBS 镜像） | **已具备且优于 uv** | `python.rs:67` 三档源 + `source/select.rs` 自动测速失效转移；uv 侧只有单个 `UV_PYTHON_INSTALL_MIRROR` 前缀替换 | 是（§5.4 补充镜像） |
| 8 | freethreaded 变体处理 | **已具备** | `python.rs:47/:340`、`:21 select_installed` | 是（已完成） |
| 9 | `uv venv`（创建虚拟环境） | **缺失** | 全树检索 `venv` 零命中（除 cache 注释） | 是（P1，**双路径**：uv 可用走 `uv venv`，否则回退 `python -m venv` 并显式告知；spawnable 探测；记录 creator 并据此分派后续安装，§4.5.7） |
| 10 | `uv venv --link-mode` | **部分具备** | osdk 有 `store/link.rs:19 LinkMode` 四档与 `same_filesystem()`，但未接到 venv | 是（P2，映射到 uv 参数） |
| 11 | `uv venv --seed` / `--relocatable` / `--system-site-packages` | **缺失** | 同上 | 是（P1，但**并非全部等价**）：`--seed` 归一化为「pip+setuptools+wheel 齐备」语义（stdlib 需补装后两者）；`--system-site-packages` 两路径均有；**`--relocatable` 为 uv 独有，pip 路径下不可用，须报错而非静默忽略——能力降级而非等价**（§4.5.7） |
| 12 | `uv pip install` | **缺失** | — | 是（P1，委托） |
| 13 | `uv pip uninstall` | **缺失** | — | 是（P1，委托） |
| 14 | `uv pip compile` | **缺失** | — | 是（P2，委托） |
| 15 | `uv pip sync` | **缺失** | — | 是（P2，委托） |
| 16 | `uv pip freeze` / `list` / `show` / `tree` / `check` | **缺失** | — | 是（P2，透传） |
| 17 | `uv pip --system` 语义 | **缺失** | — | 是（P1，但 osdk 侧默认拒绝，需显式开关） |
| 18 | `uv add` / `remove` | **缺失** | — | 是（P3，委托） |
| 19 | `uv sync` | **缺失** | — | 是（P3，委托） |
| 20 | `uv lock` + `uv.lock` universal resolution | **缺失** | `lockfile.rs` 只锁工具版本，无 Python 依赖图 | 是（P3，**引用而非复制**，见 §4.4） |
| 21 | `uv run` | **部分具备** | `osdk exec` 存在（`commands.rs`），但不解析 `pyproject.toml`/不自动 sync | 是（P3） |
| 22 | `uv tree` | **缺失** | — | 是（P2，透传） |
| 23 | `pyproject.toml` 读取 | **缺失** | `config/mod.rs` 不含 pyproject 解析 | 是（P3，只读需要的字段） |
| 24 | `uv workspace`（`metadata`/`dir`/`list`） | **缺失** | — | 是（P4，透传） |
| 25 | `uv tool install` / `uninstall` / `list` / `upgrade` | **缺失** | `tool.rs:549 DYNAMIC_NAMESPACES` 无 python 包命名空间 | 是（P4，需新增命名空间，见 §7.4；`UV_TOOL_DIR` 钉定手法已由 mise 印证，§4.5.3） |
| 26 | `uvx` / `uv tool run`（ephemeral） | **缺失** | — | 是（P4，透传） |
| 27 | `uv tool dir` / `update-shell` | **部分具备** | osdk 有 `dirs.rs:277 shims()` 与 `activate/`，但无 Python 工具目录 | 是（P4，经 `UV_TOOL_DIR` / `UV_TOOL_BIN_DIR` 钉入 osdk 安装根，§4.5.3） |
| 28 | `uv tool audit` | **缺失** | — | 否（非目标，安全审计不在本提案范围） |
| 29 | PEP 723 单文件脚本 | **缺失** | — | 是（P4，透传） |
| 30 | `uv build` | **缺失** | — | 否（非目标，§1.2） |
| 31 | `uv publish` | **缺失** | — | 否（非目标，§1.2） |
| 32 | `--index` / `--default-index` | **缺失** | `config/mod.rs:299 RegistriesConfig` 只有 `npm`(:300) | 是（P1，**核心**，§5） |
| 33 | `--extra-index-url` | **缺失** | — 但 `source/mod.rs` 的多源模型可直接建模 | 是（P1） |
| 34 | `--find-links` | **缺失** | — | 是（P2，仅透传，osdk 不解析） |
| 35 | `--index-strategy` 三档 | **缺失** | — | 是（P1，**且 osdk 侧收紧默认**，§7.3） |
| 36 | keyring / 私有源认证 | **缺失** | 有先例：`source/mod.rs:38 headers` + `:41 forward_credentials`；`package_registry.rs:850` 已能识别「凭据由环境配置」并退出规划 | 是（P4，仅透传 `--keyring-provider subprocess`） |
| 37 | `uv auth login/logout/token` | **缺失** | — | 否（非目标，凭据由 uv 自管） |
| 38 | `uv cache dir` | **部分具备** | `cache/mod.rs:86 describe`、CLI `osdk cache dir`（实测输出四条路径） | 是（**P0**，加入 uv 缓存；mise 实测不做此事，是差异化，§4.5.6） |
| 39 | `uv cache clean` | **部分具备** | `osdk cache clean` 存在但只删 `downloads`（实测帮助文本：「Remove downloaded archives (keep store + installs)」） | 是（P2） |
| 40 | `uv cache prune` | **部分具备** | `osdk prune` 存在，语义是 store GC，非 uv 缓存 prune | 是（P2，见 §6.4） |
| 41 | CAS + hardlink 策略 | **已具备（解释器层）/ 委托（wheel 层）** | `store/mod.rs:23 Cas`、`store/link.rs:132 materialize`；wheel 层由 uv `archive-v0` 承担（§6.1 实测） | 是（分层，不重复） |
| 42 | 跨盘硬链接退化 | **已具备** | `store/link.rs:62 same_filesystem`、`:145-159` 的 hardlink→reflink→copy 阶梯 | 是（P2，与 uv 行为对齐，§6.5 实测） |
| 43 | wheel 解包缓存 | **缺失（应委托）** | uv `archive-v0` 实测承担此职责 | 是（**P0**，通过统一缓存目录获得） |
| 44 | `--frozen` / `--locked` | **部分具备** | `lockfile.rs` 有 schema 校验与 `load()`(:307) 的严格性，但无 Python 依赖图的 frozen/locked | 是（P3） |
| 45 | `--offline` | **已具备（框架）** | `config/mod.rs:66 Settings::offline` | 是（P1，需传导为 `UV_OFFLINE`） |
| 46 | `--exclude-newer` | **缺失** | — | 是（P3，透传） |
| 47 | `--require-hashes` / `--no-verify-hashes` | **部分具备** | `Settings::require_checksums`(:62)、`pipeline/verify.rs:105 verify_file`；未接 Python | 是（P1，**且 `--no-verify-hashes` 禁止透传**，§7.1） |
| 48 | 构建隔离 / sdist 构建 | **缺失（应委托）** | — | 是（P3，仅透传 `--no-build-isolation`） |
| 49 | 平台标签 / `requires-python` | **部分具备** | `platform.rs`（osdk 自有三元组模型）；无 PEP 425 wheel 标签、无 `requires-python` 求值 | 是（P3，委托 uv，osdk 只做平台键映射） |
| 50 | `uv export`（3 种格式） | **缺失** | — | 是（P4，透传） |
| 51 | `uv version` / `self update` | **已具备** | `self_update.rs`、`osdk self` | 是（osdk 自管，uv 版本由 osdk 钉住，§4.4） |
| 52 | `uv format` / `check` | **缺失** | — | 否（非目标，代码风格不属 SDK 管理） |
| 53 | `uv init` | **缺失** | — | 否（非目标，脚手架不属 SDK 管理） |
| 54 | 字节码编译（`--compile-bytecode`） | **缺失** | — | 是（P2，透传） |

**汇总**：54 项中，已具备 11 项、部分具备 12 项、缺失 31 项；列入目标 47 项、显式非目标 7 项。

**第二轮（mise 旁证）后的变化**：三档判定（已具备/部分具备/缺失）**一项未变**，因为 mise 的做法不改变 osdk 自身的代码事实。变化集中在「列入目标」列的**实现约束与阶段归属**：第 9/11 项补充了 spawnable 探测与「无 stdlib 回退」的表态（**该表态已于第三轮反转，见下条**），第 25/27 项确认 `UV_TOOL_DIR` 钉定手法（§4.5.3），第 38/43 项**从 P1 前移到 P0**（§4.5.6 实测证明 mise 不纳管 uv 缓存，该项是 osdk 的差异化且成本仅一行）。

**第三轮（stdlib 回退实测）后的变化**：三档判定仍**一项未变**。「列入目标」列有两项实质改动：**第 9 项**从「委托 uv + 无 stdlib 回退」改为「双路径（uv 或 `python -m venv`）+ creator 记录与分派」；**第 11 项**从「透传」改为「部分等价」，其中 `--relocatable` 明确标注为 pip 路径下**不可用的能力降级**。改动依据是本轮实测推翻了「回退会绕过门禁」这一判断——pip 有等价的门禁表面（§4.5.7、§7.1.1）。阶段归属无变化（两项仍在 P1）。

---

## 4. 核心决策：uv 作为受管后端，而非自研

本节给出**单一推荐方案**及其论证，不留两条路。

### 4.1 推荐

**在 osdk 中新增一个 `uv` 受管工具后端，并在其上建一层 osdk 原生命令；osdk 负责源选择、缓存归置、fail-closed 门禁与版本钉定，uv 负责求解、构建与安装。**

具体形状照抄 `backend/native_tool.rs` 已验证的模式：uv 是 osdk 管理的一个二进制（像今天的 node/rust），Python 包操作是「以确定版本的 uv 为 runtime 的受管子进程」，产物经 receipt 绑定 provider+runtime+字节。

### 4.2 为什么不自研：三组数字

**第一，体积。** 按 AGENTS.md 指定方式用独立 `CARGO_TARGET_DIR` 分两次构建实测（2026-09-13，Windows x64 release）：

| 二进制 | 实测大小 |
| --- | --- |
| `osdk.exe` | 12,876,800 bytes = **12.28 MB** |
| `osdk-shim.exe` | 3,703,296 bytes = **3.53 MB** |

对照 AGENTS.md 记录的 2026-09-12 基线（11.95 MB / 3.5 MB），当前值略有增长但在正常范围。**AGENTS.md 的引用提醒是对的：基线会随功能增长而变，本文因此实测了当前值而非采信文档数字。**

uv 侧实测：`uv-x86_64-pc-windows-msvc.zip` 下载 17,612,025 bytes（压缩），解压后 `uv.exe` = 41,556,784 bytes = **39.63 MB**，另有 `uvx.exe` / `uvw.exe` 各 348,976 bytes。

**39.63 MB vs 12.28 MB。** 自研要在 osdk 内塞进求解器、wheel 安装器、PEP 517 前端、平台标签模型，即便只做 uv 的一半，也远超「增长 10% 需给理由」的阈值。而这一切都会撞上 AGENTS.md 记录的 `Backend` trait vtable 问题：`Registry::new()` 把所有 backend 实例化为 `Arc<dyn Backend>`，新方法进 vtable 后链接器无法裁剪——这条曾让 shim 白背 5.15 MB。

**第二，正确性不对称。** osdk 现有 backend 的失败模式是「下载慢了」或「装错版本号」，都可观测、可重试。依赖求解的失败模式是「装了一组互不兼容的包」，可能在运行时才炸。uv 的 PubGrub 实现处理了 pre-release 传播（官方文档专门解释了为何保持候选集固定以免让 PubGrub 的 learned incompatibilities 失效【官方】）、`requires-python` 上界的实用取舍、URL 依赖的传递性限制等一系列非显然决策。这些不是能靠读规范补齐的，是靠生态碰撞磨出来的。

**第三，维护面。** uv 官方与 pip 的差异清单本身就有 24 条【官方】。自研意味着要么复刻这 24 条，要么产生第 25 条差异而无人为其负责。

**第四，同类产品的独立选择。** mise 作为同类 SDK 版本管理器，在求解、wheel 安装、venv 创建、Python 工具安装四个环节**全部选择委托**，自研部分为零（§4.5）。它自己的二进制实测 **98.52 MB**（`mise-v2026.9.6-windows-x64.exe`，103,305,728 bytes），仍不愿把 Python 打包纳入自研——这与「体积」这条理由指向同一结论。

### 4.3 复用 uv 的现实约束及应对

诚实列出代价，并给出可执行的应对，而非掩盖：

| 约束 | 事实 | 应对 |
| --- | --- | --- |
| **二进制体积** | uv 39.63 MB【实测】 | uv **不打进 osdk**，而是按需下载的受管工具，与今天的 node/rust 同构。osdk 与 shim 体积**零增长**（新增代码全部在 `#[cfg(feature = "install")]` 之后，§8 每阶段标注） |
| **许可** | Apache-2.0 OR MIT 双许可【一方】 | 无 copyleft 传染。作为独立二进制分发时保留其许可文件即可 |
| **离线** | uv 支持 `--offline`【实测：warm cache 下 `uv pip install --offline requests` 成功解析并安装 5 包】；pip 支持 `PIP_NO_INDEX=1`【实测 2026-09-14】 | osdk `Settings::offline`(config/mod.rs:66) 传导为 `UV_OFFLINE=1`（uv 路径）或 `PIP_NO_INDEX=1`（回退路径）。**第三轮修正**：原文此处主张「uv 未安装时离线需 fail-closed，不得降级到系统 pip」——「不得用系统 pip」仍成立，但「必须 fail-closed」已被推翻。正确行为是使用 osdk 创建的 venv 内自带的 pip（隔离且受门禁约束），见 §6.7 的概念拆分与 §4.5.7 |
| **版本漂移** | uv 迭代快（0.12.13 发布于 2026-09-10，距实测仅 3 天）；mise 亦然（2026.9.6 发布于实测前一天） | **osdk 钉住 uv 的确定版本并记入 `osdk.lock`**，照 `LOCKED_NATIVE_RUNTIME_VERSION_OPTION`（native_tool.rs:27）的既有做法。升级是显式动作 |
| **与 osdk lock 职责重叠** | `osdk.lock` 锁工具版本，`uv.lock` 锁 Python 依赖图 | **不复制内容，只记指纹**。照 `LockedNativeLock`(lockfile.rs:122) 与 `LOCKED_NPM_LOCK_SHA256_OPTION` 的先例：`osdk.lock` 记 `uv.lock` 的 kind/format/sha256，`uv.lock` 仍是唯一真相源。**已由 mise 印证方向**（其 `mise.lock` 连摘要都不记，osdk 更严格，§4.5.4） |
| **与 store 职责重叠** | osdk `store/` 是文件级 blake3 CAS；uv `archive-v0` 是 wheel 解包树 | 分层：解释器归 osdk store，wheel 归 uv 缓存（§6.2 论证为何不能合） |
| **与 shim 职责重叠** | osdk shim 解析并 exec 已安装工具 | uv 自身可被 shim（它是普通二进制）；但 **venv 内的 `python`/`pip` 不应被 osdk shim 接管**——venv 的 `Scripts/` 已有正确的启动器，插一层 shim 只会破坏 `sys.prefix` 推断 |
| **Windows 上「能查到」≠「能启动」** | 【源码】mise 在 venv（`venv.rs:188` 注释 :309-314）与 pipx（`pipx.rs:317`）两处都改用 spawnable 探测，因为 `uv.ps1` / 仅有 shebang 的 `uv` 能通过普通 `which` 却在 spawn 时失败 | osdk 探测 uv 时必须用「能否真正启动」作判据，而非路径存在。这是第二轮新增的约束，已并入 P1 与 §7.4 第 27 项 |
| **解释器 patch 升级使工具环境失效** | 【源码】mise 用 `fix_venv_python_symlink()`（pipx.rs:1069）改写 venv 内符号链接指向 minor 路径；**该函数在非 unix 上是空实现**（:1133-1136），即 Windows 无解 | osdk 不照抄符号链接改写，改用已有的 `NativeToolReceipt`（native_tool.rs:47）绑定 runtime 身份，跨平台可行。见 §4.5.3 |
| **Astral 单点依赖** | uv 与 python-build-standalone 同属 Astral | 已部分缓解：osdk 的三档解释器源（python.rs:67）中 `gh-proxy` 与 `github` 不依赖 `releases.astral.sh`。uv 二进制侧应同样配多源 |

### 4.4 版本钉定与 lock 分层（具体设计）

```
osdk.lock
  tools:
    uv: { version: "0.12.13", sha256: <uv 二进制摘要> }      # osdk 管
  python_deps:                                               # 新增
    lock_kind:   "uv"
    lock_format: "uv.lock-v1"
    lock_path:   "project/uv.lock"
    lock_sha256: <uv.lock 文件字节摘要>
```

这样 `osdk lock` 的语义保持不变（「解析确定版本」），而 Python 依赖图的真相仍在 `uv.lock`。`--locked` 的实现是「跑 uv 后比对 `uv.lock` 的 sha256 是否变化」，与 `LockedNativeLock` 已有的做法一致。

### 4.5 旁证：mise 的同题选择

mise 是与本提案最接近的现成先例——同样是 SDK 版本管理器，同样面对「要不要自研 Python 打包」。本节用 mise 的**实际实现**检验 §4.1 的推荐，而非罗列其功能。既有的 `docs/research/mise-dev-tools-backends-2026-08-28.zh-CN.md` 已覆盖 mise 的后端体系全貌（19 种后端类型、锁定与复用契约、osdk 的对应路线图），本节只做该文未展开的 Python 专项，不重复其内容。

**版本漂移说明**：既有文档固定在 2026-08-28 的修订版本。本轮实际考察 **mise `2026.9.6`（发布于 2026-09-12，即考察前一天）**，源码固定在 tag `v2026.9.6` 解引用后的 commit `acbbdee0b150f5eeb14eb287198b11625ea35472`。下文所有【源码】引用均指该 commit。

#### 4.5.1 结论速览

| 环节 | mise 的选择 | 对本提案的意义 |
| --- | --- | --- |
| 依赖求解 / wheel 安装 | **完全委托**，无自研 | **印证** §4.2 |
| venv 创建 | **委托，双路径**：有 uv 用 `uv venv`，否则 `python -m venv` | **osdk 采纳同一形状**（第三轮反转，§4.5.7）；修正矩阵第 9/11 项 |
| Python 工具安装 | **委托**：优先 `uv tool install`，回退 pipx | **印证** P4 方向 |
| Python 依赖图 lock | **完全不做**，`mise.lock` 只锁工具版本 | **印证** §4.4 |
| 解释器镜像源 | **无任何镜像设置**，PBS URL 硬编码 github.com | **osdk 已明确领先**，见 4.5.5 |
| uv 缓存归置 | **不纳管**，uv 用默认位置 | **印证** P0 是真实差异化 |
| PyPI 索引传递 | 设 `UV_INDEX`（**而非** `UV_DEFAULT_INDEX`） | **反例**，见 4.5.6 |

#### 4.5.2 venv：委托，且是「双路径 + 运行时探测」（本轮最关键的一条）

判定链完整落在 `src/config/env_directive/venv.rs`【源码】：

- `create_python_venv()`（:126）先解析出 uv 二进制：`ts.which_bin_spawnable(config, "uv")`（:188）。注意它问的是 **spawnable**——注释（:309-314，pipx.rs 同样手法）解释了原因：Windows 上一个 `uv.ps1` 或只有 shebang 的 `uv` 能通过普通查找却无法真正启动，所以「能不能 spawn」才是正确的判据。
- 分支判据是 `let use_uv = require_uv || (!Settings::get().python.venv_stdlib && uv_bin.is_some());`（:212）。
- 走 uv 时是 `build_uv_venv_command()`（:77）→ `uv venv <path> [--python <path>] [extra]`（:88-98）；走 stdlib 时是 `build_stdlib_venv_command()`（:101）→ `<python> -m venv <path> [extra]`（:121-123）。
- **解释器由 mise 指定而非交给 uv 猜**：`python_path` 来自 `plugins::core::python::python_path(tv)`（:164），作为 `--python <绝对路径>` 传入（:92）。这与本提案 P1 计划的「`--python` 指向 osdk 管理的解释器」完全一致。
- 版本不匹配的处理是**分级**的（:28-38 的字段注释写得很清楚）：`_.python.venv.python` 是用户点名的版本，miss 即报错；`active_python` 只是偏好，miss 则回退旧行为，因为「用户从未点名这个版本」。
- **解释器未安装时不创建**：`is_version_installed` 为 false 时只 warn 并 `return Ok(false)`（:170-184），提示先跑 `mise install`。

**本机实测验证**（时间 2026-09-13 约 23:10 CST，全程用隔离的 `MISE_DATA_DIR` / `MISE_CACHE_DIR` / `MISE_CONFIG_DIR` / `MISE_STATE_DIR`，未触碰用户真实 mise 状态）：

- 未安装 uv 时，`_.python.venv = { create = true }` 的调试日志为 `INFO creating venv with stdlib at: ...` + `DEBUG $ python3 -m venv ...`。**证实 stdlib 回退真实存在。**
- 通过 `mise use uv@latest` 装入 uv（实测 `uv@0.12.13`，耗时 223 s）后，同样配置的日志变为 `INFO creating venv with uv at: ...` + `DEBUG $ ...\uv.exe venv ...\.venv --seed`。生成的 `pyvenv.cfg` 含 **`uv = 0.12.13`** 字段——这是 uv 创建的机器可读证据，stdlib venv 不会写该字段。
- `uv_create_args = ["--seed"]` 被原样附加到命令尾部，`pyvenv.cfg` 中出现 `seed = true`。

**另有一条独立的 uv 项目集成路径** `src/uv.rs`【源码】，语义与上面那条不同，容易混淆：

- 触发条件是 `uv_root()`（:82）—— `file::find_up(CWD, &["uv.lock"])`，即**向上查找 `uv.lock`**。
- `python.uv_venv_auto` 实测默认 **`false`**（`mise settings get python.uv_venv_auto` 返回 `false`）。取值为 `"source"` / `"create|source"` / 已弃用的 `true`【源码 settings.toml:2546-2565；`true` 自 2026.7.0 起告警，2027.7.0 移除】。
- `"source"` 只激活已存在的 `.venv`，**创建权归 uv**。实测：设 `uv_venv_auto = "source"` 且存在 `uv.lock` 但无 `.venv` 时，mise 不创建，只警告 `uv venv not found at: ...` 并提示「run a uv command like `uv sync` or `uv venv`. Alternatively, enable `[deps.uv]` and run `mise deps`」；用 uv 自己建好 `.venv` 后再跑 `mise env`，`VIRTUAL_ENV` 立即被导出。无 `uv.lock` 时整条路径 no-op（实测无 `VIRTUAL_ENV`）。
- venv 路径遵循 `UV_PROJECT_ENVIRONMENT`（:85-115），默认 `.venv`，支持绝对路径。
- 另有 `[deps.uv]` provider（`src/deps/providers/uv.rs`）：`install_command` 是 `uv sync`（:49），`sources` 为 `uv.lock` + `pyproject.toml`（:32），`optional_outputs` 为 `.venv`（:45），`applicability` 要求 `uv.lock` 存在（:53）。**即 mise 把「同步依赖」也整体委托给 `uv sync`，自己只做 staleness 追踪。**

**对本提案的修正**：矩阵第 9/11 项原判定「缺失，P1 委托 uv」方向正确，但**实现形状需要补两点**，已在下文矩阵与 P1 中改掉：(a) 探测 uv 必须问「能否 spawn」而非「是否存在」，这是 Windows 特有的坑，mise 在 venv 与 pipx 两处都专门处理了；(b) 是否提供 stdlib 回退是一个必须明确表态的决策点——见 4.5.7。**第三轮补充**：该决策点在第二轮被判为「不提供」，已于第三轮反转为「提供且受管」，因此 mise 的双路径形状不仅是「摆出了岔路」，而是 **osdk 最终采纳的形状**。

#### 4.5.3 pipx 后端：委托 + 一处必须修正既有文档的表述

`src/backend/pipx.rs`【源码】：

- 判定顺序：`uvx_allowed = Settings::get().pipx.uvx != Some(false) && !options.uvx_disabled()`（:315），然后 `spawnable_dependency(..., "uv")`（:317）。只有 uv 不可用时才探测 pipx（:322-328）。
- uv 路径执行 `uv tool install`（:390），且**把 uv 的工具目录钉到 mise 的安装路径**：`.env("UV_TOOL_DIR", tv.install_path())` + `.env("UV_TOOL_BIN_DIR", tv.install_path().join("bin"))`（:808-809）。这正是本提案 P4 计划的做法，**得到印证**。
- 降级规则是**显式且带解释**的：包级 `uvx = false` 或全局 `pipx.uvx = false` 时强制走 pipx，且错误消息会说明「因为设置了 X 所以不能用 uv」（:341-350），避免把用户引向死路。
- `minimum_release_age` 映射到 uv 的 `--exclude-newer`（:680），pipx 则映射到 `--pip-args=--uploaded-prior-to=`（:688）；并且会检查 uv 版本是否足够新以支持该参数，不够则告警（:856-864）。

**必须修正既有文档理解的一点**：`mise-dev-tools-backends-2026-08-28.zh-CN.md:499` 那句「所使用的 Python 运行时和安装器记录在安装 manifest 中，并在复用前验证」出现在该文的 **osdk 路线图 Phase 3 章节**，是对 **osdk 自身实现的建议**，**不是**对 mise 现状的描述。本轮在 `pipx.rs` 全文检索 `manifest` / `install_metadata` / `installed_by` / `python_runtime` **均无命中**——mise 并没有这样的 manifest。它解决「解释器 patch 升级后工具 venv 失效」用的是另一手法：`fix_venv_python_symlink()`（:1069）把 venv 内的 `python3` 绝对符号链接从 `.../installs/python/3.12.1/bin/python3` 改写为 minor 版本路径（`3.12`），使 3.12.1→3.12.2 不必重装（:1060-1067）。

**这一手法在 Windows 上是空实现**：`#[cfg(not(unix))] fn fix_venv_python_symlink(...) -> Result<()> { Ok(()) }`（:1133-1136）。也就是说 **mise 在 Windows 上没有解决解释器 patch 升级导致工具环境失效的问题**。这不是可以照抄的先例，而是 osdk 必须自己解决的缺口——而 osdk 已有的 `NativeToolReceipt`（`native_tool.rs:47`，绑定 provider/runtime/产物字节）恰好是比符号链接改写更可移植的答案。既有文档 Phase 3 的那条建议因此是对的，只是它是**建议**而非**对 mise 的观察**。

#### 4.5.4 lock：mise 完全不涉 Python 依赖图（印证 §4.4）

`src/lockfile.rs`（283,688 字节）中检索 `python` 仅 7 处命中，全部在测试里，形态为 `[[tools.python]]` + `backend = "core:python"` + `version = "3.11.0"`（:4611-4646）；检索 `pyproject` 与 `uv.lock` **零命中**。

即 `mise.lock` 只锁「工具 python 的版本」，对 Python 依赖图零认知。mise 唯一读 `uv.lock` 的地方是把它当**项目根标记**（`src/uv.rs:82`）与**staleness 输入**（`src/deps/providers/uv.rs:32`），从不解析其内容。

**这与本提案 §4.4「`osdk.lock` 只记 `uv.lock` 的 kind/format/sha256，不复制内容」完全同向，且 osdk 的做法更严格**——mise 连摘要都不记，因此无法察觉 `uv.lock` 被外部改动；osdk 记摘要使 `--locked` 可实现。§4.4 无需修改。

#### 4.5.5 解释器源：osdk 明确领先，且差距比上一轮认知的更大

上一轮已实测 osdk 在解释器镜像上优于 uv（三档源 + 自动测速 vs 单个前缀替换）。把 mise 放进同一对比后，结论更强：

- **PBS 下载 URL 在 mise 中是硬编码常量**：`const PBS_RELEASE_DOWNLOAD_URL: &str = "https://github.com/astral-sh/python-build-standalone/releases/download/";`（`src/plugins/core/python.rs:36`），另在 :556 与 :1218 直接以 `format!` 拼同样的 github.com URL。
- **实测确认 mise 没有任何 Python 镜像设置**：`mise settings --all` 共 194 项，其中 `python.*` 只有 4 项（`default_packages_file`、`pyenv_repo`、`uv_venv_auto`、`venv_stdlib`），`pipx.*` 只有 1 项（`registry_url`）。检索 `mirror` / `url_rewrite` / `github_url` 只命中 **`go.download_mirror`（默认 `https://dl.google.com/go`）** 与 **`zig.use_community_mirrors`（默认 `true`）**——**mise 为 Go 和 Zig 做了镜像，却没为 Python 做**。可调的只有 `python.precompiled_arch` / `precompiled_os` / `precompiled_flavor`（选资产变体，不改主机）。
- 校验路径与 osdk 同构：`fetch_checksum_from_shasums()` 从 release 的 `SHA256SUMS` 取摘要（:1225），另有可选的 GitHub Artifact Attestations（`python.github_attestations`，:33-34）。
- 编译路径判据：`if cfg!(windows) || settings.python_compile(CompilePurpose::Install) != Some(true) { install_precompiled } else { install_compiled }`（:1076-1081）。即 **Windows 上恒走预编译，源码编译路径不可用**；`python.compile` 三态语义为 `true` 恒编译、`false` 恒预编译、未设则「有预编译就用，否则编译」【源码 settings.toml:2466-2473】，实测默认未设。源码编译经 pyenv 的 python-build（`pyenv_repo` 实测默认 `https://github.com/pyenv/pyenv.git`），并有 `python.patch_url` / `patches_directory`。

**三方对比**：

| 维度 | osdk（现状） | uv | mise |
| --- | --- | --- | --- |
| PBS 源数量 | **3 档**（astral / gh-proxy / github），`python.rs:67` | 1（`UV_PYTHON_INSTALL_MIRror` 前缀替换） | **1（硬编码 github.com）** |
| 自动测速与失效转移 | **有**（`source/select.rs`） | 无 | 无 |
| 探测目标与真实下载同源 | **是**（`probe_url` 用 `SHA256SUMS`） | — | — |
| 源码编译回退 | 无 | 无 | 有（Windows 除外） |
| PyPy / GraalPy | 见既有文档 | 有 | 有（PyPy 走 `downloads.python.org`） |
| 校验和来源 | release `SHA256SUMS` | PBS 元数据 | release `SHA256SUMS` |
| 可选签名/证明 | `verify_minisign` + Sigstore 证据 | — | GitHub Attestations |

**osdk 在「解释器获取的可达性」这一维上领先两者**，§5.5 建议新增的 nju / ustc 两档 PBS 镜像会把差距进一步拉大。这也解释了为什么本提案坚持解释器层自研、只委托包层——osdk 在这一层的既有投入是真实资产，不该为了统一而丢掉。

#### 4.5.6 索引与缓存：mise 提供了一个反例和一个空白

**反例（索引）**：`uvx_cmd()` 设置的是 `.env("UV_INDEX", Self::get_index_url()?)`（`pipx.rs:810`），pipx 路径设 `PIP_INDEX_URL`（:837）。`get_index_url()`（:709-743）把 `pipx.registry_url`（实测默认 `https://pypi.org/pypi/{}/json`）规范化为 `/simple` 形式。

**`UV_INDEX` 对应 uv 的 `--index`，即「额外索引」，而非 `--default-index`。** 按 uv 官方语义，`--index` 提供的索引**优先级高于默认索引**。因此当用户把 `pipx.registry_url` 指向一个 PyPI 镜像时，mise 实际是把镜像装配成「优先于 pypi.org 的额外索引」——正是本提案 §5.6 论证要避免的形状。

需要公允地说明这在 mise 的场景下危害有限：`pipx:` 装的是独立 CLI 工具，且 `first-index` 策略会让「第一个含该包的索引」赢，用户配了镜像通常就是想让它赢。但**这个形状不能照抄进 osdk**，因为 osdk 的索引配置会同时服务 `osdk python install`（项目依赖）与工具安装两条路径，前者一旦让镜像盖过私有索引就是依赖混淆。§7.3 的四条硬规则因此保留并加强，**新增一条测试**（§7.4 第 25 项）专门断言 osdk 永不产出 `UV_INDEX` / `--index` 形式的镜像配置。

**空白（缓存）**：在 mise 全部已下载源码中检索 `UV_CACHE_DIR` **零命中**（`UV_PYTHON_INSTALL_DIR` 只在 `src/cli/sync/python.rs:91,111` 用于**读取** uv 已装的解释器以便同步，不是写入重定向）。

**本机实测证实 mise 不纳管 uv 缓存**：即使把 `MISE_CACHE_DIR` 指向隔离目录，该目录下的 `cache/uv/` 只含 mise 自己的工具元数据（`0.12.13/`、`remote_versions-*.msgpack.z`、`version_tags_v2-*.msgpack.z`，最大 6,466 B），**没有** uv 的任何缓存标志目录；而 uv 的真实缓存 `%LOCALAPPDATA%\uv\cache` 下 `archive-v0` / `interpreter-v4` / `sdists-v9` / `simple-v25` / `wheels-v6` 齐备，且本轮 venv 创建期间有 1 个 `archive-v0` 子目录被写入（20 分钟内修改）。

即：**在 mise 下用 uv，wheel 缓存仍散落在 uv 默认位置，mise 的 `cache` 目录对它无效。** 这正是本提案 P0（补 `UV_CACHE_DIR`）要消除的问题。**P0 因此不是「补齐同类产品已有的能力」，而是 osdk 相对 mise 的差异化**——`cache/mod.rs` 已有的 `PIP_CACHE_DIR` 重定向机制让 osdk 只花一行就能做到 mise 没做的事。P0 的定位在路线图中已据此改写。

#### 4.5.7 决策点：要不要提供 stdlib venv 回退（**第三轮结论反转**）

mise 有 `python -m venv` 回退，本提案原先只写「委托 uv」，没表态。第二轮把这个岔路摆出来后给了「不提供回退」的推荐，**第三轮的本机实测推翻了该推荐所依赖的关键理由**。本节保留原三条理由并逐条处理，以便追溯判断是怎么错的。

**修正后的推荐：提供 stdlib venv 回退，但必须是「受管且带门禁」的回退，而非放任。**

##### 本轮实测前提【实测】

环境：Windows x64，2026-09-14；全程使用隔离的 `PIP_CACHE_DIR` 与 `PIP_CONFIG_FILE`（后者指向空文件以屏蔽用户全局 pip 配置）+ 临时目录，未触碰用户真实状态；解释器 Python 3.14.7；**本机当前 `uv` 未安装（`Get-Command uv` → NOT FOUND），`osdk` 位于 `E:\osdk-bin\osdk.exe`**。

这个前提本身就是论证的一部分：**回退路径不是假想分支，而是本机眼下就会走到的默认路径。** 一个在用户当前环境下必然触发 fail-closed 的设计，实际效果等于「Python 包功能不可用」。

##### 原三条理由的处理

**原理由 1「两条创建路径产出的 venv 不等价」——仍然成立，且本轮拿到了更精确的依据，但推论错了。**

实测差异：

| 维度 | `python -m venv`【实测】 | `uv venv`【第二轮实测】 |
| --- | --- | --- |
| pip | **自带**（pip 26.2.1） | 默认不装，需 `--seed` |
| setuptools | **不带**（`find_spec` → False） | `--seed` 时装 |
| wheel | **不带**（`find_spec` → False） | `--seed` 时装 |
| `pyvenv.cfg` 字段 | `home` / `include-system-site-packages` / `version` / `executable` / `command`，**无 `uv =`** | 含 **`uv = 0.12.13`**、`seed = true` |
| `Scripts/` 内容 | `pip.exe`/`pip3.exe`/`pip3.14.exe`/`python.exe`/`pythonw.exe` + activate 系列 | uv 布局 |

**正确推论不是「禁止回退」，而是「必须记录创建者与 seed 状态，并在后续操作时按创建者分派」。** 差异是可检测、可分派的工程问题，不是不可逾越的障碍。`pyvenv.cfg` 的 `uv =` 字段正是一个现成的、机器可读的创建者标志（第二轮已实测其存在，本轮实测其在 stdlib 路径下缺失）——两轮实测正好构成完整的判定依据。

**原理由 2「回退会静默绕过全部门禁」——已被本轮实测推翻。**

原文断言「§7.1 的哈希、索引策略、离线传导全部依赖 uv 的参数表面」。这是错的。实测证明 pip 有等价的参数与环境变量表面：

| 门禁 | uv 侧 | pip 侧【实测】 |
| --- | --- | --- |
| 哈希强制 | `--require-hashes` | `--require-hashes`，**fail-closed 已验证**：正确上游 sha256（certifi 2026.7.22 = `62f2…3775`）经 TUNA 安装成功 exit=0；64 个 `0` 的错误哈希 → `ERROR: THESE PACKAGES DO NOT MATCH THE HASHES FROM THE REQUIREMENTS FILE.` 列出 Expected/Got，**exit=1** |
| 索引替换 | `UV_DEFAULT_INDEX` | `PIP_INDEX_URL`，**已验证生效**：`pip config debug` 的 `env_var` 段可见，安装日志 `Looking in indexes: https://pypi.tuna.tsinghua.edu.cn/simple` 且 `Downloading` 成功 |
| 离线 | `UV_OFFLINE=1` | `PIP_NO_INDEX=1`，**已验证**：`pip install requests` → `ERROR: Could not find a version that satisfies the requirement requests (from versions: none)` |
| 其他可用参数 | — | `--no-index` / `-i,--index-url` / `--extra-index-url` / `--only-binary` |

**错在哪里**：原判断把「uv 的参数表面」当成了门禁的**唯一载体**，而门禁的实质是「哈希必须被校验、索引必须可控、离线必须可强制」——这三件事 pip 都能做到，只是变量名和参数名不同。把实现载体误认为能力边界，是这次判断反转的根因。

附带一个连带修正：原文说回退「等于回到『用户手改 pip.conf』的世界，违背用户偏好」。这也不成立——**osdk 注入 `PIP_INDEX_URL` 等环境变量，用户完全不需要手改 `pip.conf`**，这正好符合「源与配置一律通过 osdk 命令管理」的偏好。而且 osdk 的 `cache/mod.rs:40` 早已在注入 `PIP_CACHE_DIR`，机制现成。

**原理由 3「uv 是受管工具，缺失时应装上」——降级为「首选项」而非「唯一项」。**

uv 的性能与 universal resolution 收益真实存在，osdk 应优先引导安装它。但「应该装 uv」不能推出「没装 uv 就什么都不做」。前者是偏好，后者是硬前置——把偏好实现成硬前置，代价由用户承担（见上文「本机 uv 未安装」这一前提）。

##### 单一推荐：自动回退 + 显式告知 + 可选 uv-only

**判据**：

1. **默认自动回退。** uv 不可用（含未安装、不可 spawn）时，用 `python -m venv` 创建，并在输出中**明确说明**：(a) 当前走的是 pip 路径；(b) 与 uv 路径的能力差异（无 universal resolution、无 `--relocatable`、解析较慢）；(c) 提示 `osdk install uv` 可获得更快路径。**静默回退是不可接受的**——用户必须知道自己在哪条路径上，否则性能与能力差异会变成难以归因的困惑。
2. **仅在显式要求 uv-only 时 fail-closed。** 通过配置项或 `--require-uv` 表达。这与 mise 的 `require_uv` 字段（`venv.rs:37`）同构，也与其 `uv_venv_auto` 路径的 `require_uv: true`（`uv.rs:39`）用法一致——**mise 已经证明「默认回退 + 特定路径强制 uv」这个组合是可行的**。
3. **回退路径同样受全部门禁约束。** 见 §7.1 新增的 pip 侧等价门禁清单。回退是路径切换，**不是门禁豁免**。
4. **记录创建者并据此分派。** 见下。

##### creator 记录与分派设计

**记录**：venv 创建后，由 osdk 在自己的状态中记录该 venv 的 `creator`（`uv` / `stdlib`）与 `seed` 内容（`pip` / `setuptools` / `wheel` 各自是否存在）。**不写入 venv 内部**——venv 是用户可随手删除、也可能由用户自己用 uv/python 直接创建的目录，osdk 不应污染它。同时保留一条**兜底探测**：读 `pyvenv.cfg` 是否含 `uv =` 字段（两轮实测已确立该字段的判别力），用于 osdk 记录缺失（如用户在 osdk 之外创建了 venv）时恢复判定。

**分派**：`osdk python install` 按 creator 选择安装器——`uv` → `uv pip install --python <venv python>`；`stdlib` → 该 venv 自带的 `python -m pip install`。**关键是用 venv 内的 pip，而不是系统 pip**，这个区分见 §6.7。

**`--seed` 语义对齐**：两条路径的 seed 语义不同，必须归一化而非直接透传。

| osdk 请求 | uv 路径 | stdlib 路径 |
| --- | --- | --- |
| 默认（不要求 seed） | `uv venv`（无 pip） | `python -m venv`（**已含 pip**，无法去除，除非 `--without-pip`） |
| `--seed` | `uv venv --seed`（装 pip/setuptools/wheel） | `python -m venv`（已有 pip）**+ 显式补装 setuptools、wheel** |

即：stdlib 路径下「默认」已经超出了 uv 的默认（多一个 pip），而「`--seed`」又不足（缺 setuptools/wheel）。osdk 应把 `--seed` 实现为「确保 pip + setuptools + wheel 三者齐备」这一**语义**，而不是把参数原样传给两个行为不同的工具。

**能力降级须如实告知**：`--relocatable` 是 uv 独有，stdlib 无对应物；回退路径下该能力**不可用**，应报错而非静默忽略（静默忽略会产出一个不可迁移却被以为可迁移的 venv）。矩阵第 11 项已据此标注为「能力降级而非等价」。

---


## 5. 镜像加速：索引元数据与制品下载分开处理

本节是全文实测密度最高的部分。核心论点：**PyPI 的两类流量性质不同，必须分别设计。**

### 5.1 为什么必须分开

`syspkg/mirror.rs` 的模块注释（:15-28）已经建立了这个思维方式：winget 镜像只加速「找包」，因为 manifest 里的 `InstallerUrl` 指向厂商自己的服务器；Homebrew bottle 集中托管，所以两半都能加速。注释最后一句是「A user who switches winget sources expecting faster downloads will conclude the feature is broken」。

PyPI 的情况是**两半都能加速，但安全性质相反**：

- **索引元数据**（`/simple/<name>/`）：镜像返回的是「有哪些版本、每个文件的 URL 与哈希」。这份数据**决定了后续要下什么**，镜像在此处有实质影响力——它可以隐藏新版本、可以列出一个不存在于上游的文件。
- **制品下载**（`.whl` / `.tar.gz`）：文件内容**自带可校验的哈希**，且哈希来自索引。若索引可信，制品侧镜像**不扩大信任面**——篡改必然被哈希捕获。

这个区分直接决定了 §7 的门禁设计。

### 5.2 索引形态：PEP 503 / 691 / 658 / 700 的镜像差异【实测】

**测量方法**：对每个镜像的 `/simple/requests/` 发两次请求。第一次带 `Accept: application/vnd.pypi.simple.v1+json`，检查响应 `Content-Type` 是否为 `application/vnd.pypi.simple.v1+json`（PEP 691 的判据）；若是则解析 JSON，检查 `meta.api-version`、顶层 `versions`（PEP 700）、`files[0].size`（PEP 700 必填）、`files[0].upload-time`（PEP 700 可选）、以及正文是否含 `core-metadata`（PEP 658）。第二次带 `Accept: text/html`，检查 `data-core-metadata` / `data-dist-info-metadata` 属性。另单独请求 `<wheel-url>.metadata` 检查 PEP 658 旁挂文件是否真实存在，并校验响应体首行是否为 `Metadata-Version:`。测量时间：2026-09-13 21:30–21:50 CST，北京，家用宽带。

| 镜像 | PEP 691 JSON | api-version | PEP 700 versions/size/upload-time | PEP 658 (JSON 内) | PEP 658 (`.metadata` 旁挂) | 旧版 `/pypi/<name>/json` |
| --- | --- | --- | --- | --- | --- | --- |
| pypi.org（上游） | **是** | 1.4 | 是 / 是 / 是 | **是** | **200，真实 METADATA**（2474 B，首行 `Metadata-Version: 2.4`） | 是 |
| 清华 TUNA | **是** | 1.1 | 是 / 是 / 是 | 否 | 404 | 是 |
| 中科大 USTC | **是** | 1.1 | 是 / 是 / 是 | 否 | 404 | 否 |
| 北外 BFSU | **是** | 1.1 | 是 / 是 / 是 | 否 | 404 | 是 |
| 阿里云 | 否 | — | — | — | 404 | 否 |
| 腾讯云 | 否 | — | — | — | 404 | 是 |
| 华为云 | 否 | — | — | — | **200，真实 METADATA**（2474 B，首行 `Metadata-Version: 2.4`） | 否 |
| 南京大学 NJU | 否 | — | — | — | **200，真实 METADATA**（2474 B） | 是 |

三个非显然的发现：

1. **PEP 691 支持率仅 4/8，且支持者全部停留在 api-version 1.1，落后于上游的 1.4。** 这意味着依赖 1.4 新增字段的客户端行为在镜像上不可假定。
2. **PEP 658 出现了「JSON 里不声明，但旁挂文件实际存在」的错位。** 华为云与 NJU 的 `.metadata` 返回 200 且内容是真实的 `METADATA`（已验证首行与字节数与上游一致），但它们连 PEP 691 都不支持，因此客户端**无从发现**这些文件可用。反过来 TUNA/USTC/BFSU 支持 PEP 691 却在响应里不含 `core-metadata`，且旁挂文件 404。**结论：目前没有任何国内镜像能让 uv 走上 PEP 658 快路径。**
3. **`/pypi/<name>/json`（非 PEP，PyPI 私有 API）支持率 5/8**，其中 TUNA/腾讯/BFSU/NJU 可用且响应可解析。这个 API 不应被依赖——它不在任何 PEP 中。

**PEP 658 缺失的实际代价【实测】**：对 `flask`+`pandas`+`requests` 做 `uv pip compile`，用独立冷缓存分别针对四个索引测量。结果四者的缓存产物规模几乎相同（3.13–3.27 MB，74 个文件），`archive-v0` 与 `sdists-v9` 均为 0 字节——**即无论索引是否提供 PEP 658，uv 在此用例下都没有为取元数据而下载完整 wheel**。耗时 pypi.org 26,632 ms、TUNA 1,937 ms、华为 1,133 ms、阿里 1,045 ms，差异由网络主导而非协议。

**因此本提案不把 PEP 658 列为镜像选择的硬指标**（这是实测修正了动笔前的预期：原以为缺 PEP 658 会导致回落到下载整个 wheel 取元数据）。待验证：在含大量 sdist-only 依赖的项目上，PEP 658 缺失是否会显著放大下载量。验证方法：构造一个显式依赖若干仅发布 sdist 的包的 `requirements.in`，对 pypi.org 与 TUNA 分别用冷缓存 `uv pip compile`，比较 `sdists-v9` 目录字节数。

### 5.3 镜像与上游的哈希一致性与同步延迟【实测】

**测量方法**：对 `numpy` 与 `pandas` 抓取每个镜像的 `/simple/<name>/` HTML，用正则提取每个 `<a href>` 的文件名与 `#sha256=` 片段，以 pypi.org 为基线做逐文件比对，统计 (a) 文件条目总数、(b) 能解析出的最高版本、(c) 与基线同名文件的 sha256 比较数与不一致数、(d) 基线有而镜像缺的文件数。测量时间 2026-09-13 约 21:55 CST。

| 镜像 | numpy 文件数 | numpy 最新 | numpy sha 比较/不一致/缺失 | pandas 文件数 | pandas 最新 | pandas sha 比较/不一致/缺失 |
| --- | --- | --- | --- | --- | --- | --- |
| pypi.org | 4232 | 2.5.3 | 基线 | 2393 | 3.0.5 | 基线 |
| TUNA | 4232 | 2.5.3 | 4232 / **0** / **0** | 2393 | 3.0.5 | 2393 / **0** / **0** |
| 阿里云 | 4232 | 2.5.3 | 4232 / **0** / **0** | 2393 | 3.0.5 | 2393 / **0** / **0** |
| USTC | 4232 | 2.5.3 | 4232 / **0** / **0** | 2393 | 3.0.5 | 2393 / **0** / **0** |
| 腾讯云 | 4232 | 2.5.3 | 4232 / **0** / **0** | 2393 | 3.0.5 | 2393 / **0** / **0** |
| 华为云 | 4232 | 2.5.3 | 4232 / **0** / **0** | 2393 | 3.0.5 | 2393 / **0** / **0** |
| BFSU | 4232 | 2.5.3 | 4232 / **0** / **0** | 2393 | 3.0.5 | 2393 / **0** / **0** |
| NJU | 4232 | 2.5.3 | 4232 / **0** / **0** | 2393 | 3.0.5 | 2393 / **0** / **0** |

**八个镜像、6,625 个文件条目、零不一致、零缺失、最新版本完全一致。** 这是一个强结论，但要正确理解它的范围：它证明**在测量时刻**这些镜像是上游的忠实副本，**不证明**任何时刻都是。TUNA 官方说明「PyPI 镜像在每次同步成功后间隔 5 分钟同步一次」【一方，https://mirrors.tuna.tsinghua.edu.cn/help/pypi/】——即存在同步窗口。

一个重要的结构性观察【实测】：**所有镜像的 `<a href>` 都是相对路径 `../../packages/<hash-path>/<file>#sha256=...`**，且 hash-path 与上游完全一致（例如 certifi 的 `a3/c2/24167ea.../certifi-2026.7.22.tar.gz`）。这说明这些镜像是**路径同构的完整镜像**，制品由镜像自己的域名提供。这对设计有直接影响：镜像替换可以是纯前缀替换，且**索引里的 sha256 与上游逐字节相同**，因此「用上游哈希校验镜像制品」是可行的（§7.2 已实测验证）。

### 5.4 延迟实测

**测量方法**：对每个镜像的 `/simple/numpy/`（一个大页面，上游 2.31 MB、镜像 1.32 MB）连续请求 5 次，记录每次墙钟耗时，取中位数/最小/最大。测量时间 2026-09-13 约 21:50 CST。注意镜像页面比上游小约 43%——上游返回的 HTML 含额外属性（`data-core-metadata` 等）。

| 镜像 | 中位数 | 最小 | 最大 | 失败 | 响应字节 |
| --- | --- | --- | --- | --- | --- |
| **BFSU** | **82 ms** | 75 ms | 118 ms | 0 | 1,320,021 |
| 华为云 | 203 ms | 196 ms | 222 ms | 0 | 1,368,179 |
| USTC | 221 ms | 202 ms | 293 ms | 0 | 1,320,021 |
| TUNA | 343 ms | 326 ms | 353 ms | 0 | 1,320,021 |
| 腾讯云 | 398 ms | 288 ms | 523 ms | 0 | 1,320,021 |
| NJU | 891 ms | 738 ms | 1003 ms | 0 | 1,320,021 |
| pypi.org | 1774 ms | 1440 ms | 3481 ms | 0 | 2,311,480 |
| 阿里云 | 2420 ms | 2300 ms | 2549 ms | 0 | 1,320,023 |

**最快与最慢镜像相差 29 倍，且阿里云比上游 pypi.org 还慢。** 这直接印证了 osdk 既有设计的正确性：`source/select.rs` 的自动测速不是可选优化，而是必需——任何硬编码的镜像优先级都会在某些网络下选错。**本表的数字不应写入代码作为默认优先级**，它只是「差异确实巨大」的证据。

### 5.5 解释器侧：现有三源与 python-build-standalone 镜像现状【实测】

**现有三源可达性**（测量方法：请求 tag `20260814` 的 `SHA256SUMS`，记录状态、耗时、行数、字节数）：

| 源 id | 结果 | 耗时 | 内容 |
| --- | --- | --- | --- |
| `astral`（`releases.astral.sh`） | 200 | — | 124,665 bytes |
| `gh-proxy`（`gh-proxy.com`） | 200 | — | 同上 |
| `github`（`github.com`） | 200 | — | 同上 |

三源均可达且返回同一份 `SHA256SUMS`。

**新发现：存在两个未被 osdk 使用的高质量 PBS 镜像**【实测】。检索国内镜像站的 `github-release` 服务后确认：

| 镜像 | tag 20260814 | tag 20250317 | tag 20240415 | tag 20260601 | 不存在的 tag 20990101 | `SHA256SUMS` 与上游逐字节相同 |
| --- | --- | --- | --- | --- | --- | --- |
| `mirror.nju.edu.cn/github-release/astral-sh/python-build-standalone/` | 200 | 200 | 200 | 404 | 404 | **是**（124,665 B，trim 后完全相等） |
| `mirrors.ustc.edu.cn/github-release/astral-sh/python-build-standalone/` | 200 | 200 | 200 | 404 | 404 | **是**（同上） |

两点需要说明：`20260601` 返回 404 是**上游本身没有这个 tag**（PBS 的发布 tag 是不规则日期），不是镜像缺失；不存在的 tag 返回 404 而非被代理到上游，说明这两个是**真实同步的镜像而非透明代理**——这一点很重要，代理型镜像会把上游错误伪装成成功。

同时确认**清华 TUNA 的 `github-release` 服务不覆盖 astral-sh**：`https://mirrors.tuna.tsinghua.edu.cn/github-release/` 返回 200 且列出 FreeCAD/Homebrew/PowerShell/conda-forge/denoland/git-for-windows 等目录，但 `/github-release/astral-sh/` 返回 **404**。因此不能把 TUNA 加入 PBS 源列表。

**建议**：在 `python.rs:67 default_sources()` 中新增 `nju` 与 `ustc` 两档 Mirror。因为 `SHA256SUMS` 逐字节等同上游、且校验和与资产选择共用同一次请求（python.rs:284 的注释），新增这两源**不扩大信任面**——若镜像返回篡改的 `SHA256SUMS`，其内容会与上游不一致；若返回篡改的制品，哈希校验会失败。这是 §7 fail-closed 论证的一个实例。

至于 uv 的 `UV_PYTHON_INSTALL_MIRROR`：由于 osdk 自己管解释器且**已优于 uv**（三/五档源 + 自动测速失效转移 vs 单个前缀替换），osdk 应把自己安装的解释器通过 `--python <path>` 交给 uv，并设置 `UV_PYTHON_DOWNLOADS=never` 阻止 uv 自行下载解释器。这避免了两套解释器管理并存——本文所有 uv 实测都是在 `UV_PYTHON_DOWNLOADS=never` 下完成的，证明该模式可行。

### 5.6 依赖混淆：镜像与 `--index-strategy` 混用的风险

这是本节最需要讲清的安全问题。

uv 默认 `first-index` 的设计意图是：私有索引里有的包**永远**从私有索引装，绝不从 PyPI 装【官方】。而**镜像的语义与私有索引根本不同**——镜像是 PyPI 的完整副本，不是「另一个包源」。

危险的组合是把镜像配成 `--extra-index-url`：

- `--extra-index-url` 在 uv 中**优先级高于默认索引**【官方】。
- 于是「PyPI 镜像作为 extra index + 私有索引作为 default index」会让镜像抢先——一个上游 PyPI 上的同名包会覆盖私有包。这正是 `torchtriton` 类攻击的形状。
- 更隐蔽的是：即便镜像本身完全诚实（§5.3 实测证明它们是），它作为 PyPI 的完整副本**必然包含 PyPI 上的同名恶意包**。镜像的诚实性不能防御依赖混淆，因为被镜像的内容本身就是攻击载荷。

**因此 osdk 的设计必须把「镜像」和「额外索引」建模为两个不同概念**：

| osdk 概念 | 映射到 uv | 语义 |
| --- | --- | --- |
| PyPI 镜像 | `--default-index <mirror>` | 替换默认索引。多个镜像之间由 osdk 测速择一，**不同时传给 uv** |
| 额外索引（私有源） | `--index <url>` | 附加索引。osdk 不参与其测速与替换 |
| 包→索引钉定 | uv 的 `explicit = true` 索引 | 某包只从指定索引取 |

并且：**osdk 绝不把 PyPI 镜像传成 `--extra-index-url` 或 `--index`。** 镜像是 default-index 的候选，仅此一种用法。这条规则应有测试守护（§7.4 验收清单第 6 项）。

`--index-strategy` 的处理：osdk 默认**不传**该参数（即用 uv 的安全默认 `first-index`）。`unsafe-best-match` 与 `unsafe-first-match` 只在用户通过 osdk 配置显式开启时传递，且开启时应当告警。理由：这两档的风险是「安装了错误来源的同名包」，属于不可逆后果，不适合作为加速手段的副作用被默默启用。

---

## 6. 缓存统一管理

### 6.1 uv 缓存的真实布局【实测】

**测量方法**：设 `UV_CACHE_DIR` 到一个空目录，创建 venv 后 `uv pip install requests`（TUNA 索引），然后统计缓存各顶层目录的文件数与字节数。

```
archive-v0        117 files   1,818,510 bytes    ← wheel 解包后的内容（hardlink 源）
simple-v25         10 files     897,680 bytes    ← 索引元数据（rkyv 二进制 + .lock）
interpreter-v4      2 files       4,532 bytes    ← 解释器探测结果
wheels-v6          20 files       6,755 bytes    ← wheel 元数据/指针
sdists-v9           2 files           0 bytes    ← sdist 构建产物（本例未用到）
.lock / CACHEDIR.TAG / .gitignore
```

`uv cache size` 报告 2,760,289 bytes。

`simple-v25` 内部是**每个包一个 `.rkyv` + 一个 `.lock`**（实测：`requests.rkyv` 72,024 B、`charset-normalizer.rkyv` 708,544 B、`certifi.rkyv` 40,672 B、`idna.rkyv` 20,776 B、`urllib3.rkyv` 55,664 B）。`.rkyv` 是零拷贝反序列化格式，`.lock` 是**每包粒度的锁**——这个设计细节对 §6.6 的并发讨论有直接意义。

目录名全部带版本后缀（`-v0` / `-v25` / `-v9`），这是 uv 处理缓存格式演进的方式：**升版即弃旧目录，不做迁移**。osdk 的 GC 设计必须知道这一点（§6.4）。

### 6.2 为什么不把 wheel 纳入 osdk 的 store

osdk 的 `store/mod.rs` 是文件级 blake3 CAS，`store/link.rs:132 materialize()` 把对象 hardlink/reflink/copy 进安装目录。表面上看，wheel 解包后的文件也可以进这个 CAS，从而「统一」。

**不应这么做，理由是三条职责边界：**

第一，**键的语义不同**。osdk store 的键是文件内容的 blake3。uv 的 `archive-v0` 键是「wheel 身份 + 目标解释器兼容性」的派生值（实测目录名如 `gni9tQFqJoxXUZ_Q`）。同一个 wheel 在不同 Python 版本下解包结果可以不同（`.pyc`、`RECORD`、脚本 shebang），文件级 CAS 会把这些差异摊平成大量近似对象，失去 wheel 级别的复用。

第二，**生命周期不同**。osdk store 的 GC 依据是「install manifest 引用的活集」（`store/manifest.rs:MANIFEST_FILE`，`osdk prune`）。venv 不是 osdk 的 install，osdk 无从知道哪些 venv 还活着——用户可以随手 `rm -rf .venv`。把 wheel 放进 osdk store 会立刻产生一个无法计算活集的 GC 问题。

第三，**uv 已经在正确地做这件事**【实测】。测量方法：同卷（E:）下建三个 venv 并各装 requests，然后对 `certifi/cacert.pem` 执行 `fsutil hardlink list` 并读 `LinkType`。结果：

```
LinkType = HardLink
链接列表（4 条，同一 inode）：
  \...\uv-lab\cache\archive-v0\gni9tQFqJoxXUZ_Q\certifi\cacert.pem
  \...\uv-lab\proj\.venv\Lib\site-packages\certifi\cacert.pem
  \...\uv-lab\proj\.venv2\Lib\site-packages\certifi\cacert.pem
  \...\uv-lab\proj\.venv3\Lib\site-packages\certifi\cacert.pem
```

三个 venv 与缓存对象共享单一 inode。重复实现只会得到一个更差的版本。

**分层结论**：

| 层 | 归属 | 键 | 链接策略 | GC 依据 |
| --- | --- | --- | --- | --- |
| 解释器（CPython 归档解包） | **osdk store** | 文件内容 blake3（`store/mod.rs:306`） | `store/link.rs:132` 的 hardlink→reflink→copy | install manifest 活集（`osdk prune`） |
| Python wheel / sdist / 索引元数据 | **uv 缓存（目录归 osdk 管）** | uv 内部键 | uv `--link-mode` | `uv cache prune` |

### 6.3 目录布局

沿用 `cache/mod.rs:17 downstream_root()` 的 `<cache>/pkg` 约定。本机实测 `osdk cache env` 已输出 13 个变量（`PIP_CACHE_DIR=E:\osdk-data\cache\pkg\pip` 等），新增：

```
<cache>/pkg/
  pip/            ← 已有（cache/mod.rs:40）
  uv/             ← 新增：UV_CACHE_DIR
```

Python 工具与虚拟环境不进 `pkg/`（它们是安装物而非缓存）：

```
<data>/installs/
  python/<version>/                 ← 已有
  pypi/<package>/<version>/<install-id>/   ← 新增（P4，工具安装）
<data>/uv-tools/                    ← 新增（P4）：UV_TOOL_DIR
```

**关键约束**：`pypi` 这个目录名**必须**加入 `tool.rs:549 DYNAMIC_NAMESPACES`，否则 `is_dynamic_install_directory()`（tool.rs:590）返回 false，inventory 扫描不会进入该子树，安装就对扫描**完全隐形**。tool.rs:586-589 的注释已经把这个坑写明，且 AGENTS.md 记录 `npm-global` 在第一版实现里正是这样被漏掉的。

### 6.4 键设计、GC 与 prune 语义

**键设计**。osdk 侧只需为「uv 二进制本身」设计键，wheel 层的键归 uv：

- uv 二进制：走 `store/` 现有路径，键为 blake3（`store/mod.rs:306`），receipt 绑定按 `native_tool.rs:47 NativeToolReceipt`（含 `provider` / `runtime` / `bins[].sha256`）。
- 制品哈希算法：**索引给的是 sha256，osdk store 用 blake3**。这两者不冲突且都必要——sha256 用于对照索引声明（`pipeline/verify.rs:105 verify_file` 接受 `HashAlgo`），blake3 用于 CAS 寻址。不要试图统一成一个。

**链接与退化阶梯**。复用 `store/link.rs` 已有逻辑：`same_filesystem()`(:62) 判定后，`Auto` 走 hardlink → reflink → copy（:145-159）。uv 侧对应 `--link-mode`，osdk 的 `Settings::link_mode`(config/mod.rs:54) 应映射过去：`Auto`→不传（让 uv 自选）、`Hardlink`→`hardlink`、`Reflink`→`clone`、`Copy`→`copy`。**`symlink` 不映射**——`store/link.rs:6-8` 的注释已论证过为何不自动选 symlink（Windows 无特权时失败，且会让 `realpath` 自身可执行文件的工具困惑）。

**GC / prune 语义**，三个动作职责分明：

| 命令 | 现状 | 应有语义 |
| --- | --- | --- |
| `osdk cache clean` | 只删 `downloads`（实测帮助文本） | 增加「同时清 uv 缓存」，即调 `uv cache clean` |
| `osdk prune` | store GC，依据 install manifest 活集 | 不变。**不触碰 uv 缓存** |
| 新增：`osdk cache prune` | — | 调 `uv cache prune`，清理悬挂条目与缓存环境 |

**一个必须注意的交互**：uv 的缓存目录带版本后缀，升级 uv 后旧目录（如 `simple-v24`）会变成永久垃圾，`uv cache prune` 是否清理它需要验证。**待验证**：uv 升级跨缓存版本后，旧版本目录是否被 `uv cache prune` 回收。验证方法：用两个相邻 uv 版本（跨 `simple-v*` 变更）依次在同一 `UV_CACHE_DIR` 下执行安装，然后跑 `uv cache prune` 并比对目录列表。若不回收，osdk 需自行识别并清理孤立的版本化目录——但**必须**只删已知前缀模式（`archive-v*` / `simple-v*` / `wheels-v*` / `sdists-v*` / `interpreter-v*`）中版本号低于当前的目录，绝不做通配删除。

### 6.5 Windows 具体坑

| 坑 | 实测/依据 | 应对 |
| --- | --- | --- |
| **跨卷硬链接失败** | 【实测】缓存在 E:、venv 在 C: 时 uv 报 `os error 17`「系统无法将文件移到不同的磁盘驱动器」，随后 `falling back to copy` 并发 warning 建议设 `UV_LINK_MODE=copy` | osdk 用 `store/link.rs:62 same_filesystem()` 预判：若 uv 缓存与目标 venv 跨卷，**主动**传 `--link-mode=copy` 以避免每次安装刷一屏 warning。这是 osdk 能提供的真实价值——用户不必自己诊断 |
| **同卷硬链接正常** | 【实测】同卷下 4 条路径共享 inode，`LinkType=HardLink` | 无需干预 |
| **长路径（MAX_PATH 260）** | site-packages 下的深层包路径 + `<cache>/pkg/uv/archive-v0/<key>/...` 容易超限 | **待验证**：osdk 数据根位于深路径时 uv 是否失败。验证方法：把 `UV_CACHE_DIR` 设到一个约 200 字符的路径下，安装一个已知深层结构的包（如 `jupyterlab`），观察是否报路径错误。应对方向是缩短 osdk 侧前缀（`pkg/uv` 已很短）并在 doctor 中检查 `LongPathsEnabled` 注册表项 |
| **文件占用 / 杀软** | Windows 上运行中的 `python.exe` 与已加载的 `.pyd` 会被独占；实时防护会在写入后立即扫描新文件 | uninstall/清理路径必须容忍 `ERROR_SHARING_VIOLATION` 并给出可操作报错（「有进程正在使用该环境」），而非重试到超时。`reshim-hang-and-state-recovery-2026-09-13` 已确立「fail-closed 但要留恢复出路」的原则 |
| **大小写不敏感** | NTFS 默认大小写不敏感但保留大小写；PEP 503 规范化会把 `PyPDF2` 变成 `pypdf2`，而 uv 输出规范化名、pip 输出原名【官方】 | osdk 展示层统一用 PEP 503 规范化名（与 uv 一致），并且**不要**用文件系统是否存在某路径来判断包是否安装——用 uv 的 `pip list` 输出 |
| **可执行文件复制而非符号链接** | 【官方】uv 文档明示：tool 可执行文件在 Unix 上 symlink，**在 Windows 上 copy** | osdk 的 shim 生成逻辑不要假定 venv `Scripts/` 下是符号链接 |
| **解释器 patch 升级后工具/venv 失效** | 【源码】mise 的对策 `fix_venv_python_symlink()`（pipx.rs:1069）依赖符号链接改写，**在 Windows 上是空实现**（:1133-1136）——即同类产品在此平台上没有解 | osdk 不走符号链接路线。用 `NativeToolReceipt`（native_tool.rs:47）把 runtime 身份绑进 receipt，升级后 receipt 校验失败即要求显式重建，行为在 Windows 与 Unix 上一致。这是 osdk 相对 mise 的一处实质优势（§4.5.3） |
| **「查得到」≠「启动得了」** | 【源码】mise 在 venv 与 pipx 两处都用 spawnable 探测替代普通查找，注释点明 `uv.ps1` / shebang-only `uv` 会骗过 `which`（venv.rs:309-314） | osdk 探测 uv 用「能否 spawn」判据（§7.4 第 27 项） |

### 6.6 并发与锁

uv 自身已有分层锁【实测】：缓存根有 `.lock`，`simple-v25` 下**每个包一个 `.lock`**。osdk 不应在其外再加一把粗粒度锁——那会把 uv 的细粒度并发退化成串行。

osdk 侧只需锁自己的东西，且用已有机制：`dirs.rs:319 lock_dir()` + `fslock`（`reshim-hang-and-state-recovery-2026-09-13` 已确认 fslock 随进程退出由内核释放，强杀不会留下死锁）。具体：

- uv 二进制的安装/升级：走 `native_tool.rs` 的 identity 限定锁。
- Python 工具安装（P4）：同上，按 `InstallIdentity` 加锁。
- venv 创建与包安装：**不加 osdk 锁**，由 uv 自己的锁负责。

### 6.7 离线模式

`Settings::offline`(config/mod.rs:66) 传导为 `UV_OFFLINE=1`。实测确认 warm cache 下 `uv pip install --offline requests` 能成功解析（3 ms）并安装 5 个包。

**pip 路径的离线传导**【实测 2026-09-14】：`PIP_NO_INDEX=1` 下 `pip install requests` → `ERROR: Could not find a version that satisfies the requirement requests (from versions: none)`。即离线语义在回退路径上同样可实现（另有 `--no-index` 参数形式）。

**第三轮改写：原「离线且 uv 未安装时不得回落到系统 pip」这句必须拆开看，它把两件不同的事混成了一件。**

| 行为 | 判定 | 理由 |
| --- | --- | --- |
| 回落到**系统** pip（`python -m pip install` 打到系统解释器的 site-packages，或 `pip install --user`） | **仍然禁止** | 污染全局环境、无隔离、卸载困难、与 osdk 的安装模型冲突。uv 官方也不支持 `--user`【官方】 |
| 使用 **osdk 创建的那个 venv 内自带的** pip | **可接受** | 完全隔离在该 venv 内；`PIP_INDEX_URL` / `PIP_NO_INDEX` / `--require-hashes` 均可由 osdk 注入并已实测生效（§4.5.7）；`PIP_CACHE_DIR` 早已由 `cache/mod.rs:40` 纳管 |

**这个区分是原论证含混的根源。** 原文写「回落到系统 pip」，但实际担心的是「绕过门禁」；而绕过门禁的原因是「不受管的 pip」，不是「pip 这个工具」。一旦 pip 运行在 osdk 创建的 venv 内、且环境变量由 osdk 注入，它就和受管的 uv 处于同一地位——受同一套门禁约束，只是能力较弱（无 universal resolution、解析较慢）。

因此离线模式的正确行为是：

1. uv 可用 → `UV_OFFLINE=1`。
2. uv 不可用但目标 venv 已存在 → 用该 venv 的 pip + `PIP_NO_INDEX=1`。
3. uv 不可用且目标 venv 不存在 → `python -m venv` 不需要网络，可正常创建（stdlib 自带 pip，无需下载 seed 包）；随后同 2。这是回退路径在离线场景下的一个**实际优势**：uv 路径若要 `--seed` 反而需要网络取 pip/setuptools/wheel。
4. 仅当显式 `--require-uv` 且 uv 不可用时 → fail-closed，提示需先在联网环境安装 uv。

---

## 7. 安全与 fail-closed 门禁

### 7.1 镜像场景下哈希校验不被削弱的机制

链条是这样闭合的：

1. **索引是哈希的来源**。`/simple/` 页面的 `#sha256=` 片段（或 PEP 691 JSON 的 `hashes.sha256`）由索引提供。
2. **§5.3 实测证明镜像的哈希与上游逐字节相同**（6,625 个文件条目零差异）。
3. **§5.3 实测还证明镜像路径与上游同构**（`../../packages/<同样的 hash-path>/<同样的文件名>`）。
4. 因此**「从镜像取索引、用索引里的哈希校验从镜像取的制品」与「全走上游」在制品完整性上等价**——除非镜像同时篡改索引与制品且保持自洽。
5. 防御第 4 点的手段是**锁文件**：`uv.lock` / `requirements.txt --hash` 记录了哈希。一旦锁定，镜像即使自洽地篡改也会与锁不符。

**因此门禁的核心是：镜像可以随便换，但哈希的来源必须能被独立固定。** 具体规则：

- **`--no-verify-hashes` 永不透传。** 这是唯一能关掉校验的开关，osdk 不提供任何路径让它出现在 uv 命令行上。
- **`Settings::require_checksums`(config/mod.rs:62) 为真时传 `--require-hashes`。**
- **`--allow-insecure-host` 永不透传。** 它关闭对指定主机的 TLS 校验。
- **索引 URL 必须是 HTTPS。** 复用 `source/env.rs:85 validate_https_endpoint()`——它已经在做这件事，且 `cargo_package.rs:42 validate_registry_index()` 是同类校验的另一个先例（要求 canonical sparse HTTPS、无凭据、无 query、无 fragment）。PyPI 索引校验应照此写，特别是**拒绝 URL 里内嵌凭据**（`https://user:pass@mirror/simple`），因为那会让凭据进入日志与配置文件。
- **TLS 根证书来自系统信任库。** AGENTS.md 记录了 reqwest 0.13 删除内置 webpki 根证书 feature 这一用户可见行为，并要求改动 TLS 依赖后实测 badssl.com 的四种坏证书。uv 侧有 `--system-certs`（实测其帮助文本：「Whether to load TLS certificates from the platform's native certificate store」）。**osdk 不应默认传 `--system-certs`**，除非验证过它在企业 CA 场景下确实必要——待验证：uv 默认（不传 `--system-certs`）时企业自签 CA 是否生效。验证方法：在配置了企业根 CA 的 Windows 机器上，对一个用该 CA 签发证书的私有索引执行 `uv pip install`，比较传与不传 `--system-certs` 的结果。

#### 7.1.1 pip 路径的等价门禁清单（第三轮新增）

既然 stdlib venv 回退被接受（§4.5.7），回退路径必须受同一套门禁约束。**回退是路径切换，不是门禁豁免。** 以下为逐条对应，pip 侧行为均为本轮实测【实测 2026-09-14】：

| 门禁项 | uv 侧 | pip 侧 | 实测状态 |
| --- | --- | --- | --- |
| 哈希强制 | `--require-hashes` | `--require-hashes` | **已验证 fail-closed**，见 §7.2.1 |
| 关闭校验的开关永不透传 | `--no-verify-hashes` 禁传 | pip 无等价「关闭校验」开关；但**禁止在 `--require-hashes` 应开启时省略它** | 规则 |
| 索引替换 | `UV_DEFAULT_INDEX` | `PIP_INDEX_URL` | **已验证生效**（`pip config debug` 的 `env_var` 段可见；日志 `Looking in indexes: …tuna…`） |
| 离线 | `UV_OFFLINE=1` | `PIP_NO_INDEX=1`（或 `--no-index`） | **已验证** |
| 索引必须 HTTPS、拒绝内嵌凭据 | `validate_https_endpoint()` | **同一个校验函数，在注入 `PIP_INDEX_URL` 前执行** | 规则；复用 `source/env.rs:85` |
| TLS 降级开关禁传 | `--allow-insecure-host` 禁传 | `--trusted-host` **禁传**（等价危害：对指定主机跳过 TLS 校验） | 规则 |
| 缓存纳管 | `UV_CACHE_DIR` | `PIP_CACHE_DIR` | 已有（`cache/mod.rs:40`） |
| 屏蔽用户全局配置 | uv 不读 `pip.conf`【官方】 | **`PIP_CONFIG_FILE` 指向受控文件** | 本轮实测即用此法隔离；osdk 应据此确保用户全局 `pip.conf` 不能覆盖 osdk 注入的索引与门禁 |

最后一行值得强调：uv 天然不读 `pip.conf`【官方】，而 pip 会读它，且用户全局配置的优先级可能高于环境变量中的某些项。因此**回退路径必须显式设置 `PIP_CONFIG_FILE`**，否则用户遗留的 `pip.conf`（例如指向一个不受信任的索引）会悄悄改变行为。这是 pip 路径独有的、uv 路径不存在的门禁要求。

**一个 pip 侧不存在的能力及其后果**：pip **没有 `--index-strategy` 的等价物**。uv 用 `first-index` 默认防依赖混淆（§5.6），pip 的 `--extra-index-url` 语义是「把所有索引的候选合并后取最优版本」，近似 uv 的 `unsafe-best-match`——**即 pip 的默认行为就是 uv 明确标为 unsafe 的那一档**。对 §7.3 依赖混淆门禁的影响见 §7.3 的第三轮补充。

### 7.2 镜像返回不一致内容时的行为【实测】

这不是推演，本次做了实机验证。

**测量方法**：(a) 从 pypi.org 的 PEP 691 JSON 取 `certifi-2026.7.22-py3-none-any.whl` 的上游 sha256 = `62f22742b58a1a33014a2b6b706588a8d7e2a88ae7bd1a6ebe8c992928483775`；(b) 写成 `requirements.txt` 的 `certifi==2026.7.22 --hash=sha256:<upstream>`；(c) 把默认索引指向 TUNA，用 `--require-hashes --no-cache` 安装；(d) 把哈希改成 64 个 `0` 重试。

结果：

- **(c) 成功**：`Resolved 1 package in 354ms / Prepared 1 package in 141ms / Installed 1 package`。即**用上游哈希校验从镜像下载的制品是可行的**——这实证了 §7.1 第 4 点。
- **(d) 失败且 fail-closed**：
  ```
  × Failed to download `certifi==2026.7.22`
  ╰─▶ Hash mismatch for `certifi==2026.7.22`
      Expected: sha256:0000...0000
      Computed: sha256:62f22742b58a1a33014a2b6b706588a8d7e2a88ae7bd1a6ebe8c992928483775
  ```
  非零退出、无安装、报错含期望与实际值。

**结论**：uv 的哈希门禁行为正确且是 fail-closed 的。osdk 的职责不是重新实现校验，而是**确保这个门禁始终开着**（§7.1 的规则）并把失败原因（含「当前使用的是哪个镜像」）呈现给用户——后者是 osdk 能补充的价值，因为 uv 的错误里没有「你在用 TUNA 镜像」这个信息。

#### 7.2.1 pip 路径的同一验证，以及一个必须记录的方法学教训【实测 2026-09-14】

**测量方法**：与 §7.2 同构。取上游 sha256（certifi 2026.7.22 = `62f22742b58a1a33014a2b6b706588a8d7e2a88ae7bd1a6ebe8c992928483775`），索引指向 TUNA，用 `--require-hashes` 安装；再把哈希换成 64 个 `0` 重试。环境隔离见 §4.5.7。

结果：

- **正确哈希**：安装成功，**exit=0**。
- **错误哈希**：`ERROR: THESE PACKAGES DO NOT MATCH THE HASHES FROM THE REQUIREMENTS FILE.` 并列出 Expected/Got，**exit=1**，无安装。

即 **pip 的哈希门禁与 uv 同样是 fail-closed 的**，回退路径不削弱这一层防护。

##### 方法学教训：第一次测这条时拿到了 exit=0 的假阴性

**原因**：错误哈希的用例复用了前一步已经装过 certifi 的同一个 venv。pip 判定该包已满足需求，**跳过下载，因而根本没走到校验路径**。于是「错误哈希也 exit=0」这个看似惊人的结论，实际测的是「pip 的 already-satisfied 短路」。

**这与 AGENTS.md 记录的坑属同一类**：那里是「参数写错、环境被破坏时 osdk 打印 usage 后以退出码 2 退出，耗时约 12 ms，看起来快得惊人，实际测的是报错路径」，因此要求「基准必须断言退出码为 0 且输出非空」。此处的形态是镜像对称的——**看起来通过，实际测的是另一条路径**。

两者的共同教训是：**一个测试必须自证它落在了被测机制之内。** 判断标准是「这个断言的结果会不会随被测机制改变」——如果把 pip 的哈希校验整段删掉，复用 venv 的那个用例照样 exit=0，说明它与被测机制脱钩，等于空测试。

**因此 §7.4 的相关测试项显式要求「每个哈希用例使用独立的干净 venv」**，并额外断言输出中出现下载行（证明确实走了下载与校验路径）。这条要求同样适用于 uv 路径——uv 也有缓存与 already-satisfied 短路，§7.2 的原实测之所以没踩坑，是因为当时用了 `--no-cache` 且 venv 是新建的，属于侥幸而非设计。

### 7.3 索引混淆的门禁

承 §5.6，落为四条硬规则：

1. PyPI 镜像**只能**映射到 `--default-index`，永不映射到 `--index` / `--extra-index-url`。
2. 同一次调用**只传一个** `--default-index`（osdk 测速择一），不把镜像列表整体倾泻给 uv。
3. `--index-strategy` 默认不传（用 uv 的 `first-index`）。`unsafe-*` 两档仅在 osdk 配置显式开启时传递，且传递时打印告警。
4. 用户显式配置的私有索引（`--index`）**不参与 osdk 的测速与替换**——它不是加速对象，是语义对象。

**第二轮补强：规则 1 有一个真实反例，不是假想风险。** mise 的 `uvx_cmd()` 设置的正是 `.env("UV_INDEX", ...)`（`pipx.rs:810`）【源码】，而 `UV_INDEX` 对应 uv 的 `--index`（额外索引，优先级高于默认索引）。把镜像配成 `pipx.registry_url` 的用户，实际得到的是「镜像优先于 pypi.org 的额外索引」。

这在 mise 的场景下危害有限——`pipx:` 只装独立 CLI 工具，且 `first-index` 会让第一个命中的索引赢，通常正是用户意图。但 osdk 的索引配置会**同时**服务项目依赖安装与工具安装两条路径，前者一旦让镜像盖过私有索引即构成依赖混淆。因此规则 1 从「设计取舍」升格为**必须有测试守护的硬约束**，见 §7.4 第 25 项。这也是本轮唯一一处「同类产品的做法明确不该照抄」的地方。

**第三轮补充：pip 路径上依赖混淆更危险，因为 pip 没有 `--index-strategy`。**

事实对照：

| | uv | pip |
| --- | --- | --- |
| 默认跨索引策略 | `first-index`：只取**第一个含该包的索引**的候选集【官方】 | 把 `--index-url` 与所有 `--extra-index-url` 的候选**合并**后取最优版本 |
| 等价关系 | — | pip 的默认 ≈ uv 的 **`unsafe-best-match`** |
| 可否收紧 | 可（三档可选） | **不可，无此开关** |

也就是说：uv 提供了一个可以关上的门，pip 那扇门根本不存在。**在 pip 路径上把镜像与私有索引同时配置，就是把恶意同名包的替换机会直接交出去。**

**osdk 在 pip 路径上避免依赖混淆的做法**（单一方案）：

1. **只注入 `PIP_INDEX_URL`（单一索引），永不注入 `PIP_EXTRA_INDEX_URL`。** 镜像替换默认索引，语义与 uv 的 `--default-index` 对齐。这样候选集只有一个来源，合并策略无从发生。
2. **需要私有索引时，禁止与 PyPI 镜像并存于 pip 路径。** 若用户同时配置了私有索引与镜像，pip 路径应当：(a) 报错并说明 pip 无法安全地混用多索引；(b) 提示两条出路——装 uv 以获得 `first-index` 保护（`osdk install uv`），或改用 `--index-url` 指向一个已代理了上游的私有索引（由该索引自己承担合并语义与准入控制）。**这是回退路径的一处实质能力缺口，必须显式告知而非静默降级**，与 §4.5.7 「能力差异要讲清」的原则一致。
3. **包→索引钉定在 pip 路径上不可用。** uv 的 `explicit = true` 索引无 pip 等价物；如用户配置了此类钉定而当前走 pip 路径，应报错。

这三条使「pip 路径下永远只有一个索引来源」成为不变量——它比逐项模拟 uv 的策略更简单，也更容易被测试固定（§7.4 第 28 项）。

### 7.4 可执行验收清单

分为「必须新增的测试」与「必须跑通的既有基准」两部分。

**必须新增的测试**

| # | 测试 | 断言 | 为何不能省 |
| --- | --- | --- | --- |
| 1 | `python_index_must_be_canonical_https` | 内嵌凭据 / http / 带 query / 带 fragment 的索引 URL 全部被拒 | 照 `cargo_package.rs:42` 的先例；凭据入配置是不可逆泄露 |
| 2 | `no_verify_hashes_is_never_forwarded` | 遍历所有 osdk→uv 参数构造路径，断言产出的 argv 不含 `--no-verify-hashes` 也不含 `--allow-insecure-host` | 这是唯一能关校验的开关，需要机械保证 |
| 3 | `pypi_mirror_only_becomes_default_index` | 给定含多个镜像的配置，断言 argv 中 `--default-index` 恰好一个、且不出现 `--extra-index-url`；镜像 URL 不出现在 `--index` 之后 | §5.6 的依赖混淆防线，纯靠 review 守不住 |
| 4 | `unsafe_index_strategy_requires_explicit_opt_in` | 默认配置下 argv 不含 `--index-strategy`；显式开启 `unsafe-best-match` 时才出现且伴随告警 | 防止加速需求把安全默认顺手关掉 |
| 5 | `require_checksums_maps_to_require_hashes` | `Settings::require_checksums = true` ⇒ argv 含 `--require-hashes` | 配置与行为的连接需断言 |
| 6 | `offline_setting_maps_to_uv_offline` | `Settings::offline = true` ⇒ 子进程环境含 `UV_OFFLINE=1` | 同上 |
| 7 | `offline_uses_venv_pip_never_system_pip` | 离线且 uv 未安装时：**不**报错而是使用目标 venv 内的 pip 并注入 `PIP_NO_INDEX=1`；断言 argv 中的 python 可执行文件位于该 venv 内，且**不**指向系统解释器、不含 `--user` | **第三轮改写**（原为 `offline_without_uv_fails_closed`）。原测试会把新的正确行为判为失败。区分「系统 pip」（禁止）与「venv 内自带 pip」（可接受）是 §6.7 的核心 |
| 8 | `cross_volume_forces_copy_link_mode` | uv 缓存与目标 venv 跨卷时 argv 含 `--link-mode=copy` | 实测 uv 会 warn 到刷屏；osdk 的价值就在预判 |
| 9 | `link_mode_never_maps_to_symlink` | osdk 任何 `LinkMode` 都不产出 `--link-mode=symlink` | `store/link.rs:6-8` 已论证 symlink 的危害 |
| 10 | `pypi_namespace_is_reachable_from_is_dynamic_install_directory` | `is_dynamic_install_directory("pypi")` 为真 | **最容易犯且最隐蔽的错**。AGENTS.md 记录 `npm-global` 正是这样被漏掉；漏掉表现为「装了却用不了」，不报错 |
| 11 | `uv_runtime_version_is_pinned_in_lockfile` | `osdk.lock` 中记录 uv 版本；uv 版本变化导致 receipt 校验失败并要求显式重装 | 防版本漂移（§4.3） |
| 12 | `uv_lock_is_referenced_by_digest_not_copied` | `osdk.lock` 只含 `uv.lock` 的 kind/format/sha256，不含其内容 | 两份真相源必然发散（§4.4） |
| 13 | `hash_mismatch_error_names_the_active_source` | 制品哈希不符时，osdk 的错误消息包含当前使用的索引 id | uv 的原始错误缺这个信息（§7.2） |
| 14 | `python_pbs_mirror_sha256sums_match_official` | 对 `nju` / `ustc` 源，`SHA256SUMS` 与 official 源的内容一致（可用固定 fixture 离线断言解析等价性） | §5.5 新增源的前提条件 |
| 15 | `venv_scripts_are_not_shimmed` | venv 的 `Scripts/` 下的 `python`/`pip` 不被 osdk shim 接管 | 插 shim 会破坏 `sys.prefix` 推断（§4.3） |
| 16 | `uv_cache_prune_does_not_touch_osdk_store` | 调 `uv cache prune` 前后 osdk store 对象数不变 | 职责边界（§6.2） |
| 17 | `stale_uv_cache_version_dirs_use_prefix_allowlist` | 清理孤立的版本化缓存目录时，只匹配已知前缀（`archive-v*` 等），不做通配删除 | 通配删除用户缓存目录是不可逆事故（§6.4） |

**第二轮新增（由 mise 的实现逼出来的三项）**

| # | 测试 | 断言 | 为何不能省 |
| --- | --- | --- | --- |
| 25 | `mirror_never_becomes_uv_index_env_or_flag` | 遍历所有镜像配置路径，断言产出的 argv 与子进程环境中**都不出现** `--index` / `--extra-index-url` / `UV_INDEX` / `UV_EXTRA_INDEX_URL` 承载镜像；镜像只能出现在 `--default-index` / `UV_DEFAULT_INDEX` | 这不是假想风险：mise 的 `pipx.rs:810` 正是 `.env("UV_INDEX", ...)`【源码】。同一个直觉错误很容易在 osdk 重演，而后果是依赖混淆（§7.3） |
| 26 | `missing_uv_falls_back_to_stdlib_venv_with_visible_notice` | uv 不可用时，`osdk python venv` **成功创建** venv（argv 含 `-m venv`），且输出中包含：当前走 pip 路径的说明、能力差异、`osdk install uv` 提示。**同时**断言未使用系统解释器的 site-packages | **第三轮反转**（原为 `missing_uv_fails_closed_without_stdlib_venv_fallback`）。原测试固定的是已被推翻的结论，必须整体替换，否则它会阻止正确实现通过。可见提示是硬要求——静默回退不可接受（§4.5.7） |
| 27 | `uv_detection_requires_spawnable_not_merely_present` | 构造一个只有 `uv.ps1` / 无有效可执行位的 `uv`，断言 osdk 判定为「不可用」并**走回退分支**（而非 fail-closed），且不会提交到 uv 分支后在 spawn 时崩 | **第三轮修正断言的落点**：探测判据不变（仍须 spawnable），但不可用后的动作从 fail-closed 改为回退。mise 在 venv 与 pipx 两处都处理了这个 Windows 坑（`venv.rs:309-314`）；osdk 是 Windows 优先项目，更不能漏 |

**第三轮新增（stdlib 回退被接受后必须补的门禁）**

| # | 测试 | 断言 | 为何不能省 |
| --- | --- | --- | --- |
| 28 | `pip_path_injects_single_index_only` | pip 路径下断言子进程环境含 `PIP_INDEX_URL` 且**不含** `PIP_EXTRA_INDEX_URL`；若配置中同时存在私有索引与 PyPI 镜像，则**报错**并在消息中给出两条出路 | pip 无 `--index-strategy`，其默认合并语义 ≈ uv 的 `unsafe-best-match`。「只有一个索引来源」是 pip 路径的不变量（§7.3 第三轮补充） |
| 29 | `pip_path_enforces_require_hashes_fail_closed` | 错误哈希 ⇒ 非零退出且无安装。**每个用例必须使用独立的干净 venv**，并断言输出含下载行 | 本轮实测踩过这个坑：复用已装过该包的 venv 会让 pip 短路跳过下载，产生 exit=0 的**假阴性**（§7.2.1）。不强制干净 venv，这个测试就是空的 |
| 30 | `uv_path_hash_tests_also_use_clean_venv_and_no_cache` | uv 路径的哈希用例同样使用新建 venv + `--no-cache`，并断言输出含下载/准备行 | uv 也有 already-satisfied 与缓存短路。§7.2 原实测未踩坑属侥幸而非设计（§7.2.1） |
| 31 | `pip_path_forbids_trusted_host_and_sets_config_file` | pip 路径 argv 中**不出现** `--trusted-host`；环境中**必须**设置 `PIP_CONFIG_FILE` 指向受控文件 | `--trusted-host` 是 `--allow-insecure-host` 的 pip 等价物。`PIP_CONFIG_FILE` 是 pip 路径独有要求——uv 不读 `pip.conf`【官方】，pip 会读，用户遗留配置可能覆盖 osdk 注入的索引（§7.1.1） |
| 32 | `venv_creator_is_recorded_and_dispatched` | 用 uv 创建的 venv ⇒ 后续安装走 `uv pip install`；用 stdlib 创建的 ⇒ 走该 venv 的 `python -m pip install`。osdk 记录缺失时，能从 `pyvenv.cfg` 是否含 `uv =` 字段恢复判定 | 两条路径产出的 venv 不等价（§4.5.7 实测表），分派错误会产生难诊断的失败。`uv =` 字段的判别力已由两轮实测确立（uv 路径有、stdlib 路径无） |
| 33 | `seed_semantics_are_normalized_across_paths` | 请求 `--seed` 时，两条路径**结果一致**：pip + setuptools + wheel 三者齐备。断言 stdlib 路径确实补装了 setuptools 与 wheel | 实测 `python -m venv` 自带 pip 但**不带** setuptools/wheel，而 `uv venv --seed` 三者都装。原样透传参数会产出两种不同的环境（§4.5.7） |
| 34 | `relocatable_is_rejected_on_pip_path` | pip 路径下请求 `--relocatable` ⇒ 报错，**不得静默忽略** | `--relocatable` 是 uv 独有，stdlib 无对应物。静默忽略会产出一个不可迁移却被以为可迁移的 venv——这类错误在使用时才暴露（矩阵第 11 项） |

**必须跑通的既有基准与断言**

| # | 项目 | 判据 |
| --- | --- | --- |
| 18 | `cargo bench -p osdk-core` | `benches/interactive_latency.rs` 的**遍历量**（目录数、stat 次数）不因新增 `pypi` 命名空间而增长超过既有基线。AGENTS.md 明确遍历量是产品指标，且它是确定性的、能在噪声环境暴露退化 |
| 19 | shim 体积编译期断言 | `crates/osdk-shim/src/main.rs:34` 的 `assert!(!osdk_core::INSTALL_PATH_LINKED)` 必须通过。**遇到它失败要改构建命令，不是删断言** |
| 20 | 分两次独立构建量体积 | 用独立 `CARGO_TARGET_DIR`，`cargo build --release -p osdk-cli` 与 `-p osdk-shim` **分开执行**。`--workspace` 单次构建量出的 shim 体积是错的（feature 统一会重开 `install`） |
| 21 | `hashing_crates_are_pinned_to_a_fast_opt_level` | `pipeline/verify.rs:320` 通过。若为 Python 引入新的哈希路径，profile override 的包名匹配不能失效（Cargo 对匹配不到的 override 只 warning） |
| 22 | `ScanOptions::max_depth` 不被收窄 | 保持 8。`pypi:<package>` 展开后的段数需核算，若超出上界应按 backend 裁剪子树而非动深度 |
| 23 | 完整工作区测试 | 在宣布跨提交工作项完成前跑完整验证；测试使用临时 `HOME` / `OSDK_*` / `CARGO_HOME` / 构建目录，不动用户真实 SDK 状态 |
| 24 | TLS 坏证书拒绝 | 若触及 TLS 依赖，实测 badssl.com 的 untrusted-root / expired / wrong-host / self-signed 四种均被拒且原因正确。**测试套件基本离线，证书校验失效也会全绿** |

---

## 8. 分阶段路线图

每阶段独立可交付、可验证。体积与延迟影响按 AGENTS.md 要求逐阶段标注；由于所有新增能力都在安装路径上，**预期对 shim 体积与 hook-env / shim 延迟零影响**，但每阶段都要用 §7.4 的第 18–20 项实测确认而非假定。

### P0：接住 uv 的缓存（最小可交付，纯收益）

**内容**：在 `cache/mod.rs` 的 `cache_env()`（:23）中新增 `set_if_unset("UV_CACHE_DIR", root.join("uv"))`，并在 `describe()`（:86）中体现。

**为什么单独成阶段**：这是**唯一一个不需要任何新后端就能交付的真实收益**。今天已有 `PIP_CACHE_DIR`，加 `UV_CACHE_DIR` 是对称补齐。用户立刻得到「uv 缓存归 osdk 管」，`osdk cache dir` 能看到它。

**第二轮补充定位**：这一阶段不只是「补齐同类产品已有的能力」，而是 **osdk 相对 mise 的差异化**。实测证明 mise 全源码中 `UV_CACHE_DIR` 零命中，其 `MISE_CACHE_DIR` 下的 `cache/uv/` 只放自己的工具元数据（最大 6,466 B），而 uv 的 wheel 缓存仍写在 `%LOCALAPPDATA%\uv\cache`（§4.5.6）。osdk 因为 `cache/mod.rs` 已有 `set_if_unset` 机制，只需一行就能做到 mise 没做的事。矩阵第 38/43 项因此从 P1 前移到本阶段。

**验收**：`osdk cache env` 输出含 `UV_CACHE_DIR`；`variable_is_available()`（:81）语义保持（用户已设的不覆盖）；`cache/mod.rs` 现有 7 个测试全绿。

**体积影响**：新增一个字符串常量与一次 `set_if_unset` 调用。预期 osdk 与 shim 均无可测量变化——但 `cache/mod.rs` 在 shim 的依赖图内（shim 也要 emit cache env），所以**必须实测**而非假定。

**延迟影响**：`cache_env()` 在 hook-env 与 shim 路径上都会执行。新增一个 map 插入与两次 getenv，理论上可忽略，但**必须跑 `cargo bench -p osdk-core`** 确认——AGENTS.md 记录过「不要在装了 osdk 钩子的 shell 里测 osdk」以及「基准必须断言退出码为 0 且输出非空」这两个坑。

---

### P1：uv 作为受管后端 + 索引镜像 + venv 与基础安装

**内容**：

- 新增 uv 后端，照 `native_tool.rs` 的 `NativeToolLifecycle` 模式；uv 版本钉入 `osdk.lock`。
- `config/mod.rs:299 RegistriesConfig` 新增 `python` 字段（对称于既有的 `npm`(:300)），承载索引镜像列表与私有索引。
- PyPI 索引建模为 `Source`，接入 `source/select.rs` 的测速与失效转移；镜像→`--default-index` 的唯一映射（§7.3）。
- `osdk python venv` / `osdk python install`，`--python` 指向 osdk 管理的解释器（形状已由 mise 的 `venv.rs:92` 印证），uv 路径环境中设 `UV_PYTHON_DOWNLOADS=never`。
- **uv 探测用「能否 spawn」判据，不用路径存在**（第二轮新增，§4.5.2）。
- **实现双路径 venv（第三轮反转，§4.5.7）**：uv 可用 → `uv venv` + `uv pip install`；uv 不可用 → `python -m venv` + 该 venv 内的 `python -m pip install`，并在输出中显式说明当前路径、能力差异与 `osdk install uv` 提示。仅 `--require-uv` 时 fail-closed。
- **creator 记录与分派**：osdk 侧记录 venv 的 creator（uv/stdlib）与 seed 内容，后续安装据此选择安装器；osdk 记录缺失时从 `pyvenv.cfg` 的 `uv =` 字段兜底判定。
- **`--seed` 语义归一化**为「pip+setuptools+wheel 齐备」；**`--relocatable` 在 pip 路径下报错**，不静默忽略。
- 门禁：§7.1 全部规则 + **§7.1.1 的 pip 侧等价门禁清单**（含 `PIP_CONFIG_FILE` 隔离、禁传 `--trusted-host`、单一索引不变量）；跨卷预判 `--link-mode=copy`（§6.5）。

**验收**：§7.4 的第 1–9、11、13、15 项；**第二轮的第 25、27 项**；**第三轮改写的第 7、26、27 项与新增的第 28–34 项**；第 18–21 项。

**体积影响**：新增代码全部在 `#[cfg(feature = "install")]` 之后。**双路径不新增任何依赖**——`python -m venv` 与 venv 内的 pip 都是子进程调用，与 uv 路径共用同一套 `CmdLineRunner` 式的进程编排与环境注入逻辑，增量仅是分派判断与参数归一化代码（预计百行量级）。**关键风险**：若在 `Backend` trait 上新增方法，`Registry::new()` 会把它送进所有 13 个 backend 的 vtable 并落到 shim 上（AGENTS.md 记录曾白背 5.15 MB）。因此**仅安装用到的新方法必须加 `#[cfg(feature = "install")]`（trait 定义与每个 impl 都要加）**。新增依赖（如 pyproject 的 TOML 解析）必须 `optional = true` 并经 `install = ["dep:..."]` 引入。预期 shim 零增长，osdk 增长应控制在 10% 内（当前 12.28 MB ⇒ 上限约 13.5 MB）。

**延迟影响**：不触碰 hook-env 与 shim 路径（uv 与 pip 调用都是显式命令）。但新增 `pypi` 命名空间会影响 inventory 遍历——见 P4。

---

### P2：pip 兼容层其余部分 + 缓存生命周期

**内容**：`uv pip compile` / `sync` / `freeze` / `list` / `show` / `tree` / `check` 的映射与透传；`--find-links` 透传；`--compile-bytecode` 透传；`osdk cache clean` 扩展为同时清 uv 缓存；新增 `osdk cache prune`；`Settings::link_mode` → `--link-mode` 映射。

**验收**：§7.4 第 16、17 项；孤立版本化缓存目录的清理走前缀白名单。

**体积影响**：主要是参数映射代码，预期 osdk 增长 < 2%，shim 零增长。仍需实测。

**延迟影响**：无。

---

### P3：项目与依赖解析

**内容**：`uv add` / `remove` / `sync` / `lock` / `run` / `tree` 映射；`pyproject.toml` 只读所需字段；`uv.lock` 按 §4.4 以摘要方式记入 `osdk.lock`；`--frozen` / `--locked` / `--exclude-newer` / `--no-build-isolation` 透传；`uv python upgrade` 语义对齐。

**验收**：§7.4 第 12 项；`osdk.lock` schema 递增并保持向后兼容（`lockfile.rs:307 load()` 已有 schema 范围校验与逐版本拒绝逻辑，如 `reject_native_backends_before_schema_four`）。

**体积影响**：新增 TOML 解析（若 `toml_edit` 已在 CLI 侧存在，则复用；`config_edit.rs:1` 的模块注释表明它已用 `toml_edit`）。预期增长 < 3%。

**延迟影响**：无。

---

### P4：工具安装、ephemeral 运行、workspace、PEP 723

**内容**：`pypi` 动态命名空间（含 `tool.rs:549 DYNAMIC_NAMESPACES` 注册）；`uv tool install` / `uninstall` / `list` / `upgrade` / `dir`；`uvx` 透传；`uv workspace` 三子命令透传；PEP 723 脚本透传；`uv export` 透传；`--keyring-provider subprocess` 透传。

**第二轮补充的两条实现约束**（均来自 §4.5.3）：

- **把 uv 的工具目录钉进 osdk 安装根**：设 `UV_TOOL_DIR` = 该 tool version 的 install path、`UV_TOOL_BIN_DIR` = 其下 `bin`。这正是 mise `pipx.rs:808-809` 的做法【源码】，已被验证可行，无需另行设计。
- **解释器 patch 升级后工具环境失效，用 receipt 解决而非符号链接**。mise 的 `fix_venv_python_symlink()` 在 Windows 上是空实现（`pipx.rs:1133-1136`），即同类产品在此平台无解。osdk 应把 runtime 身份绑进 `NativeToolReceipt`（`native_tool.rs:47`），升级后 receipt 校验失败即要求显式重建——跨平台行为一致。

**验收**：§7.4 第 10 项（**本阶段最关键**）、第 22 项；`cargo bench -p osdk-core` 的遍历量增长必须有解释。

**体积影响**：预期增长 < 3%。

**延迟影响**：**这是唯一有实质延迟风险的阶段。** 新增 `pypi` 命名空间意味着 inventory 扫描多一棵子树。AGENTS.md 记录过两个相关的坑：一是「扫描下探进 conda prefix，一次多走 1,000 个目录」，二是「性能探针必须自证落在被测机制之内」——不能在基准里复刻一份被测判据去数目录，那样把 `inventory.rs` 的判据改成恒真它也输出同一个数字。正确做法是埋 identity 与路径不符的诱饵 manifest（`benches/interactive_latency.rs:267 write_decoy_manifest` 已有此手法），裁剪失效时 fail-closed 扫描会报错。

**若遍历量增长不可接受**：按 backend 裁剪子树，**不要收窄 `ScanOptions::max_depth`**。AGENTS.md 明确记录收窄它在只装了 2 段 id 工具的机器上能带来 8.9x 加速且测试全绿，但会让 `go:github.com/user/cmd/tool`（7 段，manifest 在深度 8）等安装被漏扫——扫不到的动态工具等于不存在。

---

## 9. 遗留待验证项汇总

本文所有无法在本次确证的问题集中列此，各带验证方法：

| # | 待验证 | 验证方法 |
| --- | --- | --- |
| 1 | 含大量 sdist-only 依赖时，镜像缺 PEP 658 是否显著放大下载量 | 构造显式依赖若干仅发布 sdist 的包的 `requirements.in`，对 pypi.org 与 TUNA 分别用冷缓存 `uv pip compile`，比较 `sdists-v9` 字节数 |
| 2 | uv 升级跨缓存版本后旧目录是否被 `uv cache prune` 回收 | 用两个相邻 uv 版本（跨 `simple-v*` 变更）在同一 `UV_CACHE_DIR` 下依次安装，跑 `uv cache prune` 并比对目录列表 |
| 3 | osdk 数据根位于深路径时 uv 是否触发 MAX_PATH 失败 | 把 `UV_CACHE_DIR` 设到约 200 字符路径下，安装深层结构包（如 `jupyterlab`），观察是否报路径错误；同时检查 `LongPathsEnabled` |
| 4 | uv 默认（不传 `--system-certs`）时企业自签 CA 是否生效 | 在配置企业根 CA 的 Windows 机器上，对用该 CA 签发证书的私有索引 `uv pip install`，比较传与不传该参数的结果 |
| 5 | 各镜像的同步延迟上界 | 本次测量时刻八个镜像完全同步（§5.3）。需在新版本发布后的短窗口内重复测量，以观察实际滞后。TUNA 自述 5 分钟间隔【一方】，其余镜像未找到官方 SLA |
| 6 | `pypi:<package>` 展开后的 tool-id 段数是否触及 `MAX_TOOL_ID_SEGMENTS` | 核算 PEP 503 规范化名的最长实际情形（含 `extra` 语法如 `pkg[extra]`），与 `tool.rs` 的段数上界比对 |
| 7 | mise 的 `[deps.uv]` provider 在 Windows 上是否可靠触发 `uv sync` | 本轮只从源码确认其 `install_command` 为 `uv sync`（`deps/providers/uv.rs:49`）与 applicability 条件，未实机跑 `mise deps`。验证方法：在含 `uv.lock` 的项目中启用 `[deps.uv]`，跑 `mise deps` 并观察是否执行 `uv sync`、`.venv` 是否被正确识别为 output |
| 8 | osdk 若采用 `UV_TOOL_DIR` 钉定，Windows 上工具可执行文件的复制语义是否与 osdk shim 生成冲突 | 本轮已确认 uv 在 Windows 上 copy 而非 symlink【官方】、mise 亦设 `UV_TOOL_BIN_DIR`【源码】，但未实测两者叠加后 osdk shim 的行为。验证方法：P4 实现后，在同一工具上比较 `UV_TOOL_BIN_DIR` 下的产物与 osdk `shims()` 目录内容，确认不产生双重启动器 |

---

## 10. 附：本次实测汇总表

便于复核与后续对照。全部为 2026-09-13、Windows x64、北京家用宽带。

| 测量项 | 结果 |
| --- | --- |
| `osdk.exe` release 体积 | 12,876,800 B = 12.28 MB |
| `osdk-shim.exe` release 体积 | 3,703,296 B = 3.53 MB |
| uv 最新版本与发布时间 | 0.12.13，2026-09-10 |
| `uv.exe` 解压体积 | 41,556,784 B = 39.63 MB |
| `uvx.exe` / `uvw.exe` | 各 348,976 B |
| uv Windows zip 压缩体积 | 17,612,025 B |
| uv 顶层子命令数 | 22 |
| PEP 691 支持率（国内镜像） | 4/8（TUNA、USTC、BFSU 支持，api-version 均为 1.1） |
| PEP 658 JSON 内声明（国内镜像） | 0/8 |
| PEP 658 `.metadata` 旁挂实际可用 | 华为云、NJU（200，真实 METADATA）；TUNA/USTC/BFSU/阿里/腾讯 404 |
| 旧版 `/pypi/<name>/json` 支持率 | 5/8 |
| 哈希一致性（numpy + pandas，8 镜像） | 6,625 个文件条目，mismatch 0，缺失 0 |
| 索引延迟中位数（`/simple/numpy/`，5 次采样） | BFSU 82 ms（最快）… 阿里云 2420 ms（最慢），差 29 倍；pypi.org 1774 ms |
| uv 缓存冷装 requests（TUNA） | 1,556 ms；缓存 2,760,289 B |
| uv 缓存暖装（第二个 venv） | 555 ms |
| 离线暖缓存安装 | 成功，解析 3 ms |
| 同卷硬链接 | 4 条路径共享 inode，`LinkType=HardLink` |
| 跨卷硬链接 | 失败（os error 17），自动 fallback to copy 并告警 |
| `--require-hashes` 用上游哈希装镜像制品 | 成功 |
| `--require-hashes` 哈希不符 | fail-closed，报错含期望与实际值 |
| PBS 源可达性 | astral / gh-proxy / github 三源均 200，`SHA256SUMS` 124,665 B |
| PBS 国内镜像 | NJU 与 USTC 的 `github-release` 覆盖 astral-sh，`SHA256SUMS` 与上游逐字节相同，保留历史 tag，不存在的 tag 正确 404 |
| TUNA `github-release` 是否覆盖 astral-sh | **否**（`/github-release/astral-sh/` 返回 404） |

### 第二轮新增实测项（mise，2026-09-13 约 22:40–23:30 CST）

测量环境同上；mise 全程使用隔离的 `MISE_DATA_DIR` / `MISE_CACHE_DIR` / `MISE_CONFIG_DIR` / `MISE_STATE_DIR`，未修改用户真实 mise 状态。

| 测量项 | 结果 |
| --- | --- |
| mise 最新版本与发布时间 | `v2026.9.6`，2026-09-12（考察前一天） |
| 源码固定 commit | tag `v2026.9.6` → annotated tag `6ebe2c70…` → commit `acbbdee0b150f5eeb14eb287198b11625ea35472` |
| `mise-v2026.9.6-windows-x64.exe` 体积 | 103,305,728 B = **98.52 MB** |
| 同版本 zip 压缩体积 | 37,271,912 B |
| Windows arm64 exe 体积 | 90,532,352 B |
| mise 仓库文件总数（该 commit） | 4,084 |
| `mise settings --all` 总项数 | 194 |
| 其中 `python.*` 项数 | **4**（`default_packages_file`、`pyenv_repo`、`uv_venv_auto`、`venv_stdlib`） |
| 其中 `pipx.*` 项数 | **1**（`registry_url`，默认 `https://pypi.org/pypi/{}/json`） |
| mise 是否有 Python 镜像设置 | **无**。检索 `mirror`/`url_rewrite`/`github_url` 只命中 `go.download_mirror`（默认 `https://dl.google.com/go`）与 `zig.use_community_mirrors`（默认 `true`） |
| `python.uv_venv_auto` 默认值 | **`false`**（实测 `mise settings get`） |
| `python.venv_stdlib` 默认值 | `false` |
| `python.pyenv_repo` 默认值 | `https://github.com/pyenv/pyenv.git` |
| venv 创建：无 uv 时 | `INFO creating venv with stdlib` + `DEBUG $ python3 -m venv <path>` |
| venv 创建：有 uv 时 | `INFO creating venv with uv` + `DEBUG $ <uv.exe> venv <path> --seed`；`pyvenv.cfg` 含 `uv = 0.12.13` 与 `seed = true` |
| 经 mise 安装 uv 的耗时 | 223 s（`uv@0.12.13`，`uv-x86_64-pc-windows-msvc.zip`） |
| `uv_venv_auto="source"` + 无 `uv.lock` | no-op，未导出 `VIRTUAL_ENV` |
| `uv_venv_auto="source"` + 有 `uv.lock` 但无 `.venv` | **不创建**，警告 `uv venv not found`，提示跑 `uv sync`/`uv venv` 或启用 `[deps.uv]` |
| 同上，用 uv 自行建好 `.venv` 后 | `VIRTUAL_ENV` 立即被导出 |
| mise 是否纳管 `UV_CACHE_DIR` | **否**，全源码零命中（`UV_PYTHON_INSTALL_DIR` 仅在 `cli/sync/python.rs:91,111` 用于读取） |
| 隔离 `MISE_CACHE_DIR` 下 `cache/uv/` 内容 | 仅 mise 自身元数据：`0.12.13/`、`remote_versions-*.msgpack.z`(6,466 B)、`version_tags_v2-*.msgpack.z`(726 B) |
| 同时 `%LOCALAPPDATA%\uv\cache` 内容 | `archive-v0` / `interpreter-v4` / `sdists-v9` / `simple-v25` / `wheels-v6` 齐备；本轮 venv 创建期间有 1 个 `archive-v0` 子目录被写入 |
| mise `pipx:` 传给 uv 的索引变量 | **`UV_INDEX`**（`pipx.rs:810`），pipx 路径为 `PIP_INDEX_URL`（:837） |
| mise `pipx:` 传给 uv 的工具目录变量 | `UV_TOOL_DIR` + `UV_TOOL_BIN_DIR`（`pipx.rs:808-809`） |
| `mise.lock` 中 Python 相关内容 | 仅 `[[tools.python]]` + `backend = "core:python"` + `version`；检索 `pyproject` / `uv.lock` **零命中** |
| PBS 下载 URL 在 mise 中的形态 | 硬编码常量 `PBS_RELEASE_DOWNLOAD_URL`（`plugins/core/python.rs:36`），另在 :556、:1218 直接拼同一 github.com URL |
| `fix_venv_python_symlink` 平台覆盖 | unix 实现（`pipx.rs:1069`）；**非 unix 为空实现**（:1133-1136） |
| Windows 上 mise 的 Python 编译路径 | 不可用：`plugins/core/python.rs:1076` 的判据为「若在 windows **或** `python_compile != Some(true)`，则走 `install_precompiled`」 |

### 第三轮新增实测项（pip 回退路径，2026-09-14 CST）

环境：Windows x64；全程使用隔离的 `PIP_CACHE_DIR` 与 `PIP_CONFIG_FILE`（后者指向空文件以屏蔽用户全局 pip 配置）+ 临时目录，未触碰用户真实状态；解释器 Python 3.14.7；**前提：本机 `uv` 未安装**（`Get-Command uv` → NOT FOUND），`osdk` 位于 `E:\osdk-bin\osdk.exe`。

| 测量项 | 结果 |
| --- | --- |
| 本机 uv 是否安装 | **否**（`Get-Command uv` → NOT FOUND）——即回退路径是当前默认路径 |
| `python -m venv` 是否自带 pip | **是**，pip 26.2.1 |
| `python -m venv` 是否自带 setuptools | **否**（`find_spec` → False） |
| `python -m venv` 是否自带 wheel | **否**（`find_spec` → False） |
| stdlib `pyvenv.cfg` 字段 | `home` / `include-system-site-packages` / `version` / `executable` / `command` 五项，**无 `uv =`** |
| uv 路径 `pyvenv.cfg` 字段（第二轮对照） | 含 **`uv = 0.12.13`**、`seed = true` —— 可作创建者判定标志 |
| stdlib venv 的 `Scripts/` 内容 | `pip.exe` / `pip3.exe` / `pip3.14.exe` / `python.exe` / `pythonw.exe` + activate 系列 |
| pip `--require-hashes` + 正确上游 sha256（经 TUNA） | 安装成功，**exit=0**（certifi 2026.7.22 = `62f2…3775`） |
| pip `--require-hashes` + 64 个 `0` 的错误哈希 | **exit=1**，`ERROR: THESE PACKAGES DO NOT MATCH THE HASHES FROM THE REQUIREMENTS FILE.`，列出 Expected/Got，无安装 |
| **假阴性教训** | 首次测错误哈希用例时复用了已装过 certifi 的同一 venv，pip 跳过下载因而未校验，得到 **exit=0 的假结果**。必须每用例用全新干净 venv（§7.2.1） |
| `PIP_INDEX_URL` 是否被 pip 识别 | **是**，`pip config debug` 的 `env_var` 段可见；安装日志 `Looking in indexes: https://pypi.tuna.tsinghua.edu.cn/simple` 且 `Downloading` 成功 |
| `PIP_NO_INDEX=1` 离线效果 | `pip install requests` → `ERROR: Could not find a version that satisfies the requirement requests (from versions: none)` |
| pip 可用的相关门禁参数 | `--require-hashes` / `--no-index` / `-i,--index-url` / `--extra-index-url` / `--only-binary` |
| pip 是否有 `--index-strategy` 等价物 | **无**。其 `--extra-index-url` 为候选合并语义，≈ uv 的 `unsafe-best-match`（§7.3 第三轮补充） |
