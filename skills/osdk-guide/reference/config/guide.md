# `osdk config`

定位、读取和编辑项目或用户配置中的可写 settings。

| 子命令 | 参数 | 语义 |
| --- | --- | --- |
| `path` | — | 打印解析到的配置目录/文件路径 |
| `list` | — | 打印所有解析后设置 |
| `get <KEY>` | `-g, --global` | 读取项目有效值或用户全局值 |
| `set <KEY> <VALUE>` | `-g, --global` | 写项目配置或用户配置；列表值用逗号分隔 |
| `unset <KEY>` | `-g, --global` | 删除显式值并恢复下层/默认值 |
| `migrate` | `--dry-run`、`-g, --global` | 将旧配置段名迁移到规范布局；默认处理当前项目 |

```bash
osdk config path
osdk config list
osdk config set jobs 6
osdk config set shims.exclude "apkanalyzer,lint"
osdk config get attestations -g
osdk config unset offline
osdk config migrate --dry-run
osdk config migrate
osdk config migrate --global
```

可写 key：`jobs`、`offline`、`yes`、`verify_signatures`、`require_checksums`、
`attestations`、`prerelease`、`link_mode`、`lang`、`shims.include`、`shims.expose`、
`shims.exclude`、`sources.probe_timeout_ms`、`sources.model_probe_timeout_ms`、
`sources.model_download_attempts`、`sources.model_download_retry_base_ms`、
`sources.model_jobs`、`sources.model_read_timeout_ms`、`registries.python.urls`、
`registries.npm.urls`。另接受 `shims.<tool>.include/expose/exclude` 动态 key。
布尔值接受 `true/1/yes/on` 和 `false/0/no/off`。

配置分层与全部 TOML 段从 `reference/configuration/overview.md` 进入；`config set`
不是任意 TOML 编辑器，复杂声明使用对应配置 reference。

`migrate` 只移动四组旧布局：`[aliases]` → `[alias.tools]`、`[containers]` →
`[container]`、`[task_config]` → `[task]`、`[syspkg]` → `[sys.pkg]`，并将临时的
`[sys.pkg.packages]` 条目扁平到 `[sys.pkg]`。它保留注释和
其他配置；如果同一组的新旧写法同时存在，会拒绝猜测合并顺序并保持文件不变。
