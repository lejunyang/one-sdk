---
name: osdk-guide
description: >-
  osdk（one-sdk）命令行使用指引。当需要用 osdk 安装 / 切换语言运行时、包管理器、
  开发工具、Android SDK 或大模型快照，声明并复现项目工具链（osdk.toml / osdk.lock），
  用 [tasks] 跑项目任务，装项目自身的应用依赖（[deps]），配置下载源 / 镜像 /
  离线 / 校验策略，信任项目配置，管理容器运行时与宿主包管理器，或需要查阅 osdk 的
  命令用法与 osdk.toml / config.toml 配置字段写法时使用。仅覆盖 osdk 自身的 CLI 与
  配置，不涉及被它管理的各语言工具本身的用法。
---

# osdk 使用指引

osdk（二进制名 `osdk`，仓库名 one-sdk）是一个跨平台（Windows / macOS / Linux）的
统一 SDK 管理器：用一套命令管理语言运行时、包管理器、开发工具、Android SDK 与大模型
快照，并为团队和 CI 保存区分平台、可复现的项目锁定结果。

本 skill 是 osdk 命令与配置的**速查与索引**。它不替代 `osdk --help`：`--help` 永远是某个
版本命令面的权威来源；本指引提供的是「按功能找到该用哪个命令」「配置字段怎么写」这类
`--help` 不擅长回答的问题。

## 什么时候读哪个 reference

先判断意图属于哪一类，再 `Read` 对应文件，不要凭本页摘要直接下结论：

- **要查某个命令怎么用、有哪些子命令和参数、按功能分类的命令清单** →
  `reference/commands.md`。它按「工具生命周期 / 项目复现 / 任务 / 应用依赖 / 生态专属 /
  源与安全 / 模型 / 容器 / 宿主包 / 存储缓存 / 自身与诊断」分类，覆盖全部一级命令。
- **要写或改 `osdk.toml` / 全局 `config.toml`，想知道有哪些段、字段名怎么拼、取值范围** →
  `reference/configuration.md`。它按 TOML 段（`[settings]` / `[tools]` / `[sources]` /
  `[registries]` / `[containers]` / `[syspkg]` / `[tasks]` / `[deps]` / `[models]` /
  `[aliases]` / `[task_config]`）逐一给出字段、类型、默认值与最小示例。

两个 reference 都以「与当前实现一致」为第一要求：命令面对齐 `crates/osdk-cli/src/cli.rs`
的 `enum Command`，配置字段对齐 `crates/osdk-core/src/config/mod.rs` 的 `ConfigFile` 及各
子结构。修改 osdk 能力后，这两份文档要与两份 README、两种语言的 site 文档一起同步更新
（见仓库根 `AGENTS.md`）。

## 关键前提（先记住，避免走弯路）

- **`osdk run <name>` 才是跑任务，没有裸 `osdk <name>`。** 裸形式会被以后新增的子命令
  遮蔽，因此项目任务一律 `osdk run`。
- **`install` 装工具，`deps` 装项目自己的依赖。** `[tools]` 把包管理器本身准备好，
  `osdk deps` 再驱动它读 `package.json` / `pyproject.toml` 等，把整份依赖闭包装进项目。
  增删单个依赖仍走 `osdk install <npm:pkg>`。
- **动态工具带命名空间前缀**：`npm:`、`cargo:`、`go:`、`conda:`、`pypi:`、`github:`、
  `http:`。例如 `osdk use npm:prettier@3`、`osdk install github:sharkdp/fd`。
- **会执行代码或削弱校验的项目配置要先 `osdk trust`**。仅声明「装哪些工具/包」不需要
  信任；`[syspkg]`、源/镜像改写、`allow_build_from_source`、自定义 index 等才需要。
- **配置分层，高者胜**：CLI 参数 → 环境变量（`OSDK_*`）→ 项目 `osdk.toml`（向上查找）→
  用户全局 `config.toml` → 内置默认。
- **不要手写 osdk 管控目录（data/config）下的配置或代理源**：源与配置一律通过
  `osdk source` / `osdk config` 等 osdk 自身命令管理。

## 快速定位（意图 → 命令，细节进 reference）

| 想做什么 | 入口命令 |
| --- | --- |
| 装 / 切换 / 卸载某个运行时或工具 | `osdk install` / `osdk use` / `osdk uninstall` |
| 查已装 / 可装版本、当前生效版本、安装路径 | `osdk list` / `osdk list-remote` / `osdk current` / `osdk where` |
| 固定并复现项目工具链 | `osdk use` → `osdk lock` → `osdk install`；`osdk outdated` / `osdk upgrade` |
| 用项目任务替代 Makefile | `osdk run <name>` / `osdk task list` / `osdk task info` |
| 装项目自身应用依赖 | `osdk deps [--list\|--dry-run\|--frozen\|--verify]` |
| 临时用某工具跑一条命令 | `osdk exec --tool <t> -- <cmd>` |
| Shell 按目录自动切换 | `osdk activate <shell>` / `osdk deactivate <shell>` |
| 配镜像 / 源 / 离线 / 校验 | `osdk source ...` / 全局 `--offline` `--require-checksums` `--attestations` |
| 信任 / 取消信任项目配置 | `osdk trust` / `osdk untrust` |
| 拉取 / 复现大模型快照 | `osdk model pull` / `osdk model sync` / `osdk model view ...` |
| 给 AI Agent 装 / 复现 skill | `osdk skills add` / `osdk skills sync` / `osdk skills list` / `osdk skills agents` |
| 看 / 调配置 | `osdk config path\|list\|get\|set\|unset` |
| 诊断环境、切语言 | `osdk doctor [--verify]` / `--lang en\|zh` |

> 表中列出的是「从哪里进」，具体参数、别名、平台差异与边界都在 `reference/commands.md`；
> 涉及写进 `osdk.toml` 的等价声明在 `reference/configuration.md`。
