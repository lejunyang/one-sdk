# `osdk deactivate`

打印移除 osdk shell 集成并恢复原环境的代码。

```bash
eval "$(osdk deactivate bash)"
osdk deactivate fish | source
osdk deactivate powershell | Invoke-Expression
```

语法：`osdk deactivate <SHELL>`。必须让目标 shell 执行输出，仅运行命令本身不会改变
当前父 shell。它恢复 activate 捕获的原 PATH、变量和 PowerShell prompt。

没有专属配置；与 `activate` 成对使用。
