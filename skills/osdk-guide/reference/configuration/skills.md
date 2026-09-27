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
# endpoint = "https://..."           # 自定义来源，需要 trust
```

`source` 必填，可为 GitHub 来源或本地路径。GitHub 安装把请求 ref、解析 commit、内容哈希和
安装方式写进 lock；`sync` 始终按这些字段精确复现。`update` 重解析 lock 的请求 ref，项目配置
明确修改 `ref` 时以配置为准；仅目标 skill 内容哈希变化时重装，仓库其他变更只推进 lock。
只有显式 `ref = "rev:<commit>"` 才永久固定；已有项目保存的裸 40 位 commit 视为安装快照并迁移
为默认分支意图。`agents` 为空时回退 `default_agents`；两者都未指定时
先检测已安装 Agent：一个结果自动选中，多个或零个结果在交互终端多选，`--yes` 接受检测结果
（零个时选择全部），其他非交互调用报错。`link_mode` 未配置且未传 `--copy` 时，仅多个实际目标
目录会选择链接或复制；单一目录直接复制。安装摘要确认后才写盘；覆盖已有真实目录时先把旧
目录改名备份，激活失败则恢复。旧 lock 缺少 `install_mode` 时按已有目标类型推断；本地源与目标
重叠时在 staging 前拒绝。未知字段硬报错。
