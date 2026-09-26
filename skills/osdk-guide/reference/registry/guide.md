# `osdk registry`

诊断项目依赖包管理器将使用哪个 Registry，不管理 osdk 自身的工具下载源。

```bash
osdk registry test [MANAGER]
```

`MANAGER` 省略时检查所有支持的依赖管理器；可按需指定 npm/pnpm/yarn/bun/deno 或
Python 路径。命令会探测候选、显示选择计划，不运行项目安装。

```bash
osdk registry test
osdk registry test npm
osdk registry test python
```

持久候选与超时见 `reference/configuration/registries.md`。SDK 下载镜像使用
`osdk source`，OCI Registry 使用 `osdk container registry`，三者不要混用。
