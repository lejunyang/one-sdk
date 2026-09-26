# `osdk activate`

打印 shell 集成代码，由当前 shell 执行后按目录自动刷新工具环境。

```bash
eval "$(osdk activate bash)"
eval "$(osdk activate zsh)"
osdk activate fish | source
osdk activate powershell | Invoke-Expression
```

语法：`osdk activate <SHELL>`。支持值以当前版本 `--help` 为准；常用为 bash、zsh、
fish、PowerShell。命令只打印代码，不直接修改 profile。

激活会保留用户原 PATH/变量，`deactivate` 可恢复。任务子进程带 `OSDK_TASK`，shell hook
会主动避让，避免覆盖任务注入的环境。相关配置为 `[tools]`、`[settings.shims]`、模型
provider 的 `[sources.<provider>].env/env_force`。
