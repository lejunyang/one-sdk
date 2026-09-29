# `osdk pkg`

查看宿主系统包管理器并兑现 `[sys.pkg]` 中的包条目。除 `apply` 与 `mirrors apply` 外均只读。

| 子命令 | 参数 | 语义 |
| --- | --- | --- |
| `doctor` | `--json` | 查看可用 manager 及状态 |
| `status` | `--missing`、`--json` | 比较声明；`--missing` 有缺包时退出非零 |
| `plan` | `--json`、`--detailed-exitcode` | 不安装；会变化时可退出 2 |
| `apply` | `--dry-run`、`--yes`、`--json` | 安装缺失包；已有包即使版本不同也保留 |
| `mirrors test` | `--manager winget`、`--json` | 测速，不改配置 |
| `mirrors apply` | `--manager winget`、`--dry-run`、`--accept-plan <FP>`、`--json` | 修改机器级源，需管理员权限 |

```bash
osdk pkg doctor
osdk pkg status --missing
osdk pkg plan --detailed-exitcode
osdk pkg apply --yes
osdk pkg mirrors test --manager winget
```

首次无人值守镜像 apply 要传刚生成的 plan fingerprint；fingerprint 绑定当时的源状态。
声明格式与平台过滤见 `reference/configuration/sys.md`。容器运行时不放进 `sys.pkg`。
