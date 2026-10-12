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
2. **减少重复占用**：让多个已安装版本复用相同文件，并集中管理各生态缓存。
3. **兼顾速度与可信度**：自动选择可用来源，校验上游 checksum；在 backend
   支持时验证签名，并可按策略验证 GitHub Artifact Attestation。
4. **统一模型资产**：下载、校验、缓存并锁定 Hugging Face、ModelScope 与精确 Civitai LoRA 版本快照。

## 支持的平台与工具

osdk 原生运行在 Windows、macOS 和 Linux，当前内置以下后端：

| 类别 | 当前支持 |
| --- | --- |
| 运行时 | Node.js、Python、Java JDK/JRE、Go、Rust、Zig、Deno、Bun |
| 包管理器与 JVM 工具 | npm、pnpm、Yarn、Maven、Gradle、Kotlin |
| Android SDK | NDK、platform-tools、build-tools、cmdline-tools、platforms、emulator 与系统镜像 |
| 其他开发工具 | 通过 `npm:<package>` 安装 npm CLI 包、通过 `cargo:...` 安装 Registry crate 或 HTTPS Git 仓库、通过 `go:<module-or-command-path>` 安装 Go command package、通过 `pypi:<project>` 安装 Python CLI、通过 `conda:<package>` 安装 conda 包、通过 `github:owner/repo` 安装公开 GitHub Release，或通过 `http:https://...{version}...` 安装精确 checksum 锁定的制品 |
| 模型平台 | Hugging Face、ModelScope、Civitai LoRA 版本 |
| 项目输入 | `osdk.toml`、`.tool-versions` 和常见生态版本文件 |
| Shell | Bash、Zsh、Fish、PowerShell |

## 配置优先级

配置的总体优先级如下，前者优先：

1. 命令行参数；
2. `OSDK_*` 环境变量；
3. 当前目录向上查找的 `osdk.toml` / `.osdk.toml`；
4. 用户级 `config.toml`；
5. 内置默认值。

覆盖粒度不是所有字段逐项合并：高优先级文件只要出现 `[settings]`，就整段替换
低优先级设置，未写字段回到内置默认值；`[sources]` 的顶层选择、探测超时和 TTL
同样整段替换，但 `sources.<tool>` 按工具键合并；`[registries.npm]` 整段替换。
`[tools]` 与 `[alias.tools]` 则按键合并。完整字段和精确规则见
[配置文件](../config/files)，全部环境变量见[环境变量](../config/environment)。

osdk 还会读取 `.tool-versions` 以及 `.nvmrc`、`.python-version`、
`go.mod`、`rust-toolchain.toml` 等生态原生文件；不同命令对这些文件的使用范围见
[项目工具链](../project/discovery#项目版本发现)。

## 下一步

- [安装 osdk](/guide/start/installation)
- [快速上手](/guide/start/quick-start)
- [浏览使用指南](/guide/)
- [阅读实现原理](/internals/)
- [浏览源代码](https://github.com/lejunyang/one-sdk)
