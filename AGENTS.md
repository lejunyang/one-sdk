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

# 二进制体积

用户下载的就是这两个二进制，体积是产品指标而不是实现细节。以下几条不是风格偏好，而是踩过的坑：忽略其中任何一条都曾让体积成倍增长，或让优化悄悄失效。

- 发布体积的基准线：`osdk` 约 9.3 MB、`osdk-shim` 约 7.4 MB。改动如果让任一个二进制增长超过 10%，要么找出原因，要么在提交说明里讲清为什么这个代价值得付。用独立的构建目录实测，别凭感觉判断：
  `$env:CARGO_TARGET_DIR="target\size-check"; cargo build --release --bin osdk --bin osdk-shim`

- **`Backend` trait 上新增方法，代价会落到 shim 身上。** `Registry::new()` 会把全部 13 个 backend 实例化成 `Arc<dyn Backend>`，于是每个方法都进入 vtable，链接器无法证明它不可达，也就无法裁掉。shim 实际只用其中的只读子集（`list_installed`、`bin_paths`、`bin_names`、`exec_env`、`idiomatic_files`），却因此被动保活了整条安装链路 —— 包括 `pipeline::run` 和它背后的 sigstore 校验（`sigstore` 子树占 `osdk-core` 314 个依赖 crate 中的 240 个）。需要新增只有安装路径才用得到的能力时，优先考虑放进独立 trait 或独立类型，而不是加宽 `Backend`。

- **`osdk-core` 目前没有 `[features]` 段，所有依赖无条件启用。** 这意味着任何新依赖都会同时进入两个二进制，包括根本用不到它的 shim。加重依赖（HTTP 栈、加密、解压、正则引擎）前先确认它是否真的属于两者共同需要；如果只有 `osdk` 需要，正确做法是引入 feature 门控，而不是直接加到默认依赖里。

- **`[profile.release]` 里的 per-package `opt-level` override 是承重结构，不是装饰。** 整体使用 `opt-level = "z"` 换体积，但这会让 sha2 的可移植实现损失约 65% 吞吐（实测 2300 MiB/s 降到 800 MiB/s），而每个下载的归档都要做校验。把哈希相关的几个 crate 固定回 `opt-level = 3` 可以完全恢复速度，代价只有约 0.02 MB。**Cargo 对匹配不到任何包的 override 只发 warning、不报错**，所以依赖改名或手误会让这段保护静默失效；`crates/osdk-core/src/pipeline/verify.rs` 里的 `hashing_crates_are_pinned_to_a_fast_opt_level` 就是为此存在的，改动 profile 后不要绕过它。

- 调整 profile 时，用户真正在意的两个指标要分别测量，因为它们会朝相反方向变化：shim 的启动延迟（进程创建占主导，`opt-level` 影响很小）和归档校验吞吐（对 `opt-level` 极其敏感）。只测一个就下结论会得出错误的取舍。

- 依赖体积的排查手段：`cargo tree -e normal -p osdk-core`（依赖总量）、`cargo tree --duplicates --workspace`（同一 crate 的多版本共存，目前 36 个）、`cargo tree -i <crate>@<version>`（反查是谁引入的）。当前 `reqwest` 同时存在 0.12 和 0.13 两个版本，来源是 sigstore 依赖链。
