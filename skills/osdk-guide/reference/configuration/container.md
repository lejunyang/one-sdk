# `[container]`

配置宿主原生容器运行时、Buildx builder 和 Registry 镜像策略。

```toml
[container]
runtime = "auto"                  # auto / docker / containerd
builder = "auto"                  # auto 或 Buildx builder 名
platform = "runtime"              # runtime 或 OS/ARCH[/VARIANT]
probe_timeout_ms = 1500

[container.registries."docker.io"]
mirrors = ["https://mirror.example/"]
anonymous_only = true
resolve = "upstream"              # upstream / mirror
```

高优先级配置会整体替换 `[container]`，不会逐字段合并。显式 registry policy 会完整
覆盖 Docker Hub 内置候选；镜像改写需要 trust。osdk 只驱动现有 Docker/containerd/
BuildKit，不把容器镜像存入 osdk store。
