# `osdk skills`：Agent Skill 管理的一级命令 —— 调研与设计

日期：2026-09-24

状态：**调研 + 设计，未改动任何产品代码。** 本文回答「osdk 该不该做、怎么做一个形如
`npx skills` 的 skill 管理命令」，目标是让人据以拍板并分批开工，不追求穷尽到可跳过评审。

外部事实基准日：2026-09-24。仓库代码事实均给出文件与行号；外部工具（`npx skills` /
Vercel Labs `skills` CLI、skills.sh 注册表 API、Anthropic Agent Skills 约定）行为区分
**已查证**（官方文档 / 源码 URL / 本机实测）与**推导 / 待核**（§11 诚实清单逐条列出）。凡待核项
不得在实现时当既定事实使用。

---

## 1. 一页结论

- **新增一个与「工具 / 系统包 / 模型 / 应用依赖」并列的一等资产类：Agent Skill。** 单位是
  「一个 skill 目录」（一份 `SKILL.md` + 其相邻资产），来源是**远端仓库 / URL / 本地路径**，
  真相是 skill 目录里的 `SKILL.md` frontmatter（`name` + `description`）与内容哈希；osdk 的
  职责是「解析来源 → fail-closed 下载 → 落进内容寻址 store → 链接进每个目标 Agent 的
  `skills/` 目录 → 把身份记进 `osdk.lock`」。
- **命令面是 `osdk skills <子命令>`**，与现有一级动词（`install` / `lock` / `use` / `run` /
  `model` / `deps` / `pkg` …，见 `crates/osdk-cli/src/cli.rs` 的 `enum Command`）无冲突。
  子命令对齐 `npx skills`：`add` / `list` / `find` / `remove` / `update` / `use` / `init`，
  并按 osdk 惯例补 `sync`（按 lock 复现）/ `path` / `doctor` / `agents`。
- **配置面是 `[skills]`**，与 `[models]` 同构：只声明「要哪个 skill、从哪来、装给哪些 Agent」。
  声明本身**不需要 trust**（同 `[tools]` / `[models]`：仅声明装什么）；只有会改变**字节来源**
  的键（自定义 `source` 端点）才落 `WeakensVerification`（对齐 `trust.rs` 对 `[models]` 的处理）。
- **不新造下载与安全机制**，直接复用现有链路：GitHub 解析走既有 `github` backend 思路，
  直接 URL / 归档走既有 `http` backend 的 fail-closed 规则（明文 HTTP / 凭据 / 跨源
  redirect / 缺 checksum / 超限全部 fail closed，见 README「直接 HTTPS 制品」一节与
  `crates/osdk-core/src/backend/http.rs`），内容落进既有 CAS store（`<data>/store`，
  `crates/osdk-core/src/dirs.rs:8`）。
- **与既有 `plugins/` 外部 backend 机制严格区分**（`crates/osdk-core/src/backend/registry.rs:51-61`
  从 `<config>/plugins` 与 `<data>/plugins` 加载 schema-1 声明式 TOML backend）：那是**给 osdk
  自己**加工具 backend；skills 是**给外部 AI Agent** 用的内容，osdk 只负责搬运与链接，绝不把
  skill 载入 `Registry`。两者是不同资产类，命名与代码都不复用。
- **osdk 相对 `npx skills` 的两点差异化**：(1) **不可变身份** —— 记录解析到的 commit SHA 与
  内容哈希进 lock，`sync` 可在别的机器上逐字节复现，而非 `npx skills update` 那种「更新到最新」；
  (2) **CAS 去重** —— 同一 skill 被多个 Agent 引用时只存一份，链接进各 Agent 目录，天然复用
  osdk 已有的 store 与 link mode（`hardlink` / `symlink` / `copy` / `clone`）。
- **二进制体积无风险**：skill 管理是纯 CLI 能力，永不进 `osdk-shim`；重依赖（下载 / 解压）
  已在 `install` feature 之后，skill 不是 `Backend`、不加 `Backend` trait 方法，因此不经
  `Registry::new()` 的 vtable、不给 shim 增重（AGENTS.md「二进制体积」两条硬约束天然满足）。

---

## 2. 定位：skill 在 osdk 里是第几类东西

### 2.1 五类东西，不要混

| | 单位 | 装在哪 | 真相在哪 | 谁消费 | 现状 |
| --- | --- | --- | --- | --- | --- |
| **工具（tool）** | 一个可执行工具 | osdk 隔离 install 目录 + shim | `osdk.lock [platforms]` | 用户 shell / 构建 | ✅ 已有 |
| **系统包（syspkg）** | 宿主包管理器的包 | 宿主系统 | `osdk.toml [syspkg]` | 宿主 | ✅ 已有 |
| **模型（model）** | 不可变权重快照 + 视图 | `<data>/models`、`<data>/views` | `osdk.lock [models]` | 推理程序（ComfyUI…） | ✅ 已有 |
| **应用环境（deps）** | 项目整份依赖闭包 | 项目内 `node_modules`/`.venv` | 原生 lock | 项目运行时 | ✅ 已有 |
| **Agent Skill** | **一个 skill 目录** | **CAS store + 各 Agent 的 `skills/`** | **`osdk.lock [skills]`** | **外部 AI 编码 Agent** | ❌ **缺，本文要补** |

关键观察：skill 与 model 的**形状几乎一致**——都是「从远端拉一份不可变内容、校验、落地、
写进 lock、可 `sync` 复现」，差别只在「落地后给谁用」：model 渲染成推理程序的视图，skill
链接进 AI Agent 的 `skills/` 目录。因此实现可以大量借用 `model` 那条路（`ModelDeclaration`、
`model sync`、视图渲染），而不是从零造。

### 2.2 为什么由 osdk 来做（而不是让用户装 npx skills）

- **同一处声明、同一份 lock、同一套 trust / 源策略**：项目已经用 `osdk.toml` 声明工具、任务、
  依赖、模型。把「这个仓库该给 Agent 装哪些 skill」也写进 `osdk.toml [skills]`，团队 clone
  仓库后 `osdk skills sync` 一步到位，与 `osdk install` / `osdk model sync` 一致，不必再引入
  一个 node + npx 依赖链。
- **osdk 的供应链纪律恰好对味**：skill 是会**改变 AI Agent 行为**的指令集（可能含脚本、可能
  含 prompt 注入），安装一个 skill 是一次供应链事件。osdk 已有的 fail-closed 下载、
  `--require-checksums`、`--attestations`、commit 钉定与 trust 分级，正好把 `npx skills` 里
  「clone 一个分支、拿最新」这件事收紧成「解析到不可变 commit、校验、可复现」。
- **跨平台链接 osdk 已经解决过**：symlink / junction、>260 字符路径、含空格与中文的路径，
  osdk 的 Windows runtime smoke 已经覆盖（AGENTS.md「交互延迟」「跨平台路径」两节；
  `scripts/windows-runtime-smoke.ps1`）。`npx skills` 的 symlink 模式在 Windows 上要处理的
  坑，osdk 这边有现成经验与测试骨架。

### 2.3 与既有 `plugins/` 外部 backend 的界线（必须先划清，否则会写错）

`crates/osdk-core/src/backend/registry.rs:51-61`：

```rust
// 既有：从 <config>/plugins 与 <data>/plugins 加载 schema-1 声明式 TOML backend
for directory in [dirs.config.join("plugins"), dirs.plugins()] {
    backends.extend(load_dir(&directory)? ... );
}
```

| | `plugins/`（既有） | `skills/`（本设计） |
| --- | --- | --- |
| 内容 | schema-1 声明式 TOML | SKILL.md 目录（frontmatter + 资产） |
| 谁加载 | osdk 自己（载入 `Registry`） | **不载入 osdk**；供外部 Agent 读 |
| 作用 | 给 osdk 增加一个工具 backend | 给 AI Agent 增加一项能力 |
| 落地目标 | osdk 的 config/data 目录 | 各 Agent 的 `.<agent>/skills/` |

结论：**同一个「外部、内容寻址、来自某来源」的气质，但资产类不同。** 代码上 `skills` 走独立
模块与独立 store 子目录，绝不复用 `plugins` 的 backend 加载路径，也不占用 `plugins` 这个名字。

---

## 3. 与 `npx skills`（Vercel Labs `skills` CLI）对齐与差异（已查证）

来源：<https://github.com/vercel-labs/skills> README（2026-09-24 读取）。`npx skills` 是「开放
agent skills 生态」的包管理器，支持 75+ Agent；skill 定义为带 YAML frontmatter（`name` +
`description`）的 `SKILL.md`。

| 维度 | `npx skills` | osdk 本设计 |
| --- | --- | --- |
| 子命令 | `add` / `use` / `list`(`ls`) / `find` / `remove`(`rm`) / `update` / `init` | **同名对齐**，另加 `sync` / `path` / `doctor` / `agents`（§4） |
| 来源格式 | GitHub 简写 `owner/repo`、完整 URL、仓库内直达路径、GitLab / Azure Repos、任意 git URL、本地路径、直接下载 URL | **子集 + 收紧**：GitHub 简写 / URL / 仓库内路径 / 本地路径 / 直接归档 URL；git-over-SSH 私有仓列为**待定**（§10.1） |
| 安装范围 | 项目（`./<agent>/skills/`）/ 全局（`~/<agent>/skills/`），`-g` | **同**：`-g/--global`（osdk 已有此语义） |
| 目标 Agent | `-a/--agent`，75+ Agent 各有 project/global 路径表 | **同构**：`AgentTarget` 静态表（§5.3），加表项而非 match 分支 |
| 安装方式 | symlink（推荐）/ `--copy` | **复用 osdk link mode**：symlink / hardlink / copy / clone，`--copy` 强制拷贝 |
| 版本 / 更新 | `update` 更新到最新（分支通常可变） | **不可变身份**：钉 commit SHA + 内容哈希进 lock，`update` 是显式再解析，`sync` 复现（§6） |
| 下载限额 | 10 MiB 下载 / 25 MiB 解压 / 1000 文件，可用 `SKILLS_*` 覆盖 | **复用 osdk http backend 既有限额与 fail-closed**（§7），量级与来源信任挂钩 |
| 注册表 / 搜索 | `find` 走 skills.sh 注册表（有公开 `/api/v1/` JSON API） | `find` 的注册表来源列为**待决**（§10.2）：skills.sh API 提供 search / audit / 内容 hash，但实测匿名访问被拦（需申请 key），否则退回仅 GitHub owner 扫描 |
| 私有仓认证 | git 凭据助手 → gh CLI → SSH 回退 | GitHub API 匿名 → 显式 token（`GITHUB_TOKEN`/`GH_TOKEN`）→ 待定（§10.1） |
| 供应链信任 | 无显式 trust 层 | **osdk trust 分级 + 内容摘要预览**（§7），差异化重点 |

不吸收的部分：`npx skills` 的「75+ Agent 全表」第一版不必照搬，先支持主流几个（§5.3），
其余按「加表项」增量补。

---

## 4. CLI 面（精确签名建议）

一级动词 `skills`，其下子命令。签名对齐 `cli.rs` 现有风格（`clap` derive，全局参数复用
`GlobalArgs`）：

```text
osdk skills add <SOURCE> [SKILL...]        # 从来源安装一个/多个 skill
  [-g|--global]                            # 装到用户级（各 Agent 的 ~ 目录）而非项目级
  [-a|--agent <ID>...]                     # 目标 Agent（可重复；缺省=已配置默认集/交互选择）
  [-s|--skill <NAME>...]                   # 仓库含多 skill 时按名选（'*' 全选）
  [-l|--list]                              # 只列出来源里有哪些 skill，不安装
  [--copy]                                 # 拷贝而非链接进 Agent 目录
  [--ref <COMMIT|TAG|BRANCH>]              # 指定来源版本；解析后钉 commit 进 lock
  [--no-lock]                              # 不写 osdk.lock（对齐 model pull --no-lock）
  [-y|--yes]                               # 跳过确认

osdk skills use <SOURCE> [--skill <NAME>] [--agent <ID>]
                                           # 不安装，临时取用一个 skill（对齐 npx skills use）

osdk skills list [-g] [-a <ID>...]         # 列已装 skill（别名 ls）
osdk skills find [QUERY] [--owner <OWNER>] # 搜索（注册表来源见 §10.2）
osdk skills remove [SKILL...] [-g] [-a <ID>...] [-s <NAME>...] [-y]  # 卸载（别名 rm）
osdk skills update [SKILL...] [-g] [-p]    # 再解析到当前最新并更新 lock
osdk skills init [NAME]                    # 生成 SKILL.md 模板

# osdk 特有（借 model 的经验）：
osdk skills sync [--prune] [--dry-run]     # 按 osdk.lock [skills] 复现全部（团队/CI）
osdk skills path <SKILL> [--agent <ID>]    # 打印某 skill 的 store 路径 / Agent 链接路径
osdk skills doctor [-a <ID>...]            # 诊断：Agent 目录可写性、断链、link mode、越权文件
osdk skills agents                         # 列出 osdk 认识的 Agent 及其 project/global 路径
```

设计取舍：

- **`sync` 是团队 / CI 的复现入口**，与 `model sync`、`osdk install`(无参) 同位：`add` 写 lock，
  `sync` 读 lock 复现。没有它，`[skills]` 只能写不能读回，committed lock 就不描述可复现状态。
- **`agents` 是可发现性入口**：osdk 认识哪些 Agent、各自把 skill 放哪，一条命令说清，避免用户
  猜 `-a` 该填什么。
- **不新增 `osdk skills exec / run`**：skill 不是可执行工具，不进 shim、不上 PATH。
- **裸 `osdk skills` 打印用法**（对齐其他带子命令的一级命令，如 `source` / `model`）。

---

## 5. 存储与落地

### 5.1 目录布局（对齐 `dirs.rs` 既有约定）

`dirs.rs:1-17` 的 data 布局已给 `plugins/ # future external backends` 留了位。skill 另起
子树，与 `models/` 平级：

```text
$OSDK_DATA_DIR/
├── store/                 内容寻址 blob（既有；skill 文件也落这里，去重）
├── installs/<tool>/<ver>/ 工具（既有）
├── models/<name>/         模型快照（既有）
├── skills/<id>/<hash>/    ← 新增：skill 的规范副本（canonical copy），按内容哈希分身份
├── shims/                 （既有）
└── plugins/               外部 backend（既有，勿混）
```

- **规范副本在 `<data>/skills/<id>/<hash>/`**，各 Agent 目录里的是**链接**（symlink/junction）或
  拷贝（`--copy`）指向它。这就是 `npx skills` 的 symlink 模型 + osdk 的 CAS 去重。
- Agent 目标目录**不是** osdk 的目录，而是各 Agent 约定的 `skills/` 目录（§5.3）。

### 5.2 身份与 lock（§6 展开）

skill 身份 = `来源规范化 + 解析到的 commit + 选中的 skill 名 + 内容哈希`。同一 skill 的不同
commit 是不同身份，可共存（对齐工具的 `b3-v2:` 指纹化安装根，README「快速开始」尾段）。

### 5.3 Agent 目标表（静态表 + 约定，不是 match）

对齐仓库「新增生态是加表项」的既有做法（`DYNAMIC_NAMESPACES` in `tool.rs:549`；backend
`Registry` in `registry.rs:21-36`）。定义一张 `AgentTarget` 表：

```text
AgentTarget { id, project_path, global_path }
```

第一版建议只收主流几个（其余按加表项增量补，数据来自 `npx skills` 的 Supported Agents 表，
已查证）：

| Agent | `--agent` | 项目路径 | 全局路径 |
| --- | --- | --- | --- |
| Claude Code | `claude-code` | `.claude/skills/` | `~/.claude/skills/` |
| Codex | `codex` | `.agents/skills/` | `~/.codex/skills/` |
| Cursor | `cursor` | `.agents/skills/` | `~/.cursor/skills/` |
| OpenCode | `opencode` | `.agents/skills/` | `~/.config/opencode/skills/` |
| Gemini CLI | `gemini-cli` | `.agents/skills/` | `~/.gemini/skills/` |
| GitHub Copilot | `github-copilot` | `.agents/skills/` | `~/.copilot/skills/` |
| 通用 | `universal` | `.agents/skills/` | `~/.config/agents/skills/` |

- **缺省目标策略要保守**：不默认「写进所有检测到的 Agent」。缺省 = 已配置默认集
  （`[skills].agents`，§8）或交互选择；`-a '*'` 才全写。这对齐 osdk「未经授权不向额外位置
  写入」的克制原则。
- **检测**：`skills doctor` / `agents` 可探测哪些 Agent 目录已存在，供用户参考，但探测到
  ≠ 自动写入。

---

## 6. 不可变身份与 `osdk.lock`

`osdk.lock` 增一段 `[skills]`（形状照 `[models]`，`config/mod.rs` 的 `ModelDeclaration` +
lock 侧记录）：

```toml
# osdk.lock（片段，示意）
[[skills]]
id = "web-design-guidelines"
source = "github:vercel-labs/agent-skills"
resolved_commit = "0123456789abcdef0123456789abcdef01234567"   # 解析到的不可变 commit
content_hash = "b3-v2:..."                                     # skill 目录内容哈希
skill = "web-design-guidelines"                                # 仓库内多 skill 时选中的名字
```

- `add` / `update` 做「解析浮动引用 → 钉到 commit → 记录内容哈希」；`sync` 只按 lock 重放，
  不重新解析（对齐 model 的 pull/sync 分工）。
- **浮动 vs 不可变**：`--ref branch:main` 记为浮动来源但仍钉下当次 commit；`--ref rev:<sha>`
  是不可变。命名沿用 osdk 已有词汇（README「Cargo 开发工具」：`version-only` /
  `immutable-revision` / `floating-ref`）。
- lock 不含 skill 字节（同 model lock 不含权重、http lock 不含制品字节）：`--offline sync`
  从 store 复现，store 缺失且 `--offline` 时如实报错。

---

## 7. Trust 与安全（本设计的差异化重点）

### 7.1 下载侧：直接复用 fail-closed

skill 来源里凡走 HTTP(S) 归档 / 直接 URL 的，套用既有 `http` backend 规则（README「直接
HTTPS 制品」、`backend/http.rs`）：明文 HTTP、凭据、query、跨源 redirect、浮动版本、缺
checksum 一律 fail closed；受传输上限、请求超时、归档条目数 / 展开大小上限约束。skill 的
限额可对齐 `npx skills`（10 MiB 下载 / 25 MiB 解压 / 1000 文件）作为默认下限，取二者更严者。

### 7.2 配置侧：`[skills]` 的 trust 归类

对齐 `trust.rs` 对各段的既有判定（`trust.rs:150-194`：除 `tools`/`aliases` 外每个顶层键都
需 trust，但 `[models]` 里只有改变字节来源的键才是 `WeakensVerification`）：

- **仅声明 `[skills.<name>] source = "github:owner/repo"` 不需要 trust**——等同声明装哪个
  model，只说「要什么」。
- **改变字节来源的键需 `WeakensVerification`**：自定义 `endpoint` / 非默认下载源 / 关闭
  checksum 要求。
- **`[skills]` 不引入 `ExecutesCode`**：osdk 只搬运与链接 skill 文件，**自己不执行** skill 里的
  脚本（执行发生在别的 AI Agent 里）。这条要在实现里守住：`skills add` 绝不 source / 运行
  skill 内任何脚本。

### 7.3 内容侧：安装前摘要预览（新机制，skill 特有）

skill 会改变**下游 AI Agent 的行为**，这是 model / tool 没有的风险面。建议：

- **首次安装某 skill 前，打印其 `SKILL.md` frontmatter（`name` + `description`）与内容摘要**
  （文件清单、是否含可执行脚本、是否含 `SKILL.md` 之外的指令文件），让用户在写进 Agent 目录前
  看清「这份 skill 会让 Agent 获得什么能力、读到什么指令」。`-y` 跳过，CI 显式声明信任来源。
- **记录内容哈希**，`skills doctor` 能报告 Agent 目录里的 skill 是否被就地改动（对齐
  `doctor --verify` 对工具文件的「装后被改」检测思路，`cli.rs:423-441`）。
- **可选地展示第三方审计结论作为参考，但不作信任依据**：skills.sh 有
  `GET /api/v1/skills/audit/{source}/{skill}` 端点，返回 Socket / Snyk / Agent Trust Hub 等
  合作方的 pass/warn/fail 与风险级别（NONE…CRITICAL）（已查证，§11）。若来源正是 skills.sh，
  `find` / `add` 可顺带把这些审计打印出来给用户参考；但 osdk **不得**把「skills.sh 说 pass」
  当成放行依据——它是第三方结论、可能缺失（首装几分钟内无审计、返回 404）、且实测该 API 匿名
  被拦（§10.2）。osdk 自己的 fail-closed 下载与内容哈希才是信任边界，审计只是附加信息。
- 这不是要 osdk 审查 skill 语义（做不到也不该做），而是把「装了什么」摊开，把决定权交给用户。

### 7.4 跨平台落地安全

- **符号链接**：Windows 上 symlink 需要权限 / 开发者模式；osdk 对 Android 已用 junction
  处理过（README「Android SDK」尾段）。skill 链接沿用 osdk 的 link mode 选择与回退
  （symlink 不可用则 hardlink / copy），并让 `--copy` 成为无链接环境的确定出口。
- **路径归一**：写进 lock / manifest 的**相对路径一律归一成 `/`**；表示某机位置的**绝对路径
  保留原生分隔符、读侧同时接受两种分隔符**——严格遵循 AGENTS.md「跨平台路径」三条约定，
  尤其是 skill 目录清单要能在 Windows 写、Linux 读。
- **长路径 / 空格 / 中文**：纳入 `windows-runtime-smoke.ps1` 覆盖范围（AGENTS.md 已要求
  Windows 侧改动跑该脚本）。

---

## 8. 配置面 `[skills]`（osdk.toml）

与 `[models]` 同构（`config/mod.rs` 的 `ModelDeclaration`），键是逻辑名：

```toml
[skills.web-design]
source = "github:vercel-labs/agent-skills"   # 必填：来源
skill = "web-design-guidelines"               # 仓库内多 skill 时选名（可选）
ref = "branch:main"                           # 可选：来源版本；解析后钉 commit 进 lock
agents = ["claude-code", "codex"]             # 可选：装给哪些 Agent；缺省用顶层默认
when = { os = "linux" }                       # 可选：平台过滤（复用既有词汇）
# endpoint = "https://..."                    # 可选：自定义来源 —— 这一项使本条需 trust

[skills]
# 顶层默认：不写则 add 时须显式 -a 或交互选择
default_agents = ["claude-code"]
scope = "project"                             # project（默认）/ global
link_mode = "symlink"                         # 可选：覆盖全局 link_mode，仅对 skill 生效
```

- `deny_unknown_fields`：拼错字段硬报错（对齐 `[models]` / `[deps]` 的既有严格性）。
- 「只声明要什么」不阻断普通命令（对齐 model 声明不阻断工具命令）。
- **配置字段最终形态需与 §4 CLI 参数逐一对齐**，避免命令能做而配置说不出（或反之）。

---

## 9. 二进制体积与执行边界（AGENTS.md 硬约束核对）

- **永不进 shim**：skill 管理是 CLI-only 能力。实现落在 `osdk-cli` + `osdk-core` 的
  `install` feature 之后，`osdk-shim`（`default-features = false`）不编译它。
- **不加 `Backend` trait 方法**：skill 不是 backend，不进 `Registry::new()` 的 13 个
  `Arc<dyn Backend>`，因此不进 vtable、不给 shim 增重（AGENTS.md「`Backend` trait 上新增
  方法，代价会落到 shim 身上」）。这是选择「skill 独立模块而非做成 backend」的一个额外理由。
- **重依赖判断哪一侧**：下载 / 解压 / git 交互若引入新依赖，一律 `optional = true` 并只在
  `install = [...]` 里 `dep:` 引入，绝不进默认依赖（AGENTS.md「新增依赖前先判断它属于哪一侧」）。
  优先复用现有 http / 归档栈，不新增第二份。
- **交互延迟无关**：skill 命令不在 `hook-env` / shim 热路径上，不触发 `cargo bench` 门槛；但
  若 skill 落地改动了 inventory 扫描能看到的子树，则按 AGENTS.md「交互延迟」重新对基准。
  ——**结论：建议 skill store 子树对 inventory 扫描不可见**（它不是工具安装），避免拖慢扫描。

---

## 10. 待决问题（需要拍板，会显著改变实现范围）

### 10.1 私有仓 / git-over-SSH 认证

`npx skills` 支持 git 凭据助手 → gh CLI → SSH 回退。osdk 现有下载栈是 HTTP fail-closed，
**不 shell out git**。两条路：

- **A（保守，推荐先做）**：只支持 GitHub / GitLab 的 **API + 归档下载**（匿名 → 显式
  `GITHUB_TOKEN`/`GH_TOKEN`），私有仓靠 token。优点：不新增 git 依赖、全程 fail-closed、与
  osdk 现有安全模型一致。缺点：不支持任意 git host 的 SSH。
- **B（对齐 npx skills）**：允许 `osdk skills add git@...`，shell out 到宿主 `git`。优点：覆盖
  全，私有仓开箱。缺点：引入宿主 `git` 依赖、跳出 osdk 的下载 / 校验闭环，trust 语义更复杂。

建议 P0 做 A，B 视需求再议。

### 10.2 `find` 的注册表来源（skills.sh 已实测，含一条硬约束）

`npx skills find` 依赖 skills.sh。它确有一套公开 JSON API（`https://skills.sh/api/v1/`，已查证
其官方 API 文档），对 osdk 尤其顺手的是：

| 端点 | 返回 | 对 osdk 的价值 |
| --- | --- | --- |
| `GET /skills/search?q=&limit=` | 命中列表；单词=模糊、多词=语义 | `find` 的直接后端 |
| `GET /skills?view=all-time\|trending\|hot` | 排行榜（分页） | 无关键词时的浏览 |
| `GET /skills/curated` | 官方一方 skill 集（约 342 个 / 87 owner） | 可信来源白名单候选 |
| `GET /skills/{source}/{skill}` | 完整文件树 + **内容 SHA-256 `hash`** | 直接喂 §6 的不可变身份 / §7.3 的预览 |
| `GET /skills/audit/{source}/{skill}` | 第三方安全审计（pass/warn/fail + 风险级别） | §7.3 的参考信息 |

每个 skill 对象带稳定 `id`（`{source}/{slug}`）、`installUrl`（即 `owner/repo`，可直接交给
§10.1-A 的下载路径）、`isDuplicate`（fork/抄袭标记）。

**一条实测硬约束（会改变 P0 是否含 `find`）**：API 文档称「匿名可访问，仅限流更严」，但本机
用带浏览器 UA 的请求实测得到 **401 Unauthorized**，自动化抓取路径也被 `robots.txt` 拒绝
（2026-09-24 实测，§11）。osdk 的 HTTP client 固定发 `osdk/<version>` UA
（`crates/osdk-core/src/http/mod.rs:21`），大概率同样吃 401。含义：**要对接 skills.sh，很可能
必须走 `Authorization: Bearer <key>`（需向 `skills-api@vercel.com` 申请），不能假设匿名可用。**

三条路（按 osdk 意愿排序）：

- **A（P0 最稳）**：不接任何集中注册表，`find` = 按 GitHub owner / 关键词扫描仓库（复用 GitHub
  搜索 API，匿名 → `GITHUB_TOKEN`）。无第三方绑定、无 key 依赖。**或干脆 P0 省掉 `find`**，
  只保留 `add <owner/repo> --list`，把搜索留给 P1。
- **B（联邦 skills.sh，P1+）**：接入上表的 API，但因匿名被拦，要么内置/让用户配 API key，
  要么接受 `find` 在无 key 时不可用并如实报错。绑定第三方 API 的稳定性与限流也要承担。
- **C（混合）**：`find` 默认走 A 的 GitHub 扫描；检测到用户配了 skills.sh key 时，额外用
  skills.sh 的 search/audit 增强结果。

建议 **P0 走 A（或省掉 `find`）**；skills.sh 的 search/audit/hash 作为 P1 的增强项（C），
且实现里必须把「匿名被拦、需 key」当既定前提，而不是文档里的「匿名可访问」。

### 10.3 与 `.agents/skills/` 的多 Agent 共享目录

多个 Agent（Codex/Cursor/OpenCode…）项目路径都是 `.agents/skills/`（已查证）。装一次即多个
Agent 共享，`remove` 时的归属计数要按「哪些 Agent 还引用它」判断——这类**共享目录 + 归属**
的坑，AGENTS.md「测试规模不足以暴露冲突」明确点过（N=1 测不出、N=2 才暴露）。实现与测试**至少
两个共享同目录的 Agent**同时在场。

### 10.4 是否需要 `skills.lock` 独立文件

model / tool 都进统一 `osdk.lock`。skill 建议**也进 `osdk.lock` 的 `[skills]` 段**，不另起
文件，保持「一个仓库一份 lock」。除非未来 skill 数量 / 更新频率与工具差异过大再拆。

---

## 11. 诚实清单（已查证 / 待核）

**已查证（附来源）**

- osdk 一级命令面与全局参数：`crates/osdk-cli/src/cli.rs`（`enum Command`），实测
  `osdk 0.0.2 --help` 输出一致（38 个一级命令，无 `skills`）。
- 既有外部 backend 机制从 `<config>/plugins` 与 `<data>/plugins` 加载 schema-1 声明式 TOML：
  `crates/osdk-core/src/backend/registry.rs:46-61`。
- data 目录布局与 `plugins/ # future external backends` 预留：`crates/osdk-core/src/dirs.rs:1-17`。
- trust 判定「除 tools/aliases 外每个顶层键需 trust」与两档 `ExecutesCode` /
  `WeakensVerification`：`crates/osdk-core/src/trust.rs:86-194`。
- `[models]` 声明式形状与 `deny_unknown_fields`、model pull/sync 分工：
  `crates/osdk-core/src/config/mod.rs:960-1006`、`cli.rs:731-808`。
- CAS store、link mode、指纹化安装根共存：README.zh-CN.md「快速开始」尾段；`dirs.rs`。
- `npx skills` 子命令 / 来源格式 / 范围 / symlink / 限额 / Supported Agents 表：
  <https://github.com/vercel-labs/skills> README（2026-09-24 读取）。
- skills.sh 公开 API 的端点与响应字段（`/skills`、`/skills/search`、`/skills/curated`、
  `/skills/{source}/{skill}` 含 `hash` + `files`、`/skills/audit/...`）：
  <https://skills.sh/docs/api>（2026-09-24 读取）。
- **skills.sh API 匿名访问实测被拦**：带浏览器 UA 的 `Invoke-RestMethod` 返回 401，自动化抓取
  被 `robots.txt` 拒绝（2026-09-24 本机实测）。文档写「匿名可访问」，与实测不符——以实测为准。
- osdk HTTP client 固定发 `osdk/<version>` User-Agent：`crates/osdk-core/src/http/mod.rs:21`。

**待核（实现前必须验证，不得当既定事实用）**

- 各 Agent 的 `skills/` 具体路径是否随 Agent 版本变化（本文表格取自 `npx skills` README 快照，
  Agent 侧可能自行调整）。实现时应以「可配置的 `AgentTarget` 表 + 可被用户覆盖」而非硬编码。
- osdk 现有 `github` backend 是否能直接解析「仓库子树 / 非 Release 内容」，还是只解析 Release
  资产（README 措辞是「公开 GitHub Release」）。若只支持 Release，则 skill 的「仓库内 skill 目录」
  拉取需要新增 GitHub API 取 tree / tarball 的路径——属于 §10.1 的 A 方案范围，需实测。
- skill 目录内容哈希方案是否直接复用工具的 `b3-v2:` 身份哈希实现，还是需要一套面向「目录树」
  的哈希（工具是「一个安装根」，skill 是「一份目录内容」，粒度需确认）。
- `[skills]` 段进入 lock 后，`--offline` 复现路径是否能完全复用 model 的离线重放（model 是权重，
  skill 是文本目录，store 落地方式需确认一致）。
- skills.sh API 用 `Authorization: Bearer <key>` 是否真能解除 §10.2 实测到的 401（本机无 key，
  未能验证 key 之后是否放行、限流与稳定性如何）；以及它对内容 `hash` 用的算法是否与 osdk 的
  `b3-v2:` 一致（文档写 SHA-256，osdk 身份哈希是 BLAKE3，两者不能直接互认，需各自计算）。

---

## 12. 分批建议（据以开工）

- **P0（最小可用，本地 + GitHub）**：`skills add <owner/repo>[--list][--skill]`、`list`、
  `remove`、`sync`、`path`、`agents`；来源支持 GitHub（API + tarball，§10.1-A）与本地路径；
  落进 CAS store + symlink/copy 进 §5.3 的主流 Agent；`[skills]` 配置段 + `osdk.lock [skills]`；
  安装前摘要预览（§7.3）。**验收**：clone 一个仓库 → `osdk skills sync` → 目标 Agent 目录出现
  skill；改动源 commit → `update` → lock 变化、内容哈希变化；`doctor` 能报断链与就地改动。
- **P1**：`update` 的批量 / 范围语义、`use`（临时取用）、`find` 最小版（§10.2-A：GitHub 扫描，
  不依赖 skills.sh key）、更多 Agent 表项、`--copy` 与 link mode 覆盖、共享 `.agents/skills/`
  的归属计数（§10.3，测试 N≥2）。
- **P2（视需求）**：git-over-SSH / 任意 host（§10.1-B）、接入 skills.sh 增强 `find`
  （§10.2-B/C，需处理 API key 与 401）、`attestations` / 签名校验接入 skill 下载。

每批遵循 AGENTS.md：单独 commit、跑范围最小的相关测试、跨平台改动在另一侧实跑、Windows 落地
改动跑 `windows-runtime-smoke.ps1`、面向用户能力变更同步两份 README + 两语言 site 文档 +
`skills/`（本仓库新增的指引目录）。
