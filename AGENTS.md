# 开发流程

- 每完成并验证一个独立任务或功能后，先创建专门的 Git 提交，再开始下一个任务。
- 多个已完成的功能如果能安全拆分，就不要合并到同一个提交里。
- 每个提交只聚焦一处行为变更，并包含它的测试和直接相关的文档。
- 任何面向用户的能力变更，都要检查 `README.md`、`README.zh-CN.md`、`site/guide/`、`site/en/guide/` 和 `site/.vitepress/config.mts`；在同一个提交里更新所有受影响的文档，使两份 README、两种站点语言和导航结构始终与实现保持一致。
- `README.md` 和 `README.zh-CN.md` 应保持为面向使用场景的入口，只讲产品功能和用法。不要在任何一份 README 里写内部架构、实现算法或设计取舍，这些内容应放进 VitePress 中对应的实现说明章节。
- VitePress 的用户指南页和实现说明页必须中英文成对存在。新增、删除或重命名页面时，同步更新两种语言的侧边栏，并运行 VitePress 生产构建，让失效链接在校验阶段暴露出来。

- 每次提交前运行范围最小的相关测试。在宣布一个跨多个提交的工作项完成之前，运行完整的工作区验证。
- 测试和冒烟检查必须在适用处使用临时的 `HOME`、`OSDK_*`、`CARGO_HOME`、`RUSTUP_HOME` 和构建目录。不要修改或依赖用户真实的 SDK 管理器状态。
- 在 Windows 上执行脚本必须使用 PowerShell 7（`pwsh`），禁止使用 PowerShell 5（`powershell` / Windows PowerShell）。PowerShell 5 在字符编码、`Latin1` 等 .NET API 可用性和输出重定向行为上与 PowerShell 7 存在差异，会导致脚本结果不可靠。当默认 shell 为 PowerShell 5 时，通过 `pwsh -NoProfile -Command "..."` 或 `pwsh -File <script>` 显式转由 PowerShell 7 执行。
- 从 Linux 验证 Rust 代码、测试、构建脚本、安装器或 CI 时，必须在宣布任务完成前用 `./scripts/windows-wine-tests.sh` 运行完整的 Windows GNU 工作区测试套件。缺少脚本前置依赖时先安装（包括 `x86_64-pc-windows-gnu` Rust 目标和 `mingw-w64`）；该脚本会自行下载并校验其固定版本的 Wine 构建。仅做 Windows 交叉编译或 Clippy 检查不满足这项运行时测试要求。纯文档改动可豁免。
