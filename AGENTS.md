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

- 发布体积的基准线：`osdk` 约 11.95 MB、`osdk-shim` 约 3.5 MB（2026-09-12 于 Windows x64 实测）。改动如果让任一个二进制增长超过 10%，要么找出原因，要么在提交说明里讲清为什么这个代价值得付。**基准线会随功能增长而变，引用前先按下面的命令实测当前值，不要直接采信本文写下的数字。** 用独立的构建目录实测，**且必须分两次调用**（见下条，用 `--workspace` 一次构建量出来的 shim 体积是错的）：
  ```powershell
  $env:CARGO_TARGET_DIR="target\size-check"
  cargo build --release -p osdk-cli
  cargo build --release -p osdk-shim
  ```

- **shim 必须单独构建，不能和 CLI 放在一条 `--workspace` 命令里。** shim 以 `default-features = false` 依赖 `osdk-core`，从而不链接安装路径（7.4 MB → 3.5 MB）。但 Cargo 会在单次 `--workspace` 构建内统一 feature，把 `install` 重新打开，产物照样能跑、没有任何警告，体积却悄悄退回原样。`crates/osdk-shim/src/main.rs` 里有一条编译期断言专门拦这件事，它只作用于 release 构建，所以开发时 `cargo check/test/clippy --workspace` 不受影响。**遇到这条断言失败时，要改的是构建命令，不是删掉断言。**

- **`Backend` trait 上新增方法，代价会落到 shim 身上。** `Registry::new()` 会把全部 13 个 backend 实例化成 `Arc<dyn Backend>`，于是每个方法都进入 vtable，链接器无法证明它不可达，也就无法裁掉。这条曾让 shim 白背 5.15 MB（实测：同样 13 个 backend，`dyn` 分发 7.02 MB，静态分发 1.83 MB）。现在四个仅安装用到的方法（`list_remote_versions`、`resolve_version`、`install`、`uninstall`）已在 `install` feature 之后。**新增方法时先判断它属于哪一侧**：只有安装路径用得到的，要一并加上 `#[cfg(feature = "install")]`（trait 定义和每个 impl 都要加）；shim 也要用的，才放进无条件部分。

- **新增依赖前先判断它属于哪一侧。** 只有安装路径需要的重依赖（HTTP 栈、加密、签名校验、解压）应写成 `optional = true`，并在 `install = [...]` 里用 `dep:` 引入，这样它完全不进入 shim 的依赖图（sigstore 就是这么处理的：shim 的依赖图因此从 982 个 crate 降到 441 个，并去掉了第二份 `reqwest`）。直接加进默认依赖等于让 shim 也编译它。

- **关闭 `install` feature 时，安装路径留下的死代码是预期的。** `crates/osdk-core/src/lib.rs` 顶部用 `cfg_attr(not(feature = "install"), allow(...))` 收敛了这些告警（否则 shim 构建会有 254 条），默认构建仍保持全部 lint 强度，CI 也按默认 feature 跑 clippy。**不要为了消警告去动这段，也不要把它扩大成无条件的 `allow`。**

- 需要给安装路径的类型加 `#[cfg]` 时，注意 `GithubAttestation` 和 `VerificationEvidence` 采用的是「关闭时替换为无法构造的占位类型」这一手法：调用点都在 `if let Some(attestation) = attestation` 内，`Option<&Never>` 恒为 `None`，因此分支在编译期不可达，函数签名和公开字段都不必改。给这类类型加成员时，用方法而不是字段（占位类型可以有方法，不能有字段）—— `VerificationEvidence::digest` 就是为此从字段改成访问器的。

- **`[profile.release]` 里的 per-package `opt-level` override 是承重结构，不是装饰。** 整体使用 `opt-level = "z"` 换体积，但这会让 sha2 的可移植实现损失约 65% 吞吐（实测 2300 MiB/s 降到 800 MiB/s），而每个下载的归档都要做校验。把哈希相关的几个 crate 固定回 `opt-level = 3` 可以完全恢复速度，代价只有约 0.02 MB。**Cargo 对匹配不到任何包的 override 只发 warning、不报错**，所以依赖改名或手误会让这段保护静默失效；`crates/osdk-core/src/pipeline/verify.rs` 里的 `hashing_crates_are_pinned_to_a_fast_opt_level` 就是为此存在的，改动 profile 后不要绕过它。

- 调整 profile 时，用户真正在意的两个指标要分别测量，因为它们会朝相反方向变化：shim 的启动延迟（进程创建占主导，`opt-level` 影响很小）和归档校验吞吐（对 `opt-level` 极其敏感）。只测一个就下结论会得出错误的取舍。

- 依赖体积的排查手段：`cargo tree -e normal -p osdk-shim` 与 `-p osdk-cli`（分别看两个二进制的实际依赖图，2026-09-12 实测 427 / 1550 行）、`cargo tree --duplicates --workspace`（同一 crate 的多版本共存，实测 39 个；注意该命令按 crate 分块输出，数的是不同 crate 名而非行数）、`cargo tree -i <crate>@<version>`（反查是谁引入的）。注意按二进制分别查，两个二进制的图差别很大。

- **升级依赖时要跟着 sigstore 走，别自己钉版本。** `reqwest` 曾长期双版本（0.12 + 0.13）编译，原因不是某个 feature 多拉了一份，而是我们钉 0.12 而 sigstore 全家钉 0.13，且 `sigstore-rekor` / `sigstore-tsa` 对它是**非 optional** 依赖，任何 feature 组合都躲不掉。跟随上游升到 0.13 后单版本，顺带把 `ring` 也消掉了（此前 `ring` 与 `aws-lc-rs` 两个加密后端同时在编）。遇到重复依赖先用 `cargo tree -i` 看是谁引入，若是上游已整体前进，正确做法是跟随而不是钉住。

- **TLS 根证书来自操作系统信任库，不是内置副本。** reqwest 0.13 的 `rustls` feature 取代了 0.12 的 `rustls-tls`，同时把内置 webpki 根证书的 feature 全部删除。这是用户可见行为：企业 CA 自动生效，但精简容器缺 `ca-certificates` 时所有 HTTPS 下载都会失败。**测试套件基本离线，即使证书校验完全失效也会全绿**，所以改动 TLS 相关依赖后必须实测：既要确认正常 HTTPS 能通，也要确认 badssl.com 的 untrusted-root / expired / wrong-host / self-signed 四种坏证书都被拒绝且原因正确。

# 交互延迟

`osdk hook-env` 由 shell 钩子在**每个提示符**执行，`osdk-shim` 在**每次命令调用**时执行。这两条路径上的耗时会被用户逐次感知，所以它们和二进制体积一样是产品指标。以下几条都是踩过的坑。

- **改动 `hook-env`、`activate` 片段、shim 启动或 `inventory` 扫描后，必须跑 `cargo bench -p osdk-core` 并对照基准。** 基准在 `crates/osdk-core/benches/`，它测的是遍历量（目录数、stat 次数）而不只是墙钟时间——遍历量是确定性的，能在噪声环境里稳定暴露退化。它曾抓到「扫描下探进 conda prefix，一次多走 1,000 个目录」，以及「扫描走穿静态 backend 子树」。

- **不要在装了 osdk 钩子的 shell 里测 osdk。** 钩子挂在提示符上，而某些 shell（PowerShell 的 `PostCommandLookupAction` 是历史实现）会在**每次命令查找**时触发，于是每一次 `& osdk ...` 计时都额外包含一次完整激活。这条曾让 `hook-env` 被测成 1000 ms（真实值约 400 ms），并据此把根因误判成「单次太慢」，实际问题是触发频率。**基准和手工计时都必须 `pwsh -NoProfile`，并用 `Diagnostics.Process` 直接起进程，不经 shell 管道。**

- **基准必须断言退出码为 0 且输出非空。** 参数写错、环境变量被破坏时，osdk 会打印 usage 后以退出码 2 退出，耗时约 12 ms。这看起来像「快得惊人」，实际测的是报错路径。曾因辅助函数用 `$env` 作参数名（与 PowerShell 内建 `$env:` 驱动器同名）破坏了子进程环境，量出「12 ms、输出 0 字符」的假结果。

- **性能探针必须自证落在被测机制之内。** 两种形态都踩过。一是探针放在超出 `max_depth` 的深度，被深度上限先拦住，于是有没有剪枝断言都成立——三个变异全部存活，测试等于空的。二是**在基准里复刻一份被测判据**去数「走了多少目录」：它与产品代码脱钩，把 `inventory.rs` 的判据改成恒真，它照样输出同一个数字（37 → 37），变异一动不动地存活。后者更隐蔽，因为数字看起来很精确。

  正确做法是让失败信号来自被测代码自己的行为。例如埋一个 identity 与所在路径不符的 manifest 当诱饵：裁剪失效才会走到它，fail-closed 扫描随即报错。**判断标准是「这个断言的结果会不会随产品代码改变」，而不是「它看起来测得准不准」。**

- **深度上限不是可自由收窄的旋钮。** `ScanOptions::max_depth = 8` 是 `MAX_TOOL_ID_SEGMENTS(5) + version + install_id` 算出来的上界。收窄它在只装了 `conda:xxx`（2 段 id）的机器上能带来 8.9x 加速且测试全绿，但会让 `go:github.com/user/cmd/tool`（展开成 7 段，manifest 在深度 8）、`github:owner/repo`、`npm:@scope/pkg` 的安装**被漏扫**——扫不到的动态工具等于不存在。要减少遍历量，按 backend 裁剪子树，不要动深度。

- **新增动态 backend 或改 install 目录名时，新名字必须能从 `is_dynamic_install_directory`（`tool.rs`）到达。** 扫描据它决定进不进一棵子树，漏掉的名字会让那个 backend 的安装**对扫描完全隐形**——不报错，只表现为「装了却用不了」。所以 `namespace_schema` 是从 `DYNAMIC_NAMESPACES` 查的，不是另写一份 `match`；派生出来的目录名（如 npm 全局安装的 `npm-global`）走 `DERIVED_INSTALL_DIRECTORIES`。这个坑本来就已经踩了一个：`npm-global` 在第一版实现里被漏掉，是 `scoped_queries_filter_versions_before_selection` 抓出来的。

