# `osdk deps`

从项目原生 manifest/lockfile 兑现完整应用依赖闭包；与安装包管理器本身的 `install`
分开。

```bash
osdk deps [PROVIDER_OR_ROOTED_ID...] [OPTIONS]
```

| 参数 | 语义 |
| --- | --- |
| `--list` | 只列当前配置根的 provider 与 freshness |
| `--all` | 与 `--list` 合用，包含 `[deps].roots` 子项目 |
| `--dry-run` | 展示命令与过期原因，不执行 |
| `--force` | 忽略 freshness |
| `--explain` | 逐 provider 展示 freshness 判据 |
| `--skip <PROVIDER>` | 可重复跳过 |
| `-F, --filter <PATTERN>` | 按子项目路径过滤；`*` 不跨 `/`，无命中报错 |
| `--no-install-tools` | 缺包管理器时失败，不自动安装；适合 CI |
| `--frozen` | 必须存在原生 lock 且执行不得改写它 |
| `--verify` | 根据包管理器 receipt 深查已安装环境漂移 |

```bash
osdk deps --list --all
osdk deps --dry-run --explain
osdk deps --frozen --no-install-tools
osdk deps npm
osdk deps //:npm
osdk deps //apps/api:uv
```

裸 provider 名选当前目录最近 root；`//:name` 指配置根；`//path:name` 精确选子项目。
无操作数会处理全部声明 root。声明、provider 字段与信任规则见
`reference/configuration/deps.md`。

成功安装后写入 `osdk.lock` 的 `run` 使用跨平台规范命令名（如 `bun`、`cargo`），
不会记录仅供 Windows 查找可执行文件使用的 `.exe` / `.cmd` 后缀。
