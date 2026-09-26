# `osdk exec`

按需物化一个或多个工具，并用这些工具的精确环境运行一条命令。

```bash
osdk exec -t <TOOL> [-t <TOOL>...] [--no-deps] -- <COMMAND> [ARGS...]
```

| 参数 | 语义 |
| --- | --- |
| `-t, --tool <TOOL>` | 必填、可重复；指定要暴露的工具请求 |
| `--no-deps` | 跳过 `[deps]` 自动兑现 |
| `-- <COMMAND>...` | 原样作为程序与 argv 执行，不经 shell |

```bash
osdk exec --tool node@20 -- node --version
osdk exec -t rust@1.98.0 -t cargo:ripgrep@14 -- rg --version
osdk exec --no-deps -t python@3.12 -- python -c "print('ok')"
```

这条一级命令与 Lua 任务里的 `exec(...)` 不是同一个入口：前者构造临时工具环境并继承
stdio，后者在任务脚本内捕获子进程输出。相关配置是 `[tools]` 和自动执行的 `[deps]`。
