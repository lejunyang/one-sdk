# 功能指南

osdk 用一套命令管理语言运行时、生态工具、大模型快照、下载源与共享缓存。
本页只作为入口；每个专题页都列出了完整命令形态、参数和当前实现边界。

## 从这里开始

| 指南 | 内容 |
| --- | --- |
| [开始使用](./getting-started) | 全局参数，安装、切换、查询、卸载与版本别名 |
| [项目与配置](./projects) | 项目发现、原生版本文件、完整配置字段、合并规则与信任 |
| [可复现锁文件](./lockfiles) | `lock`、`install`、`outdated`、`upgrade` 的交互与陈旧状态 |
| [运行时与生态工具](./runtimes) | Node.js、Python、Java、Go、Rust、Maven、Gradle、Kotlin |
| [JavaScript 包管理器](./package-managers) | npm、pnpm、Yarn、Bun、Deno、Node 依赖与 Registry 预检 |
| [npm 开发工具](./npm-tools) | 用 `npm:<package>` 安装、固定、执行、升级和离线恢复 npm CLI 包 |
| [直接 HTTPS 制品](./http-artifacts) | 从严格的 HTTPS `{version}` 模板安装精确 checksum 锁定的文件或归档 |
| [模型快照](./models) | Hugging Face、ModelScope、筛选、校验、锁定与环境适配 |
| [下载源与供应链安全](./sources-security) | 镜像、离线、预发布、checksum、签名、attestation 与 GitHub Release |
| [容器运行时、Registry 与原生操作](./containers) | Runtime/cache 诊断、匿名 OCI Registry 测试、只读原生 mirror plan、直接原生镜像拉取与绑定预览的清理 |
| [存储、Shell 与扩展](./storage-shell) | CAS、缓存、目录、shim、激活、临时执行、补全、诊断与声明式 backend |

## 常见路径

第一次使用时，先完成[安装](./installation)，再按下面的目标继续：

- 为当前仓库建立工具版本：参阅[项目与配置](./projects)和[可复现锁文件](./lockfiles)。
- 管理某种语言工具链：参阅[运行时与生态工具](./runtimes)。
- 固定 npm、pnpm 或 Yarn：参阅[JavaScript 包管理器](./package-managers)。
- 安装 Prettier、TypeScript 或 scoped npm CLI 包：参阅[npm 开发工具](./npm-tools)。
- 从直接 HTTPS URL 安装 checksum 锁定的可执行文件或归档：参阅[直接 HTTPS 制品](./http-artifacts)。
- 下载模型仓库：参阅[模型快照](./models)。
- 配置企业镜像或严格校验：参阅[下载源与供应链安全](./sources-security)。
- 检查 Docker、containerd、Buildx、OCI Registry、mirror plan 或原生缓存用量，或者拉取镜像、严格限定原生清理范围：参阅[容器运行时、Registry 与原生操作](./containers)。
- 配置终端、清理空间或添加数据型 backend：参阅[存储、Shell 与扩展](./storage-shell)。

想了解一次请求内部如何经过解析、下载、验证、CAS 物化和 shim 分派，请阅读
[实现原理](./implementation/)。
