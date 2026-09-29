# `[task]`

配置所有项目任务的 runner 默认值与发现范围。子项目不能声明自己的 `[task]`；
只采用根配置的值。

```toml
[task]
shell = "pwsh -Command"
dir = "."
roots = ["apps/*", "packages/*"]
includes = ["tools/tasks"]
```

| 字段 | 语义 |
| --- | --- |
| `shell` | 没有任务级 `shell` 时使用的解释器 |
| `dir` | 没有任务级 `dir` 时使用的工作目录 |
| `roots` | 显式发现子项目配置；`*` 不跨 `/`，不支持 `**` |
| `includes` | 文件任务搜索目录；设置后替换默认目录，不追加 |

子项目任务以 `//path:name` 进入统一任务图；子项目内不带前缀的依赖名留在本子项目，
显式 `//path:name` 才跨项目。该段会改变解释器和执行范围，项目配置需要 trust。
