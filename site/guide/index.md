# 使用指南

osdk 用一套命令管理语言运行时、开发工具、项目依赖、模型快照、下载源和共享缓存。左侧目录按功能
类型分组；第一次使用请从[安装](./start/installation)开始，再读[快速上手](./start/quick-start)。

想了解内部如何解析、下载、验证和物化，请阅读[实现原理](/internals/)。

## 开始

| 页面 | 内容 |
| --- | --- |
| [项目介绍](./start/introduction) | osdk 解决什么问题、支持哪些平台与工具 |
| [安装](./start/installation) | 安装器、shell 配置、源码构建与自更新 |
| [快速上手](./start/quick-start) | 从安装工具到在另一台机器复现的最短流程 |

## 配置

| 页面 | 内容 |
| --- | --- |
| [配置文件](./config/files) | `config.toml` 与 `osdk.toml` 的位置、完整字段、多层合并规则 |
| [环境变量](./config/environment) | 全部 `OSDK_*` 变量与凭据变量 |
| [命令行全局参数](./config/cli-options) | 对所有子命令生效的参数 |
| [目录与存储](./config/storage) | 数据、缓存、配置目录以及内容寻址存储 |
| [项目配置信任](./config/trust) | 哪些项目配置键需要审批、信任何时失效 |

## 基本使用

| 页面 | 内容 |
| --- | --- |
| [安装、切换与查询](./basics/commands) | `install`、`use`、`list`、`exec`、别名等日常命令 |
| [Shell 激活与补全](./basics/shell) | shim、按目录自动切换与补全脚本 |
| [控制哪些命令进 PATH](./basics/path-control) | `include` / `exclude` / `expose` 的精确语义 |

## 项目工作流

| 页面 | 内容 |
| --- | --- |
| [项目工具链](./project/discovery) | 版本声明的查找顺序与生态原生版本文件 |
| [外置项目配置](./project/external-config) | 把目录绑定为项目，而不在项目目录里写 `osdk.toml` |
| [可复现锁文件](./project/lockfiles) | `lock`、`install`、`outdated`、`upgrade` 与陈旧状态 |
| [项目任务](./project/tasks) | 用 `[tasks]` 替代 Makefile，含依赖、并行与平台变体 |
| [应用依赖](./project/deps) | 用 `osdk deps` 驱动包管理器安装项目自己的依赖 |
| [GitHub Actions](./project/github-actions) | 在 CI 中安装 osdk、复用缓存并复现项目环境 |

## 语言与运行时

| 页面 | 内容 |
| --- | --- |
| [运行时总览](./runtimes/overview) | 通用命令形态与各 backend 速览 |
| [Node.js](./runtimes/node) · [Python](./runtimes/python) · [Java 与 JVM 工具](./runtimes/java) | 各运行时的安装选项与专属命令 |
| [Go](./runtimes/go) · [Rust](./runtimes/rust) · [Zig](./runtimes/zig) | 同上 |
| [Android SDK](./runtimes/android) | NDK、platform-tools、系统镜像、协议与虚拟设备 |

## 包管理器与开发工具

| 页面 | 内容 |
| --- | --- |
| [包管理器](./tools/package-managers) | npm、pnpm、Yarn、Bun、Deno 与 Registry 预检、全局安装纳管 |
| [npm 工具](./tools/npm) · [Cargo 工具](./tools/cargo) · [Go 工具](./tools/go) | 按生态安装 CLI 工具 |
| [Python 工具](./tools/python) · [Conda 工具](./tools/conda) | 同上 |
| [GitHub Release 工具](./tools/github-releases) | 从任意公开 GitHub Release 安装 |
| [直接 HTTPS 制品](./tools/http-artifacts) | 从精确 checksum 锁定的 HTTPS URL 安装 |
| [声明式 Backend](./tools/declarative-backends) | 用数据文件描述新工具，无需写代码 |

## 模型与 AI

| 页面 | 内容 |
| --- | --- |
| [模型快照](./ai/models) | Hugging Face、ModelScope、Civitai 的下载、校验、锁定与消费者视图 |
| [Agent skills](./ai/skills) | 为 Claude Code、Codex、Cursor 等 Agent 安装 skill |

## 下载源与安全

| 页面 | 内容 |
| --- | --- |
| [下载源与镜像](./sources/mirrors) | 镜像测速、固定、自定义源与故障转移 |
| [离线与预发布](./sources/offline) | 只用缓存工作，以及预发布版本策略 |
| [完整性、签名与 Attestation](./sources/integrity) | checksum、签名、GitHub Artifact Attestation 与 TLS |

## 系统集成

| 页面 | 内容 |
| --- | --- |
| [容器运行时](./system/containers) | Docker、containerd、Buildx 诊断，Registry 测试与 mirror 应用 |
| [系统包管理器](./system/packages) | `[sys.pkg]` 声明、winget 与 Linux 包管理器、镜像应用 |

## 维护

| 页面 | 内容 |
| --- | --- |
| [缓存与清理](./maintenance/cache) | 缓存分层、`cache` 与 `prune` 命令 |
| [诊断与安装校验](./maintenance/diagnostics) | `osdk doctor` 与发现被改动过的安装 |
