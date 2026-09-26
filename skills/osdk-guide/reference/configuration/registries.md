# `[registries]`

配置 osdk 代跑项目包管理器前的 Registry 选择；它与 osdk 下载工具用的 `[sources]`
相互独立。

```toml
[registries.npm]
urls = ["https://registry.npmmirror.com/"]
probe_timeout_ms = 1500

[registries.python]
urls = ["https://pypi.tuna.tsinghua.edu.cn/simple/"]
probe_timeout_ms = 8000
```

- 空 `urls` 使用内置公共候选。
- Python 镜像只替代默认 index，绝不排在项目自己的私有 index 之前，避免依赖混淆。
- 该段改变依赖下载来源，项目配置需要 trust。
- 用 `osdk registry test [npm|pnpm|yarn|bun|deno|python]` 查看当前选择计划。
