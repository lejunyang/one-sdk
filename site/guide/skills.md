# Agent skills

`osdk skills` 安装 **Agent skill** —— 带 `SKILL.md` 的指令包，供 Claude Code、Codex、Cursor、
OpenCode、Gemini CLI、GitHub Copilot 等 AI 编码 Agent 读取。skill 是给外部 Agent 用的内容，
osdk 只负责三件事：从来源下载、把内容放进与工具共用的 BLAKE3 CAS、再链接进各 Agent 的 skills
目录。**osdk 自己不执行 skill 里的任何脚本**。

skill 与模型快照同属「从远端拉一份不可变内容、校验、落地、写 lock、可复现」的一类，因此复用了
osdk 已有的内容寻址存储、link mode 与 `osdk.lock`。

## 命令参考

```text
osdk skills add <SOURCE>
  [-s|--skill NAME]...        # 仓库含多 skill 时按名选装（* 全选）
  [-a|--agent ID]...          # 目标 Agent；缺省用 [skills].default_agents
  [-g|--global]               # 装到 Agent 的用户级目录而非项目
  [--copy]                    # 拷贝而非链接
  [--ref REF]                 # GitHub 版本：branch:main / tag:v1 / rev:<sha> / 分支名 / commit
  [-l|--list]                 # 只列来源里有哪些 skill，不安装
  [--no-lock]                 # 不写 osdk.lock

osdk skills list [-g]
osdk skills remove NAME [-g] [-a ID]...
osdk skills sync [-g]
osdk skills path NAME
osdk skills agents
```

### 来源格式

| 形式 | 例子 |
| --- | --- |
| GitHub 简写 | `owner/repo` |
| GitHub 带命名空间 | `github:owner/repo` |
| 仓库内子目录 | `github:owner/repo/skills/web-design-guidelines` |
| github.com URL | `https://github.com/owner/repo/tree/main/skills/x`（`tree/<ref>` 会被剥离） |
| 本地路径 | `./my-skills`、`../x`、`/abs/x`、`~/x` |

一个来源可以是**单个 skill**（根目录直接有 `SKILL.md`），也可以是**一批 skill**。批量发现会查
仓库根、其直接子目录，以及约定容器 `skills/`、`.agents/skills/`、`.claude/skills/`；用 `-s` 按
目录名选装，`-s '*'` 全装。

## 目标 Agent

`osdk skills agents` 列出 osdk 认识的 Agent 及其 project / global skills 目录：

| Agent | `--agent` | 项目目录 | 用户级目录 |
| --- | --- | --- | --- |
| Claude Code | `claude-code` | `.claude/skills` | `~/.claude/skills` |
| Codex | `codex` | `.agents/skills` | `~/.codex/skills` |
| Cursor | `cursor` | `.agents/skills` | `~/.cursor/skills` |
| OpenCode | `opencode` | `.agents/skills` | `~/.config/opencode/skills` |
| Gemini CLI | `gemini-cli` | `.agents/skills` | `~/.gemini/skills` |
| GitHub Copilot | `github-copilot` | `.agents/skills` | `~/.copilot/skills` |
| 通用 | `universal` | `.agents/skills` | `~/.config/agents/skills` |

多个 Agent 共用 `.agents/skills` 是刻意的：装一次即被它们共享，`remove` 时按「还有哪些 Agent
引用」计数。缺省不会写进「检测到的所有 Agent」——要么显式 `-a`，要么配 `[skills].default_agents`，
否则报错并提示怎么选。

## 不可变身份与复现

`add` 把身份写进 `osdk.lock`：

```toml
[skills.web-design-guidelines]
source = "github:vercel-labs/agent-skills/skills/web-design-guidelines"
content_hash = "b3-v2:…"                 # 落地内容的 BLAKE3 摘要
resolved_commit = "063bee94…"            # 浮动 ref 解析到的不可变 commit
agents = ["claude-code"]
```

`osdk skills sync` 据此复现：优先用已落地的内容寻址副本，副本不在时对 GitHub 源**按记录的
`resolved_commit` 重新下载**并重算 `content_hash`，与 lock 不符就 fail-closed 拒绝——移动过的
tag 或被替换的镜像都装不进来。本地源丢了副本无法复现，会如实报告。

## 落地方式

默认目录链接：Windows 用 junction、Unix 用 symlink（都无需特权）。无链接环境或显式 `--copy`
时整树拷贝。无论哪种，都**拒绝覆盖非 osdk 放置的真实目录**，避免删掉用户自己的文件。link mode
可用 `[skills].link_mode` 覆盖，仅对 skill 生效。

## 安全

- **下载 fail-closed**：GitHub tarball 走 osdk 既有下载栈，受归档大小 / 条目数上限约束；skill
  文件数（默认 1000）与总体积（默认 25 MiB）也有上限，超限报错而不是把整仓库塞进来。
- **安装前预览**：首次安装会打印 skill 的 `name`、`description`、文件数、体积，以及是否含脚本类
  文件，让你在写进 Agent 目录前看清「这份 skill 会让 Agent 读到什么」。
- **osdk 不执行 skill**：搬运与链接由 osdk 做，执行发生在下游 Agent 里。
- **信任门槛**：只读命令（`agents` / `list` / `path`）不触发；`add` / `remove` / `sync` 会写盘、
  保持 gated。声明式 `[skills]` 只声明「要什么」不需要 trust，只有 `endpoint` / 自定义来源这类
  改变字节来源的键才需要。

## 声明式配置

也可以在 `osdk.toml` 里声明，让团队 `osdk skills sync` 一步到位：

```toml
[skills]
default_agents = ["claude-code"]

[skills.web-design]
source = "github:vercel-labs/agent-skills"
skill = "web-design-guidelines"
ref = "branch:main"
agents = ["claude-code", "codex"]
```

顶层 `[skills]` 支持 `default_agents`、`scope`（`project`/`global`）、`link_mode`；每条
`[skills.<名>]` 支持 `source`、`skill`、`ref`、`agents`、`when`、`endpoint`，其中拼错的字段会
硬报错。仓库内给 AI Agent 阅读的 `osdk-guide` 指引里，`reference/configuration.md` 有逐字段说明。
