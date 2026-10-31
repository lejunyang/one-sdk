# `osdk env`

打印激活的 shell 在当前目录下会有的环境，供没有 shell 钩子的 IDE、CI、容器和脚本
使用。只读，不安装、不改任何状态，不需要信任。

```bash
osdk env [-f, --format <FORMAT>]
```

| `--format` | 输出 |
| --- | --- |
| `dotenv`（默认） | `KEY=VALUE` 行，值不加引号；含完整 PATH |
| `json` | 单个 JSON 对象，键为变量名；含完整 PATH |
| `bash` / `zsh` | `export KEY='VALUE'`，PATH 写成在 `$PATH` 前追加 |
| `fish` | `set -gx KEY 'VALUE'`，PATH 在 `$PATH` 前追加 |
| `powershell` | `$env:KEY = 'VALUE'`，PATH 在 `$env:PATH` 前追加 |

```bash
osdk env > .env
osdk env --format json
eval "$(osdk env --format bash)"
osdk env --format powershell | Out-String | Invoke-Expression
```

内容与 `hook-env` 是同一份计算：backend 环境（`JAVA_HOME`、`ANDROID_HOME`……）、
下游缓存目录、已启用的模型 adapter，以及 shim 与各工具目录组成的 PATH。

- dotenv / json 的 PATH 以**外层激活之前**的 PATH 为基础，在已激活的 shell 里导出
  也不会把别的目录的工具写进去。
- dotenv 不加引号是刻意的：`docker --env-file` 把引号当作值的一部分。含换行的值
  无法写成 dotenv，命令报错并提示改用 json。
- shell 格式不带钩子的 `OSDK_ORIG_*` / `OSDK_MANAGED_ENV` 簿记，适合执行一次；不随
  目录切换，不能代替 [`activate`](../activate/guide.md)。
- 输出只描述当前目录；换项目要重新导出。已配置但未安装的工具会让命令失败，与激活
  一致。

要直接在这个环境里运行命令，用不带 `--tool` 的 [`exec`](../exec/guide.md)。
