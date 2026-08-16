# 项目介绍

osdk（one SDK manager）是一个面向 Windows、macOS 和 Linux 的多语言 SDK
版本管理器。它把原本分散在 nvm、pyenv、SDKMAN!、rustup 等工具中的常见操作，
统一成一套命令、目录模型、缓存和项目配置。

## 为什么使用 osdk

开发者通常需要同时维护 JavaScript、Python、Java、Go、Rust 等工具链。每种
生态都有不同的版本管理器、镜像设置、缓存位置和激活方式，最终带来重复下载、
磁盘浪费和难以复现的环境。

osdk 重点解决四个问题：

1. **统一操作界面**：安装、切换、锁定、升级、卸载和执行命令都使用相同语法。
2. **减少重复占用**：相同内容只在 BLAKE3 内容寻址存储中保留一份。
3. **兼顾速度与可信度**：自动选择最快来源，同时执行校验和、签名与可选的
   GitHub Artifact Attestation 验证。
4. **统一模型资产**：把 Hugging Face 与 ModelScope 仓库解析为不可变快照，
   按文件校验、缓存、去重和锁定。

## 支持的平台与工具

osdk 原生运行在 Windows、macOS 和 Linux，当前内置以下后端：

| 工具 | 获取方式 |
| --- | --- |
| Node.js | nodejs.org 预编译包，验证 `SHASUMS256` |
| npm | 独立 npm registry 包，验证 SRI |
| pnpm | npm 官方平台包，验证 SRI |
| Yarn | `yarn` / `@yarnpkg/cli-dist` npm 包 |
| Python | python-build-standalone 发布索引与 Astral 镜像 |
| Java | Foojay JDK/JRE + 内置 Temurin LTS catalog |
| Maven / Gradle / Kotlin | 独立 JVM 工具 backend 与上游 checksum |
| Go | go.dev 下载索引与 SHA-256 |
| Rust | 隔离的 rustup 工具链目录 |
| Deno | 官方 npm 平台包 |
| Bun | 官方 npm 平台包 |
| GitHub Release | `github:owner/repo` 通用后端 |
| Hugging Face / ModelScope 模型 | 不可变快照、多文件 SHA-256、共享 CAS 与 `[models]` lock |

## 工作原理

一次安装会经过统一管线：

1. 解析版本请求和用户别名；
2. 探测并选择下载来源；
3. 下载或复用缓存的归档；
4. 验证校验和、签名或 attestation；
5. 安全解压；
6. 将文件写入内容寻址存储；
7. 用硬链接、reflink 或复制物化安装目录；
8. 生成 shim，让项目或全局版本可以直接执行。

版本目录和内容存储位于同一文件系统时，osdk 优先使用硬链接；不可用时自动
回退，不会牺牲正确性。

## 配置优先级

配置按以下优先级合并，前者覆盖后者：

1. 命令行参数；
2. `OSDK_*` 环境变量；
3. 当前目录向上查找的 `osdk.toml` / `.osdk.toml`；
4. 用户级 `config.toml`；
5. 内置默认值。

osdk 还会读取 `.tool-versions` 以及 `.nvmrc`、`.python-version`、
`go.mod`、`rust-toolchain.toml` 等生态原生文件。

## 下一步

- [安装 osdk](/guide/installation)
- [查看详细功能](/guide/features)
- [浏览源代码](https://github.com/lejunyang/one-sdk)
