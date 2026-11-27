# `[skills]`

声明要安装给外部 AI Agent 的 skill。osdk 只下载、校验、内容寻址存储和链接，自己不执行
skill 内脚本。

```toml
[skills]
default_agents = ["claude-code", "codex"]
scope = "project"                    # project / global
link_mode = "symlink"                # 可选，覆盖 skill 的落地方式

[skills.web-design]
source = "github:vercel-labs/agent-skills"
skill = "web-design-guidelines"
ref = "branch:main"
agents = ["claude-code"]
when = { os = ["linux", "macos"] }
```

`osdk skills sync` 精确复现已有锁条目的 commit、内容哈希、Agent 列表与安装方式。
对没有锁条目的声明，它按 `when` 筛选本平台适用的技能，打印安装摘要、安装并写入锁。
已有锁条目的技能也受声明中的 `when` 限制。不匹配当前平台时，`sync` 和 `update` 都跳过该技能，
保留已有安装和锁条目。

- 声明名是锁键和安装目录名。`source` 必填，支持 GitHub 来源或本地路径。相对本地路径从
  声明 `source` 的配置文件所在目录解析，外置项目配置以实际项目根目录为基准。
  一个来源有多个技能时，`skill` 按目录选取，缺省取声明名。
- `ref` 只用于 GitHub。用 `osdk skills update <名>` 应用 ref 或 Agent 列表变更。更新时只有
  `ref = "rev:<commit>"` 会保持固定。要切换到另一个固定 commit，修改 `ref` 后运行
  `skills update`，需要联网获取新提交的归档。旧配置的裸 40 位 commit 仍按安装快照处理。
  换来源或技能时，用 `osdk skills remove <名>` 后重新同步，或显式调用 `skills add`。
- `agents` 为空时回退到 `[skills].default_agents`，再为空时检测已安装的 Agent。仅检测到
  一个时自动选中，多个或零个时使用交互选择或传 `--yes`。在 CI 中也可以明确配置 Agent。
- `link_mode` 可覆盖安装方式。`sync` 默认单目录复制、多目录自动链接。`add` 的 `--copy`
  优先于配置。`add` 在写盘前确认，`sync` 在替换内容不同的未锁定技能前确认。
  非交互环境可传 `--yes` 确认。
- `scope` 是未传 `-g` 时的默认范围。`project` 使用项目目录和项目锁。`global` 使用用户级
  Agent 目录和用户配置目录下的 `osdk.lock`。`skills path` 读取配置指定的范围。未配置时，
  它先查项目锁，再查用户锁。
- 自定义 `endpoint` 尚未实现。缺锁的声明设置该字段会报错。未知字段也会报错。
