# `osdk node`

处理 Node 专属的全局 npm 包迁移。

```bash
osdk node migrate-packages --from <NODE_VERSION> --to <NODE_VERSION> [--apply]
```

| 参数 | 语义 |
| --- | --- |
| `--from` | 源受管 Node 版本，必填 |
| `--to` | 目标受管 Node 版本，必填 |
| `--apply` | 真正执行；省略时只生成迁移计划 |

```bash
osdk node migrate-packages --from 20.11.1 --to 22.4.0
osdk node migrate-packages --from 20.11.1 --to 22.4.0 --apply
```

项目依赖迁移不走该命令，使用 `[deps.npm]` / `[deps.pnpm]` 与 `osdk deps`。Node 安装后
是否启用 Corepack 由 `[settings.node].corepack` 控制；npm 动态工具的默认 installer
由 `[settings.npm].default_installer` 控制。
