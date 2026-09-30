# `osdk install`

安装一个或多个工具；不带操作数时按当前项目配置和 lock 复现。

```bash
osdk install [TOOL[@VERSION]...] [-o KEY=VALUE]... [--force] [--no-deps] [--include-lazy]
```

| 参数 | 语义 |
| --- | --- |
| `[TOOLS]...` | 可重复；为空时安装解析后的项目工具 |
| `-o, --opt KEY=VALUE` | backend 选项，可重复，对本次列出的所有工具生效 |
| `--force` | 已存在也重装；适合修复 `doctor --verify` 报告的漂移 |
| `--no-deps` | 仅裸 `install` 时跳过 `[deps]` 自动兑现；显式工具本就不跑 deps |
| `--include-lazy` | 裸安装也包含 `[tools]` 中 `lazy = true` 的条目；显式点名不需要此开关 |

```bash
osdk install node@20 python@3.12
osdk install rust@1.98.0 -o profile=minimal -o components=rustfmt,clippy
osdk install github:sharkdp/fd@10
osdk install                       # 按 osdk.toml / osdk.lock
osdk install --include-lazy        # 连同 lazy 工具一起安装
```

相关配置：`[tools]` 见 `reference/configuration/tools.md`；自动应用依赖见
`reference/configuration/deps.md`；源与校验策略见 `reference/configuration/sources.md`
和 `reference/configuration/settings.md`。工具请求格式见
`reference/tool-requests/guide.md`。

`install` 安装开发工具；项目自身依赖使用 `osdk deps`。模型不会作为裸 `install` 的副作用
下载，使用 `osdk model sync`。

项目内的裸 `install` 优先复现当前平台 lock；没有可用平台条目而回退到配置时，只处理
项目声明，不枚举用户全局配置中无关的固定版本，并在安装全部成功后把精确结果写回当前
平台 lock。项目外的裸 `install` 才应用全局配置，也不会在当前目录凭空创建项目 lock。
显式 `install TOOL...` 是一次性共享安装，不修改 `[tools]` 或 lock；需要项目选择时使用
`use`。工具文件仍在用户级安装池中共享，因此项目请求的同一版本已存在时会直接复用并提示
已安装。

结构化工具条目的 `lazy = true` 只影响裸安装的默认批次；显式 operand 和被其他已纳入
工具需要的 runtime 依赖不会被跳过。已有 lock 缺少 lazy 条目时，`--include-lazy` 会从
当前配置补齐它们，同时保留 lock 中已有的精确请求。
