# `osdk pkg`

查看宿主系统包管理器并兑现 `[sys.pkg]` 中的包条目。除 `apply` 与 `mirrors apply` 外均只读。

| 子命令 | 参数 | 语义 |
| --- | --- | --- |
| `doctor` | `--json` | 查看可用 manager 及状态 |
| `status` | `--missing`、`--json` | 报告合法条目的状态。无效键返回 1。`--missing` 有缺包时也返回 1 |
| `plan` | `--json`、`--detailed-exitcode` | 预览计划。带 `--detailed-exitcode` 时有待办返回 2，无待办返回 0。出错返回 1 |
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

`status` 的 JSON 保留 `invalid_keys` 字符串数组。`plan` 和通过启动检查的 `apply`
（含 `--dry-run`）在查询包管理器之前校验包键。发现无效键时，命令退出 `1`。
`--json` 在 stdout 输出 `refused.reason: "invalid-package-keys"` 和
`refused.detail.invalid_keys`。`apply` 还会输出 `applied: false`。

项目有本机适用的包声明时，`apply` 会先检查信任。信任检查失败时退出 `1`，
stdout 为空，stderr 报告信任错误。批准项目前用 `osdk pkg plan --json` 查看无效键。

首次无人值守镜像 apply 要传刚生成的 plan fingerprint；fingerprint 绑定当时的源状态。
声明格式与平台过滤见 `reference/configuration/sys.md`。容器运行时不放进 `sys.pkg`。
