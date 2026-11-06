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

激活会保留用户原 PATH/变量，`deactivate` 可恢复。每次刷新只原位替换 osdk 自己加入的
PATH 条目（记在 `OSDK_ADDED_PATH`），激活后用户自己加的目录（如 venv）保留在原处。任务子进程带 `OSDK_TASK`，shell hook
在任务项目根（`OSDK_TASK_ROOT`）之内主动避让，避免覆盖任务注入的环境；离开后照常激活。相关配置为 `[tools]`、`[settings.shims]`、模型
provider 的 `[sources.<provider>].env/env_force`。

Fish 中可选保存变量不存在不构成失败；成功激活的片段应返回零状态。
