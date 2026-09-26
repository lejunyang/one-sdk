# `osdk hook-env`（内部命令）

这是 `activate` 生成的 shell hook 在每次提示符调用的内部入口，不面向手工工作流。

```bash
osdk hook-env --shell <SHELL>
```

`--shell` 默认 `bash`。命令计算当前目录的环境差量并输出 shell 代码；激活脚本负责执行。
检测到 `OSDK_TASK` 时会避让，防止任务内部再次激活并覆盖 task 环境。

不要在构建脚本中直接依赖其输出格式；使用 `activate` 安装集成，用 `exec` 构造一次性环境。
该路径位于每个 prompt 的交互热路径，配置见 `[tools]`、`[settings.shims]` 和模型 env。
