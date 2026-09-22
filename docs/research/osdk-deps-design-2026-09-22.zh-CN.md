# `osdk deps`：应用依赖清单的实现级设计

日期：2026-09-22

状态：**设计，未改动任何产品代码。** 本文是 `model-management-refactor-2026-09-21.zh-CN.md` §12（P3 调研）的下一层：§12 回答「该不该做」，本文回答「怎么做、分几批做、每批怎么验」。目标是**让人能据以拍板并分批开工**，不是穷尽到可跳过评审。

外部事实基准日：2026-09-22。仓库代码事实均给出文件与行号；外部包管理器行为区分**已查证**（官方文档 URL）与**推导/待核**（§15 诚实清单逐条列出）。凡待核项一律不得在实现时当既定事实使用。

---

## 1. 一页结论

- **新增一个与「装工具」并列的一等概念：应用环境（application environment）。** 单位是「一个项目目录的整份依赖闭包」，装在**项目内**（`node_modules` / `.venv` / `vendor`），真相是**原生 lock**；osdk 的职责是「备好包管理器 → 挑对索引与凭据卫生 → 驱动原生工具 → 回读产物校验 → 把身份记进 osdk.lock」。
- **命令面是 `osdk deps`**，与现有一级动词（`install` / `lock` / `run` / `task` / `model` / `pkg` …，见 `crates/osdk-cli/src/cli.rs` 的 `enum Command`）无冲突。
- **配置面是 `[deps]`**，与 `[tools]` 严格分层：`[tools]` 装**包管理器本身**（node/pnpm/uv/go/cargo…），`[deps.<provider>]` 装**项目自己的包**。这一分层直接对齐 mise（§2.3）。
- **provider 是静态表 + trait**，形状照 `DYNAMIC_NAMESPACES`（`tool.rs:549`）与 `Backend` 注册表（`backend/registry.rs:21-36`）的既有做法：新增生态是加表项，不是加一条 `match` 分支。
- **「没装包管理器就自动装」不新造安装逻辑**，直接复用现有工具安装链：osdk 已内置 node/npm/pnpm/yarn/bun/deno/python/go/rust/java/zig 等 backend（`backend/registry.rs:21-36`），deps 只负责「决定要哪个工具、哪个版本」，装由 `install` 那条路做，落在**隔离的工具 install 目录**，绝不落在项目 venv。
- **trust 按 dbc63f8 修正后的三档落到每个 provider**：默认不要 trust（纯预编译产物 + 官方索引）；自定义 / extra index → `WeakensVerification`；显式放开**从源码构建 / 生命周期脚本** → `ExecutesCode`，默认 Deny、显式开。
- **freshness 借鉴 mise 的哈希模型但补上它承认的短板**：mise 明确「不逐个核验已装包」；osdk 在哈希 freshness 之外提供一层可选的**深度校验**（读 receipt / 比对原生 lock 摘要），并保留 `--force`。
- **不做隐式副作用**：`osdk deps` 默认显式调用；`auto` 前置到 `run`/`exec` 必须由用户显式开启，且有 `--no-deps` 逃生口。这比 mise 的默认更克制，理由与「`osdk install` 刻意不代拉模型」同源。

---

## 2. 定位：deps 在 osdk 里的位置

### 2.1 四类东西，不要混

| | 单位 | 装在哪 | 真相在哪 | 现状 |
| --- | --- | --- | --- | --- |
| **工具（tool）** | 一个可执行工具（node、uv、`pypi:ruff`、`npm:prettier`） | osdk 隔离 install 目录 + shim | osdk.lock `[platforms]` | ✅ 已有 |
| **系统包（syspkg）** | 宿主包管理器的系统包 | 宿主系统 | `osdk.toml [syspkg]` | ✅ 已有 |
| **模型（model）** | 不可变权重快照 + 消费者视图 | `<data>/models`、`<data>/views` | osdk.lock `[models]` | ✅ 已有（本轮重构） |
| **应用环境（app env）** | **一个项目的整份依赖闭包** | **项目内**（`node_modules`/`.venv`/`vendor`） | **原生 lock**（package-lock/uv.lock/go.sum…） | ❌ **缺，本文要补** |

### 2.2 为什么 osdk 今天做不到（§12.1 已落盘的缺口）

- `osdk install`（无 operand）语义是 install from config（`cli.rs` 的 `Command::Install` doc；`commands.rs` 的 `pub async fn install` 里 `let explicit = !tools.is_empty();` 与 `use_lock` 分支），装的是**工具**，不会因为目录里有 `package.json` 就跑一次 `npm install`。
- 真正读 `package.json` 的路径是**带 npm 包 operand** 的项目级编排（`commands.rs` 约 2470–2540），粒度是「逐个依赖加进来并装好」，不是「读整份清单一次兑现」。
- Python 侧连逐依赖的项目环境都没有：`pypi:` backend 是「一 CLI 一独立 venv」；`grep -r "requirements.txt" crates/` **0 命中**。

### 2.3 与 mise `deps` 的对齐与差异（参照，已查证）

来源：<https://mise.jdx.dev/dev-tools/deps.html>（experimental，需 `[settings] experimental = true`）。

| 维度 | mise | osdk 本设计 |
| --- | --- | --- |
| 分层 | 原话：`[tools]` 装包管理器本身，`[deps]` 装项目的包 | **同**（§4） |
| provider 表 | 内置 npm/yarn/pnpm/bun/deno/aube/pip/poetry/uv/go/bundler/composer/dart/flutter/git-submodule，各有默认 sources/outputs/命令 | **同构**，但表项带 trust 档位与「无工具时装什么」（§5） |
| 默认命令可覆盖 | `run = "npm ci"` 覆盖默认 `npm install` | **同**，且**默认就选冻结式命令**（§5.4） |
| freshness | blake3 哈希 sources + 生效命令，存 `$MISE_STATE_DIR/deps/<hash>.toml`（不写项目目录）；**不逐包核验**、不查上游 | 哈希模型**同**（复用 `tasks::freshness` 先例），**另加**可选深度校验补其短板（§8） |
| auto | `auto = true` 默认前置于 `mise run` / `mise x` | **默认关**；开启后仍有 `--no-deps`（§9） |
| pip provider | 官方明确「不创建也不选择 venv」，要另配 virtualenv | **拒绝裸装**：osdk 的 pypi provider 永远在项目 `.venv` 内（§5.5） |
| monorepo | 要显式 `[monorepo].config_roots`，不盲扫子目录 | **同**（§6.3） |

---

## 3. CLI 面（精确签名）

```text
osdk deps [PROVIDER]...                 # 默认动作：探测 + 按需兑现（stale 才动）
  [--list]                              # 只列出探测到的 provider 与 freshness，不执行
  [--dry-run]                           # 报告将执行什么命令、为何 stale，不执行
  [--force]                             # 忽略 freshness，强制重跑
  [--only <PROVIDER>]...                # 只跑这些（可重复）
  [--skip <PROVIDER>]...                # 跳过这些（可重复）
  [--explain]                           # 逐 provider 打印 freshness 判定依据
  [--verify]                            # 深度校验（不止哈希；见 §8.3）
  [--no-install-tools]                  # 缺包管理器时报错而不是自动装（CI 可复现用）
  [--frozen]                            # 要求原生 lock 存在且不被修改，否则失败（CI）
  [--jobs N]                            # 覆盖并发（默认取 settings.jobs）

osdk deps status                        # = --list 的长格式；含工具是否就绪、lock 是否记录
osdk deps doctor                        # 解释性诊断：清单冲突、lock 归属冲突、工具缺失、索引/代理问题
```

设计取舍：

- **不新增 `add`/`remove` 子命令（已定，2026-09-22 用户拍板）**。职责切分固定为：**`osdk deps` = 读整份清单一次兑现**；**`osdk install <npm:pkg>` = 单个依赖的增删**（既有路径，`commands.rs` 约 2494 起）。两者不重叠，也不再规划 `deps add`——mise 的 `deps add npm:react` 这条不吸收，因为 osdk 已有等价入口，并列两套会让「加一个包」出现两种写法。
- `--frozen` 与 `--no-install-tools` 是 CI 组合：**CI 里不希望 osdk 顺手装工具或改 lock**。
- 一级动词选 `deps` 而非 `env`：`model env` 已占用 `env` 语义（provider 环境变量导出），复用会歧义。

---

## 4. `osdk.toml` schema

### 4.1 与 `[tools]` 的关系（可直接抄的示例）

```toml
# 工具本身：谁来装包 —— 走既有 install 链，落隔离 install 目录 + shim
[tools]
node = "24"
pnpm = "10"
python = "3.12"
uv = "latest"

# 项目自己的包：装进项目内
[deps.pnpm]                 # 空表 = 启用内置 provider，但不自动触发
[deps.uv]
auto = true                 # 允许在 osdk run / exec 前自动兑现（默认 false）

# 自定义 provider：任意构建步骤，形状与 [tasks] 的 sources/outputs 同构
[deps.codegen]
description = "Generate API types"
sources = ["schema/*.graphql", "codegen.yml"]
outputs = ["src/generated/"]
run = "pnpm run codegen"
depends = ["pnpm"]          # 等 pnpm provider 先完成
dir = "."                   # 相对 config root
env = { NODE_ENV = "development" }
timeout = "5m"

# 关掉从更高层继承来的 provider
[deps]
disable = ["npm"]
```

### 4.2 provider 子表字段全集

| 字段 | 类型 | 默认 | 说明 |
| --- | --- | --- | --- |
| `auto` | bool | `false` | 是否允许前置于 `osdk run` / `osdk exec`。**默认 false 是与 mise 的关键差异** |
| `sources` | string[] | provider 内置 | 参与 freshness 的清单/输入 glob。设置即**替换**内置默认（不是追加） |
| `outputs` | string[] | provider 内置 | 必须存在才算 fresh 的产物；`[]` 显式关闭产物跟踪 |
| `run` | string | provider 内置（冻结式） | 覆盖默认命令 |
| `env` | table | 空 | 附加环境变量；与 osdk 注入的索引/凭据变量合并，**osdk 的安全变量不可被覆盖**（§11.4） |
| `dir` | string | config root | 该 provider 的工作目录（子项目） |
| `depends` | string[] | 空 | 顺序依赖，仅排序已配置的 provider，不会「声明出」一个不存在的 provider |
| `timeout` | string | 无 | 单个 provider 的执行上限 |
| `allow_build_from_source` | bool / string | `false` | **trust 第三档开关**（§11.3）。默认拒绝源码构建 / 生命周期脚本 |
| `index` / `extra_index` | string | 无 | 覆盖索引；一旦出现即触发 `WeakensVerification`（§11.2） |
| `installer` | string | auto | 显式钉 installer（如 `pnpm`/`npm`、`uv`/`pip`），覆盖自动优先级（§7） |

顶层 `[deps]` 另有：`disable = [...]`（禁用继承来的 provider）。

**为什么字段形状刻意抄 `[tasks]`**：仓库已有一套 `sources`/`outputs`/`depends`/`dir`/`env`/`freshness` 的语义与实现（`tasks/mod.rs:363-430`、`tasks/freshness.rs`），包括「`sources` 匹配到 0 个文件会让 freshness 空洞成立」这类已踩过的坑注释。deps 复用同一套心智模型与代码，比新造一套便宜且不会产生两种 glob 语义。

---

## 5. Provider 抽象

### 5.1 静态表 + trait（形状）

```rust
// crates/osdk-core/src/deps/mod.rs（提议）
pub struct DepsProviderSchema {
    /// 稳定 id，也是 `[deps.<id>]` 的键与 CLI 里的名字。
    pub id: &'static str,
    /// 生态分组，用于「同生态内多 installer 互斥」的判定（见 §7）。
    pub ecosystem: Ecosystem,          // Node | Python | Go | Rust | Ruby | Php | Deno | Custom
    /// 探测用的清单文件；第一个是"主清单"（决定项目根）。
    pub manifests: &'static [&'static str],
    /// 该 provider 认领的原生 lock（可为空，如 go/pip）。
    pub native_lock: Option<&'static str>,
    /// freshness 的默认 sources / outputs。
    pub default_sources: &'static [&'static str],
    pub default_outputs: &'static [OutputSpec],   // Required | OptionalOnceSeen
    /// 默认命令：**优先选冻结式**（见 §5.4）。
    pub default_run: &'static str,
    /// 要跑这条命令，需要哪些 osdk 工具（id + 版本来源）。
    pub required_tools: &'static [RequiredTool],
    /// trust 档位映射（哪些 key 触发哪一档）。
    pub trust: TrustProfile,
}

pub trait DepsProvider: Send + Sync {
    fn schema(&self) -> &'static DepsProviderSchema;
    /// 从清单/lock 现状选 installer（同生态多候选时）。
    fn select_installer(&self, project: &DetectedProject, settings: &DepsSettings)
        -> Result<InstallerChoice>;
    /// 生成要执行的命令与环境（含 osdk 注入的索引/凭据卫生变量）。
    fn plan(&self, project: &DetectedProject, choice: &InstallerChoice, cfg: &ProviderConfig)
        -> Result<RunPlan>;
    /// 执行后回读产物，产出可入 lock 的身份记录 + 可选深度校验材料。
    fn reap(&self, project: &DetectedProject, plan: &RunPlan) -> Result<DepsReceipt>;
}
```

`#[cfg(feature = "install")]` 门控：**整个 `deps` 模块都在 install feature 之后**，与 `tasks`、`ModelDeclaration` 同理——shim 从不兑现应用依赖，它的依赖图不该背这些解析（AGENTS.md 的体积纪律；`config/mod.rs` 里 `tasks`/`models` 已是此先例）。

### 5.2 公共层 vs per-provider 的切线

**抽进公共层（一次写，所有生态受益）**：

| 公共能力 | 来源/先例 |
| --- | --- |
| 向上逐层发现清单 + 「坏清单 fail-closed 不跳过」 | 照 `npm_tools.rs:271` `find_nearest_package_json`（遇到存在但非常规文件即报错） |
| 「安装前重新 inspect」防并发漂移 | 照 `commands.rs:2494` 的重新 inspect |
| installer 选择的**优先级骨架**（清单声明 → 现存 lock → settings 兜底）与冲突报错 | 照 `npm_tools.rs:357` `select_automatic_installer` + `err.npm_manager_lock_owner_conflict` |
| freshness 计算与状态存储 | 复用 `tasks::freshness`（`input_hash` / `state_path` / `FreshnessState`） |
| 工具就绪与自动安装编排 | 复用现有 install 链（§7） |
| 索引/镜像映射与「危险参数拒绝」 | 复用 `pypi.rs` 的 `installer_env` / `reject_unsafe_installer_args` 思路，抽成跨生态的 `IndexPolicy` |
| trust 逐 key 分类 | 复用 `trust.rs` 的 `INSPECTED_TABLES`（`trust.rs:202`）+ `collect_*_requirements` 模式 |
| lock 写入与回读（读写对称） | 照 `lockfile.rs` 的 `set_model_views` / `locked_views_from_declaration` 先例 |
| 并发与 `depends` 拓扑排序 | 复用 `tasks::runner` 的依赖排序思路 |

**每个 provider 自己实现（无法共用）**：清单解析（package.json 的 `packageManager` vs pyproject 的 `[tool.uv]` vs go.mod）、installer 候选集与互斥规则、原生 lock 的 kind/格式识别与摘要、冻结式命令的具体拼法、产物回读的判据（`node_modules/.bin` vs `pyvenv.cfg` vs `vendor/modules.txt`）。

### 5.3 内置 provider 表（首版目标与占位）

| id | ecosystem | 主清单 | 原生 lock | 默认命令（冻结优先） | outputs | 需要的 osdk 工具 | 批次 |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `npm` | Node | `package.json` | `package-lock.json` | `npm ci --ignore-scripts`（无 lock 时降级 `npm install --ignore-scripts` 并**报告降级**） | `node_modules/`（Required） | `node`（npm 随 node） | D1 |
| `pnpm` | Node | `package.json` | `pnpm-lock.yaml` | `pnpm install --frozen-lockfile --ignore-scripts` | `node_modules/` | `node`, `pnpm` | D1 |
| `yarn` | Node | `package.json` | `yarn.lock` | **按 major 分派**：berry `yarn install --immutable`（禁脚本用 `YARN_ENABLE_SCRIPTS=false`，**没有** `--ignore-scripts`）；classic `yarn install --frozen-lockfile --ignore-scripts` + **osdk 自己预检 lock 存在**（classic 缺 lock 时不报错） | `node_modules/` | `node`, `yarn` | D1 |
| `bun` | Node | `package.json` | `bun.lock`（新格式；`bun.lockb` 为旧二进制格式） | `bun install --frozen-lockfile --ignore-scripts` + **osdk 自己预检 lock 存在**（bun 缺 lock 时不报错） | `node_modules/` | `bun` | D1 |
| `uv` | Python | `pyproject.toml` | `uv.lock` | `uv sync --frozen` | `.venv/`（OptionalOnceSeen） | `python`, `uv` | P2 |
| `pip-requirements` | Python | `requirements.txt` | 无（可选 `requirements.lock`） | `uv pip sync requirements.txt`（回退 `uv pip install -r`，降级要报告） | `.venv/` | `python`, `uv` | P2 |
| `poetry` | Python | `pyproject.toml` | `poetry.lock` | `poetry install --sync` | `.venv/` | `python`, `poetry`(pypi:) | P3 |
| `go` | Go | `go.mod` | `go.sum` | `go mod download`（有 `vendor/` 时 `go mod vendor`） | `vendor/`（OptionalOnceSeen） | `go` | P3 |
| `cargo` | Rust | `Cargo.toml` | `Cargo.lock` | `cargo fetch --locked` | 无（cargo 装进 `CARGO_HOME`，**非项目内**，见下） | `rust` | P3 |
| `bundler` | Ruby | `Gemfile` | `Gemfile.lock` | `bundle install --deployment`（**待核**） | `vendor/bundle/` | ruby（**osdk 目前无 ruby backend**，见 §15） | P4 |
| `composer` | Php | `composer.json` | `composer.lock` | `composer install` | `vendor/` | php（**无 backend**） | P4 |
| `deno` | Deno | `deno.json(c)` | `deno.lock` | `deno install --frozen`（**待核**） | 视配置 | `deno` | P3 |
| `<自定义>` | Custom | 由 `sources` 决定 | 无 | 用户 `run` | 用户 `outputs` | 无（依赖 `depends`） | P4 |

**`cargo` 是一个刻意的例外提示**：它不把依赖装进项目目录，而是 `CARGO_HOME` 全局缓存。所以「outputs 必须在项目内」这条假设不能写进公共层——公共层只能要求「provider 自己说明产物在哪、以及如何判定存在」。这正是 mise 表里把 `go`/`pip` 的 outputs 标为 optional 的同一类问题。

### 5.4 Node 系参数核准结果（2026-09-22 实测，D1 的直接依据）

写 provider 表之前先把「冻结」与「禁脚本」两组开关逐个实测，**不把待核当既定事实**。探针在临时目录 + 隔离 HOME/各包管理器 cache 下运行，用一个 `file:` 本地依赖，其 `preinstall`/`install`/`postinstall` 各往标记文件追加一行——「脚本有没有跑」由标记文件内容判定，而不是看命令有没有报错。

版本：node v22.23.2 / npm 10.9.8 / pnpm 9.15.1 与 **12.5.1**（后者是 osdk 自己装出来的版本）/ yarn 1.22.19（classic）与 **4.6.0**（berry，经 corepack）/ bun 1.4.2。

| 断言 | npm 10.9.8 | pnpm 9.15.1 / 12.5.1 | yarn classic 1.22.19 | yarn berry 4.6.0 | bun 1.4.2 |
| --- | --- | --- | --- | --- | --- |
| 缺 lock 时"冻结"命令**是否失败** | ✅ `npm ci` exit=1（`EUSAGE`，明确要求先 `npm install`） | ✅ exit=1（`ERR_PNPM_NO_LOCKFILE`） | ❌ **exit=0**，`--frozen-lockfile` 被接受但只打印 `info No lockfile found.`，照常安装且**不写 lock** | ✅ exit=1（`YN0028: The lockfile would have been created by this install, which is explicitly forbidden`） | ❌ **exit=0**，照常安装且**不写 lock** |
| lock 与清单不一致时是否失败 | ✅ exit=1（`can only install packages when your package.json and package-lock.json ... are in sync`，并列出 `Missing: left-pad@1.3.0 from lock file`） | ✅ exit=1（`ERR_PNPM_OUTDATED_LOCKFILE`，并打印 specifiers 差异） | 未单独测（见诚实清单） | 未单独测（见诚实清单） | ✅ exit=1（`error: lockfile had changes, but lockfile is frozen`） |
| `--ignore-scripts` 是否存在且真的禁掉脚本 | ✅ 标记文件 `<none>`（`npm install` 与 `npm ci` 两条都验过） | ✅ 标记文件 `<none>` | ✅ 标记文件 `<none>`，并打印 `warning Ignored scripts due to flag.` | ❌ **不存在该选项**：`Unknown Syntax Error: Unsupported option name ("--ignore-scripts")`，`yarn install` 只接受 `--immutable/--immutable-cache/--refresh-lockfile/--check-cache/--check-resolutions/--inline-builds/--mode` | ✅ 标记文件 `<none>` |
| 禁脚本的替代途径 | — | — | — | ✅ `YARN_ENABLE_SCRIPTS=false`：标记 `<none>`，并打印 `YN0004: ... lists build scripts, but all build scripts have been disabled` | — |
| 不加禁脚本开关时脚本确实会跑（反向控制） | ✅ 标记含 `dep-preinstall,dep-install,dep-postinstall,root-preinstall,root-postinstall` | ✅ 9.15.1 同上；**12.5.1 只跑 root 的 `root-postinstall`，依赖的脚本默认被拦**并以 `ERR_PNPM_IGNORED_BUILDS` exit=1（提示 `pnpm approve-builds`） | ✅ 标记含全部五条 | ✅ 标记含 `dep-preinstall,dep-postinstall,root-postinstall` | ✅ 只跑 root 的两条，依赖的 postinstall 被默认拦下（`Blocked 3 postinstalls. Run 'bun pm untrusted' for details.`） |
| classic 是否接受 berry 的 `--immutable` | ❌（无关） | — | ⚠️ **exit=0 静默接受**，但不起冻结作用（脚本照跑、无 lock 也不报错） | — | — |

**对 provider 表的四条直接后果**（已写进 §5.3）：

1. **`--frozen-lockfile` 不能被当作「保证冻结」的统一手段。** yarn classic 与 bun 在**缺 lock** 时接受该标志却照常安装，属于「看起来成功其实没冻结」——正是 AGENTS.md 点名的静默降级形状。所以 **osdk 必须在调用前自己预检 `native_lock` 是否存在**，缺失时按 §5.5 的降级路径显式报告，而不是把冻结责任推给包管理器。
2. **yarn 必须按 major 分派命令**：berry 用 `--immutable` + `YARN_ENABLE_SCRIPTS=false`；classic 用 `--frozen-lockfile --ignore-scripts`。把 berry 的写法套到 classic 会静默不生效（上表最后一行）。major 从 `packageManager` 字段或 `yarn --version` 判定。
3. **「禁脚本」在 yarn berry 上是环境变量而非 CLI 参数**，所以公共层的 `RunPlan` 必须同时承载 args 与 env 两种表达，不能只拼命令行。
4. **pnpm 12 与 bun 默认已拦依赖构建脚本**（pnpm 12 甚至因此 exit=1）。这对 osdk 的 trust 第一档是**加强**而非削弱：即便用户没显式禁脚本，这两个较新的包管理器自己也默认不跑依赖脚本。但 osdk 仍显式传 `--ignore-scripts`，理由是不依赖某个版本的默认值（pnpm 9 就会跑）。

### 5.5 缺 lock 时的降级路径（因上表第 1 条而必须显式化）

```text
provider.native_lock 存在？
├─ 是 → 用冻结命令；失败即失败（lock 过期/不一致是真问题，不降级）
└─ 否 → ① 默认：降级为非冻结命令，并在输出里**明确报告**"本次为非冻结安装，将生成 <lock>"
        ② --frozen：直接失败（CI 语义：缺 lock 就是错误）
```

这条对 npm/pnpm/berry 是"顺带正确"（它们自己就会失败），对 classic/bun 是**唯一**能让语义一致的办法。

### 5.6 默认选冻结式命令（与 mise 的一处差异）

mise 的内置默认是**普通安装命令**（`npm install`、`pip install -r`），官方明确「不一定是 frozen-lockfile 安装」，要冻结得自己覆盖 `run`。osdk 反过来：**默认冻结**，因为 osdk 的整体承诺是可复现（lock 是它的立身之本）。代价是「没有原生 lock 的新项目会失败」——处理办法是：无 lock 时自动降级为非冻结命令，**但在输出里明确报告这次是非冻结安装**，而不是静默换命令（「静默降级」正是 AGENTS.md 反复点名的失效模式）。

---

## 6. 自动探测：边界与优先级

### 6.1 触发前提（不盲扫）

探测**只在以下条件下发生**，其余情况一律不做：

1. 项目配置里存在 `[deps]` 段（哪怕是空表 `[deps.pnpm]`）——即「有相应设置」；**或**
2. 用户显式 `osdk deps`（此时即使没有 `[deps]` 段也探测，但只**报告**发现了什么、并提示如何声明；不自动执行）。

这条边界是刻意的：无声明就自动跑 `npm ci` 属于「隐式大副作用」，与「`osdk install` 刻意不代拉模型」同一条克制原则。

### 6.2 发现算法

```text
从 cwd 起，向上逐层到 config root（含）：
  对每个已启用的 provider：
    若该层存在其主清单：
      → 命中，记录 (provider, project_root=该层, manifest 路径)
      → 该 provider 停止向上（最近者胜）
边界：
  - 清单文件存在但无法解析 / 不是常规文件 ⇒ 立即报错，不跳过继续向上
    （照 npm_tools.rs:271 的 fail-closed；「坏的上层清单会挡住下层」是已知且刻意的语义）
  - 同一层同生态命中多个 provider（如 package.json + 两种 lock）⇒ 走 §7 的互斥判定
  - 不向下递归。子项目要么用 `dir`，要么用 monorepo 显式 roots（§6.3）
```

### 6.3 monorepo

与 mise 一致：**不扫任意子目录**。两条途径——单个子项目用 `[deps.<p>] dir = "apps/api"`；多子项目用显式 roots 列表（形如 `[deps] roots = ["apps/*", "packages/*"]`），provider id 带 root 限定（`//apps/api:uv`）。理由与模型扫描「深度上限不能乱收窄、动态目录要显式登记」（AGENTS.md）同源：能被自动发现的集合必须是显式声明的。

---

## 7. installer 选择与「无工具自动安装」

### 7.1 同生态内的 installer 优先级（照 npm 已验证的顺序）

```text
① 项目清单自己的声明        （package.json 的 packageManager；pyproject 的工具段）
② 现存原生 lock 的归属者     （谁写的 lock 谁继续管）
③ [deps.<p>].installer      （项目显式钉）
④ settings.<eco>.default_installer（仅兜底）
冲突即报错，绝不挑一个猜着跑：
  - 清单声明 pnpm 但目录里是 package-lock.json ⇒ 报错（照 err.npm_manager_lock_owner_conflict）
  - 同层出现两种 lock（pnpm-lock.yaml + package-lock.json）⇒ 报错，要求显式 installer
```

注意 ③ 排在 ② 之后：项目**清单**的声明与**现有 lock** 都比 osdk 侧配置更权威，这与 `npm_tools.rs:313-316` 的注释（改 default 绝不会把已声明/已有 lock 的项目抢走）一致。

### 7.2 工具缺失时怎么装（复用现有链，不重造）

**先给核准结论（2026-09-22 实测，D2 的地基）**：在一个空的临时 `OSDK_*` 根里执行 `osdk install node@22 pnpm --yes` → exit=0，`osdk list` 报 `node: 22.23.2` 与 `pnpm: 12.5.1`；产物落在 `<install>/node/22.23.2` 与 `<install>/pnpm/12.5.1`（各带 `.locks`），shim 目录生成了 `node/npm/npx/corepack/pnpm/pnpx`（`.cmd` 成对）；**项目目录没有出现 `node_modules`**——即既有安装链确实把包管理器装进**隔离 install 目录**，不碰项目。所以 D2 只需「决定要哪个工具与版本」并调用这条链，不新造安装逻辑。

**⚠️ 一条改变实现方式的发现：deps 不能通过 shim 调用包管理器。** 用 `<data>/shims/pnpm.cmd` 调用时得到 `osdk-shim: no version of 'pnpm' selected (set one with 'osdk use pnpm@<version>')` 且 exit=1——shim 需要「当前目录已选定版本」，而 deps 的场景恰恰是「osdk 刚把工具装好、项目未必 `use` 过」。因此 **deps 必须用解析出的真实 bin 路径执行**：`Backend::bin_paths(&ctx, &tv)`（`backend/mod.rs:185`，已存在，**不需要给 trait 加方法**）取到 `<install>/pnpm/12.5.1/bin`，从中拿 `pnpm.mjs`/`pnpm.cmd`；同时把 node 的 bin 目录**前置**进子进程 PATH（实测必需：依赖的生命周期脚本里写 `node -e ...`，若 PATH 没有 node 就会失败）。这与 `npm_package.rs` 项目级编排已有的 `node_bin_dir` 做法一致。



```text
provider.required_tools → 形如 [{ id: "pnpm", version_from: ToolsSection|Latest }]
                        ↓
① 查 inventory：该 tool 是否已有满足版本的 install
② 缺 → 走既有安装路径（install_requests 那条链，backend/registry.rs 已内置
        node/npm/pnpm/yarn/bun/deno/python/go/rust/java/zig）
③ 装到隔离 install 目录（InstallScope，不是项目 venv）；shim 照常生成
④ 把它作为 InstallDependencyKind::Installer 记进这次应用环境的身份
        （tool.rs:102-117 已有 Runtime | Installer | Tool 三种 kind，正好对上）
```

**版本从哪来（优先级）**：`[tools]` 里已声明的 → 项目清单里声明的（`packageManager: pnpm@10.4.1`、`.python-version`）→ provider 默认（latest 稳定）。前两者冲突时报错而不是二选一。

**`--no-install-tools`** 让第 ② 步变成报错，给 CI 用：CI 通常希望工具由显式的 `osdk install` 装好，`deps` 只兑现依赖。

---

## 8. freshness 与深度校验

### 8.1 哈希 freshness（复用既有实现）

判 stale 的输入：`sources` 匹配到的文件内容 + **生效命令**（run 字符串 + installer + 工具版本 + 索引 URL + env 表）。这和 `tasks::freshness::input_hash`（`freshness.rs:285-297`，定义本身参与哈希）是同一模型，直接复用而不是新写。

- 必需 outputs 不存在 ⇒ stale。
- 可选 outputs（`OptionalOnceSeen`）：**观察到过一次之后**才检查其消失（照 mise 的语义，处理「装到项目外」的 provider）。
- `sources` 匹配到 0 个文件 ⇒ 这是个陷阱：`tasks/freshness.rs:15` 的注释已记过「空 sources 让 freshness 空洞成立」。deps 侧必须沿用同样的显式处理（报错或明确视为永远 stale），不能静默为 fresh。

### 8.2 状态存哪

`<cache>/deps/<project-key>.toml`，`project-key` 是 config 路径的 blake3 前缀——**完全照 `freshness::state_path`（`freshness.rs:300-316`）的既有做法**：派生数据放受管 cache，不写进项目目录（否则每个用户都得加 `.gitignore`）。符合用户偏好「不手写 osdk 管控目录下的文件」：这文件只由 osdk 命令读写，用户从不编辑。

### 8.3 深度校验（osdk 相对 mise 多出的一层）

mise 官方明确：freshness 检查**不逐个核验已装包**、不查上游、也发现不了「外部工具动过 site-packages」。这正是 AGENTS.md「命令成功 + 哈希没变 ≠ 环境真的对」警告的形状。osdk 因此提供 `--verify`（也可作为 `osdk deps doctor` 的一部分）：

| 层 | 检查 | 成本 |
| --- | --- | --- |
| L0 哈希（默认） | sources + 生效命令哈希、outputs 存在性 | 毫秒级 |
| L1 身份（默认，便宜） | 原生 lock 的 sha256 是否仍等于 osdk.lock 记录值；installer/工具版本是否仍匹配 | 一次文件读 |
| L2 产物（`--verify`） | provider 各自的 receipt 判据：`node_modules/.bin` 齐全（照 `npm_package.rs` 的 `validate_project_package_bins`）、`pyvenv.cfg` 的 creator/解释器仍是记录的那个（照 `pypi.rs` 的 `creator_from_pyvenv_cfg`）、关键包元数据存在 | 秒级 |

L1 是关键补强：它能抓到「原生 lock 被人改过但 sources 哈希也跟着变了所以看着一致」之外的另一种漂移——**lock 没变但环境被别的工具替换过**。

---

## 9. 与 `install` / `run` / `activate` 的关系

| 场景 | 行为 | 开关 |
| --- | --- | --- |
| `osdk install`（无 operand） | **默认不兑现应用依赖**，但**报告**「检测到 N 个 provider stale，运行 `osdk deps`」 | `settings.deps.on_install = "report" \| "apply" \| "off"`（默认 `report`） |
| `osdk install <tool>`（显式 operand） | **一律不碰 deps**，与既有「显式 operand 跳过 lock replay」语义一致 | — |
| `osdk run <task>` / `osdk exec` | 仅当该 provider `auto = true` 时前置兑现 | `--no-deps` 单次跳过 |
| `osdk activate` / `hook-env` | **绝不在此兑现依赖**；最多在 `status` 里提示 stale | `settings.deps.status_stale = false` 关提示 |
| shim 调用 | **完全不参与**（shim 每次命令都跑，任何安装动作都不可接受） | — |

`hook-env` 那条是硬红线：它在**每个提示符**执行（AGENTS.md「交互延迟」），deps 的探测涉及文件系统 walk 与哈希，绝不能进这条热路径。提示 stale 只能读已有 state 文件的一个布尔，且要进 bench 对照。

---

## 10. lock 影响

### 10.1 新增段

```toml
# osdk.lock（schema 仍为 4，理由见 10.3）
[deps.pnpm]
provider = "pnpm"
installer = "pnpm"
installer_version = "10.4.1"
runtime = "node@24.3.0"                 # 生效的运行时工具及其精确版本
manifest = "package.json"               # 相对 config root，归一为 `/`
manifest_sha256 = "…"
native_lock = { kind = "pnpm-lock", path = "pnpm-lock.yaml", sha256 = "…" }
index = "https://registry.npmjs.org/"   # 折叠为官方端点（照 model endpoint 折叠先例）
run = "pnpm install --frozen-lockfile"  # 生效命令（进 freshness 哈希，也便于审计）
allow_build_from_source = false

[deps.uv]
provider = "uv"
installer = "uv"
installer_version = "0.12.9"
runtime = "python@3.12.8"
manifest = "pyproject.toml"
manifest_sha256 = "…"
native_lock = { kind = "uv-lock", path = "uv.lock", sha256 = "…" }
index = "https://pypi.org/simple/"
run = "uv sync --frozen"
allow_build_from_source = false
```

**不入 lock 的东西**：绝对路径（本机位置）、实际使用的镜像域名（折叠为官方，照 `merge_model` 的 `canonical_provider_endpoint` 先例与 §6.3 的理由）、凭据、`.venv`/`node_modules` 的内容清单（那是原生 lock 的职责，osdk 不复制一份依赖图）。

### 10.2 读写对称（硬要求）

AGENTS.md 记过 pypi「只写 installer 不回读、重锁被本机环境静默覆盖」的坑，P1-4 也为此给 `LockedModel.views` 补了 `set_model_views` 读路径。deps 段从第一版起就必须：写入（安装成功后）+ **回读**（`deps --list`/L1 校验/跨机复现都读它），并有一条测试专门钉住「回读路径存在」——注入「只写不读」必须变红。

### 10.3 兼容与 schema 版本

`Lockfile` 与各 `Locked*` 结构**没有** `deny_unknown_fields`（`lockfile.rs:46/326`），P1-4 已用真实 v0.0.2 旧二进制实测过「旧版读带 `views` 的新 lock 会忽略未知字段、exit=0」。`[deps]` 是同类的可选新段，因此**同样用 `skip_serializing_if` 保持旧 lock 字节不变、schema 保持 4**。**实现阶段必须复跑同一实测**（旧二进制读带 `[deps]` 的 lock），不得因为 views 那次通过就假定这次也通过。

---

## 11. trust：三档落到每个 provider

依据已修正的 §12.3（提交 dbc63f8）与 `trust.rs:79-84` 的既有哲学（声明装哪个包故意不进门禁）、`npm_package.rs:786-788` 的 `BuildPolicy::Deny` 默认。

### 11.1 第一档：默认**不要** trust

**纯预编译产物 + 官方默认索引**的应用依赖声明不要求信任。逐 provider：

| provider | 为什么默认安全 |
| --- | --- |
| npm/pnpm/yarn/bun | osdk 默认传 `--ignore-scripts` 一类开关（npm 侧已是 `BuildPolicy::Deny` 默认），不跑生命周期脚本 |
| uv / pip-requirements | 默认只装 wheel（`--only-binary` 一类约束，**确切开关待核**，§15） |
| go | `go mod download` 只取模块源码到缓存，不执行构建（`go build` 才编译；**deps 不负责 build**） |
| cargo | `cargo fetch` 只下载，不编译（build script 在 `cargo build` 时才跑） |
| composer / bundler | **待核**（各自有 script/extension 构建机制） |
| 自定义 provider | **不属于这一档**，见 11.3 |

### 11.2 第二档：`WeakensVerification`

出现以下任一 key，按 `WeakensVerification` 报告（与 `sources`/`registries` 同级）：`index`、`extra_index`、任何自定义 URL、`insecure` 类开关。理由是**改了字节来源**（依赖混淆 / 被劫持源），**不是**因为执行代码。防线沿用 `pypi.rs` 的既有做法：镜像只映**默认**索引，绝不写 `--extra-index-url` / `UV_INDEX`（`installer_env`），并对用户传入的危险参数拒绝（`reject_unsafe_installer_args`）——这两个要抽成跨生态的 `IndexPolicy`，npm 侧对应 `--registry` 与 scope registry 的同类判定。

⚠️ 这里有一个 AGENTS.md 明确记过的坑：**前缀匹配过宽与写窄同样危险**（`UV_INDEX_INTERNAL_USERNAME` 是凭据、`UV_INDEX_URL` 只是地址，共享 `UV_INDEX_` 前缀，只看前缀会把每个镜像配置误判为"已配置凭据"并静默关闭镜像选择）。deps 的索引/凭据判定必须**两个方向都有测试**：该命中的命中、不该命中的不命中。

### 11.3 第三档：`ExecutesCode`，默认 Deny、显式开

| 触发 | 语义 |
| --- | --- |
| `allow_build_from_source = true`（Python：允许 sdist 构建；Node：允许生命周期/构建脚本；任意 provider：from-source） | 在本机执行任意代码，等价 npm 的 `allow_builds`，按 `ExecutesCode` 报告 |
| **自定义 provider 的 `run`** | 它本身就是一条任意命令。**自定义 provider 一律要求 trust**（`ExecutesCode`），无例外——这与 `[tasks]` 不同（task 是用户显式 `osdk run <name>` 触发的，自己就是授权），而 deps 的自定义 provider 可能被 `auto` 前置执行，用户没有逐次点头 |

### 11.4 实现落点

`trust.rs` 的 `INSPECTED_TABLES`（`trust.rs:202`）加 `"deps"`，新增 `collect_deps_requirements` 逐 key 分类；`affects_tool_dispatch` 对 `deps..` **返回 false**（shim 从不兑现应用依赖，让 `[deps]` 阻断 `cargo --version` 会精确重演 `[syspkg]` 事故）——并配一条「移除该分支即变红」的测试，照 P1-4 `models` 那两条的做法。

---

## 12. 跨平台与体积纪律

- **路径**：写进 lock/receipt 的**相对**路径归一成 `/`；表示本机位置的**绝对**路径保留原生分隔符，读取侧用 `s.split(['/', '\\'])` 而非 `Path::components()`（AGENTS.md「跨平台路径」）。这类函数的测试两种分隔符用例都要在**所有平台**跑，不加 `#[cfg(windows)]`。
- **体积**：`deps` 全模块在 `install` feature 之后；**不给 `Backend` trait 加方法**（deps 不是 backend，它调用既有安装链）。若必须加，按 AGENTS.md 判断属安装侧还是 shim 侧。每阶段按 `[tasks].size` 分两次 `cargo build --release -p`（不能用 `--workspace`）实测对照。
- **延迟**：`hook-env` 不做探测（§9）；若最终在 `status` 加 stale 提示，须跑 `osdk run bench` 对照遍历量。

---

## 13. 分批实现路线

每批可独立提交、独立验证；验收一律「先注入对应缺陷确认变红」。

| 批次 | 内容 | 验收（含注入） |
| --- | --- | --- |
| **D1 公共层 + Node 系** | `deps` 模块骨架、静态表、探测（含坏清单 fail-closed）、installer 优先级与冲突报错、freshness 复用、状态文件、`osdk deps [--list/--dry-run/--force/--explain]`、npm/pnpm/yarn/bun 四个 provider、lock `[deps]` 段写+读、trust 分类 | 真实 pnpm/npm 项目端到端（fixture 离线 registry）；注入：把 fail-closed 改成跳过坏清单 → 测试红；移除 lock 回读 → 「重锁保持 installer」测试红；`affects_tool_dispatch` 去掉 `deps` 分支 → 「带 `[deps]` 项目里 `cargo --version` 仍可用」红 |
| **D2 工具自动安装衔接** | `required_tools` → inventory 查 → 复用 install 链 → 记 `InstallDependencyKind::Installer`；`--no-install-tools` | 空 SDK 根下只声明 `[deps.pnpm]`，一条 `osdk deps` 装好 node+pnpm 再装依赖；注入：让自动安装静默跳过 → 「缺工具必须报错或装上」红 |
| **D3 Python 系** | `uv`（pyproject+uv.lock）、`pip-requirements`；强制项目 `.venv`；复用 `pypi.rs` 的解释器钉版与索引策略；L2 深度校验的 `pyvenv.cfg` 判据 | 真实 uv 项目；注入：允许 uv 自行下解释器（去掉 `UV_PYTHON_DOWNLOADS=never`）→ 「解释器必须是 osdk 解析的那个」红 |
| **D4 深度校验 + CI 模式** | `--verify`（L2）、`--frozen`、`deps doctor` | 手动破坏 `node_modules/.bin`/`pyvenv.cfg` 后 L0 仍 fresh 但 `--verify` 必须失败（这条正是 mise 短板的验证） |
| **D5 其他生态** | go / cargo / deno（+ 视情况 composer/bundler，取决于是否先补 ruby/php backend） | 每个 provider 一组 fixture；cargo 的「产物不在项目内」必须由表达能力承载而非特例分支 |
| **D6 自定义 provider + monorepo** | 自定义 `sources/outputs/run/env/dir/depends/timeout`、`disable`、显式 roots、并行与拓扑序 | 循环依赖被检出并跳过且报告；注入：允许盲扫子目录 → 「不得自动发现未声明的子项目」红 |
| **文档（每批同提交）** | README×2 用法、`site/guide/deps.md` + `site/en/guide/deps.md`、实现说明页、侧边栏×2、VitePress 生产构建 | 构建失效链接暴露 |

**建议先做 D1+D2**：它们合起来就交付了「一个 Node 项目 `osdk deps` 从零装好工具与依赖」这个完整价值，且不依赖任何待核行为。D3 依赖 §15 的 uv 待核项先核准。

---

## 14. 公开面变更预告（供评审）

- 新增一级命令 `osdk deps`（+ `status`/`doctor` 子命令）。
- 新增配置段 `[deps]`；新增 `settings.deps.{on_install,status_stale}`、`settings.<eco>.default_installer`（Node 侧已有 `settings.npm.default_installer`，Python 侧新增）。
- osdk.lock 新增 `[deps.<provider>]` 段（可选、空则不落盘、schema 不升）。
- trust 新增 `deps` 分类（默认多数不要求 trust；自定义 provider 与放开源码构建要求 trust）。

---

## 15. 诚实清单：已查证 vs 待核

**已查证（有来源）**

1. osdk 已内置 node/npm/pnpm/yarn/bun/deno/python/go/rust/java(+maven/gradle/kotlin)/zig 等 backend：`crates/osdk-core/src/backend/registry.rs:21-36`。
2. `osdk install` 无 operand 走 lock replay 装工具、有 operand 跳过 replay：`crates/osdk-cli/src/commands.rs` 的 `pub async fn install`（`explicit` / `use_lock`）。
3. npm 侧的发现、installer 优先级、lock 归属冲突：`npm_tools.rs:271/313-316/357/371`。
4. npm 默认不跑构建脚本：`npm_package.rs:786-788`（`BuildPolicy::Deny`）。
5. tasks 的 freshness 哈希模型与状态放 cache、不写项目：`tasks/freshness.rs:285-297`、`:300-316`；`sources` 匹配 0 个文件会空洞成立的坑：`tasks/freshness.rs:15`。
6. 依赖 kind 已有 Runtime/Installer/Tool：`tool.rs:102-117`。
7. lock 无 `deny_unknown_fields`、旧二进制忽略未知字段（v0.0.2 实测）：`lockfile.rs:46/326` 与 model-management 文档 §6.3。
8. mise `deps` 的分层、provider 表、freshness 模型与「不逐包核验」、monorepo 需显式 roots、pip provider 不建 venv：<https://mise.jdx.dev/dev-tools/deps.html>（experimental）。
9. uv 的 `uv sync` 语义与 `uv pip compile/sync`：<https://docs.astral.sh/uv/concepts/projects/sync/>、<https://docs.astral.sh/uv/pip/compile/>。
10. **Node 系四家的冻结与禁脚本开关矩阵**（含 yarn classic/berry 分派、classic 与 bun 缺 lock 时不失败、berry 无 `--ignore-scripts`、pnpm 12 与 bun 默认拦依赖脚本）：§5.4 实测表，2026-09-22。
11. **既有安装链能把包管理器装进隔离 install 目录且不碰项目**，以及 **deps 不能走 shim、必须用 `Backend::bin_paths` 的真实 bin + 前置 node 到 PATH**：§7.2 实测，2026-09-22。

**待核（实现前必须用能失败的验证核准，现在不得当既定事实）**

1. **uv/pip 只装预编译产物的确切开关**：`--only-binary=:all:`、`UV_NO_BUILD`/`--no-build`、`--no-build-isolation` 的语义与版本差异；uv 默认是否允许拉 sdist 并本地构建。
2. ~~npm 系的 frozen 命令与脚本开关在各 installer 上的确切形态~~ —— **已核准（2026-09-22 实测，见 §5.4）**，含版本号与反向控制。**剩余未测的两格**：yarn classic 与 yarn berry 在「lock 与清单不一致」时的行为（npm/pnpm/bun 三家已测为 exit≠0）。这一格影响的只是错误信息质量，不影响 provider 表的命令选择（osdk 自己预检 lock 存在性已覆盖更危险的那种情况）；D1 实现时补测。
3. **deno `deno install --frozen`** 是否存在及其语义（mise 表里写的是 `deno install`）。
4. **bundler / composer** 的冻结安装参数与「是否会编译 native extension / 跑脚本」，以及 osdk **目前没有 ruby/php backend**（`registry.rs:21-36` 无此二者）——D5 若要做，需先评估是否新增 backend 或要求 syspkg 提供。
5. **go**：`go mod download` 是否在任何情况下会执行代码（如 `//go:generate` 不会，但 toolchain 自动下载 `GOTOOLCHAIN` 的行为需核准），以及 `vendor/` 存在时的判定细节。
6. **cargo**：`cargo fetch --locked` 是否足以让后续离线构建成功、以及 `CARGO_HOME` 与 osdk 既有 cargo 管理（`data/cargo`）如何不打架。
7. **各 provider 的 receipt 判据是否足以发现外部篡改**（L2 深度校验的有效性）——必须按「故意破坏后 `--verify` 必须失败」实测，否则这层是装饰。
8. ~~`osdk deps add/remove` 是否应该存在~~ —— **已定（2026-09-22 用户拍板）**：不新增；`deps` 只做整份清单兑现，单包增删继续走 `osdk install <npm:pkg>`。见 §3。

---

## 16. 与 model-management 文档的关系

本文取代并深化该文档 §12.3 的骨架；§12.1（npm 现状台账）、§12.2（该不该做）、§12.4（mise 对照）、§12.5（ComfyUI B 路径阶梯）、§12.6（nvidia doctor）仍是本文的上游依据，不重复抄录。ComfyUI 的「install 后直接能跑」需要的正是本文 D1–D3：先有 `osdk deps` 的 Python 系，才谈得上 `github:` clone ComfyUI 再按 `requirements.txt` 兑现环境。
