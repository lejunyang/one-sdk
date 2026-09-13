# 005 — Windows 无扩展名 shim 用 `#!/bin/sh` 自举；`conda:m2-*` 每包一个 msys 根

**状态**：根因 A 已加固（递归未能在隔离环境复现，见「复现状态」）· 根因 C 已修复 · 根因 B 待处理 · **严重度**：严重（现场曾伴随内核崩溃） · **实测环境**：Windows x64、PowerShell 7、osdk 0.0.2、conda `m2-*`（msys2 运行时 3.6.1）

## 复现状态（先读这一节）

本文档的三个根因证据强度**不同**，不要混为一谈：

- **根因 B、C 已在本机确定性复现**，证据见下文，且 C 已有变异验证过的回归测试。
- **根因 A 的递归链条是代码审查得出的推断，尚未在隔离环境复现。** 现场确实观测到进程暴涨与系统假死（且当天 3 次蓝屏都发生在 conda `m2-*` 构建期间），但后续在隔离环境用「复刻的旧版 `#!/bin/sh` wrapper + `SHELL := /bin/sh` 的 Makefile + shims 置于 PATH 最前」多种组合尝试，**均未触发递归**：make 正常退出 0，recipe 正常执行，无残留进程。

  这意味着还有一个未识别的触发条件。已排除的假设：
  - 不是 shim 名字占用 `sh` / `bash` 本身 —— 移走这两个 shim 后现场故障仍出现，只放这两个 shim 又不触发；
  - 不是 shim 数量 —— 逐个 shim 单独测试、以及整个 shims 目录测试，都不触发；
  - 不是 `SHELL := /bin/sh` 单独所致 —— 旧实现下同样的 Makefile 正常通过。

  一次中间测量曾读到 185～243 个进程，后确认是**前一次未清理的残留在累积**，不是当次新增，已作废。凡涉及进程计数的验证必须先断言基线为 0。

  **因此根因 A 的修复应理解为「消除了一条已证存在的危险自举结构」，而不是「已证明修好了那次蓝屏」。** 真正的触发条件仍需在能安全崩溃的一次性环境（快照虚拟机）里，配合 minidump / bugcheck code 定位。

## 现象

在一个从 macOS 迁来的项目里（`Makefile` 首行为 `SHELL := /bin/sh`），用 osdk 管理的 `conda:m2-make` + `conda:m2-bash` 执行 `make <target>`：

- 任何带 recipe 的目标都挂住，**连 `@echo HELLO` 都不返回**，而 `make -n`（只解析不执行）完全正常；
- CPU 与句柄数在数秒内被吃满，**任务管理器都无法打开**；
- 同一天内触发 **3 次蓝屏**，每次都发生在 conda `m2-*` 相关的构建过程中。

初期有三个误导性观察，值得记下来：

- `make --version`、`make -n` 都正常，所以「make 装坏了」这个方向是错的；
- 直接跑 `sh.exe -c "echo ok"` 也挂住，一度以为是执行环境不支持 msys；后经对照发现 Git-Bash 的 `bash.exe` 在同一环境下同样无法回传输出，**这一条属于当时执行环境无法捕获 msys 子进程输出的假象，不是故障证据**；
- 报错文本是 `make: go: No such file or directory`，看起来像 PATH 没配好，掩盖了根因 B。

## 根因

### 根因 A：无扩展名 shim 是一个需要 `/bin/sh` 才能解释的 `#!/bin/sh` 脚本

`crates/osdk-core/src/shim/mod.rs` 的 Windows 分支为**每一个**工具名同时写两份 shim：

```rust
#[cfg(windows)]
fn generate_shim_in(shims: &Path, name: &str, osdk_shim_bin: &Path) -> Result<()> {
    create_dir_all(shims)?;
    // .cmd wrapper for cmd.exe / PowerShell
    let cmd_path = shims.join(format!("{name}.cmd"));
    let cmd = windows_cmd_wrapper_bytes(osdk_shim_bin);
    std::fs::write(&cmd_path, cmd).map_err(|e| Error::io(&cmd_path, e))?;

    // extension-less bash wrapper for Git-Bash / MSYS
    let sh_path = shims.join(name);
    let sh = format!(
        "#!/bin/sh\nexec \"{}\" \"$(basename \"$0\")\" \"$@\"\n",
        osdk_shim_bin.display().to_string().replace('\\', "/")
    );
    std::fs::write(&sh_path, sh).map_err(|e| Error::io(&sh_path, e))?;
    Ok(())
}
```

这份无扩展名 wrapper 本身就是**一个需要 `/bin/sh` 来解释的 shell 脚本**。对绝大多数工具名（`node`、`go`、`java`）没有问题，但当 `name` 恰好是 `sh` 或 `bash` 时，它变成了「解释器的 shim 需要解释器自己」：

```
make（SHELL := /bin/sh）
  → 解析 /bin/sh，PATH 命中 shims\sh（一个 #!/bin/sh 脚本）
    → 需要 /bin/sh 来解释它，PATH 再次命中 shims\sh
      → …… 递归
```

`conda:m2-bash` 提供的 bin 名里就包含 `sh` 和 `bash`，所以只要装了它，`shims\sh` 与 `shims\bash` 必然被生成。

**关于后果的推断（未经复现验证）**：若递归成立，每一层都是**真实的 msys 进程**而非轻量函数调用。msys 运行时要模拟 `fork`、维护 POSIX 信号与 pty 层，进程创建代价远高于原生进程，且每层都会映射一份运行时共享内存区，因此是指数级增殖而非有界递归。这与现场观测到的「进程暴涨、GUI 完全失去响应」一致，但**如「复现状态」一节所述，该链条未能在隔离环境重现**，所以此处只作为机制假设保留。

**无论递归是否为那次蓝屏的成因，这个结构本身都必须消除**：一个其解释器需要经 PATH 解析、而自己又能被解析为该解释器的脚本，是无条件的危险自举，没有任何理由保留。

### 根因 B：`conda:m2-*` 每包一个 msys 根，且各带一份 `msys-2.0.dll`

`crates/osdk-core/src/backend/conda.rs` 的 `conda_bin_dirs()` 把每个 conda 安装 prefix 各自的 `Library\usr\bin` 作为 bin 目录发布。conda 的 msys2 移植包（`m2-bash`、`m2-make`、`m2-coreutils`……）每个都是**独立完整的 prefix**，各自携带一份 `msys-2.0.dll`。本机实测：

| 包 | `msys-2.0.dll` | 大小 | 版本 | SHA256（前 12） |
| --- | --- | --- | --- | --- |
| `conda:m2-bash` | 有 | 3269522 | 3.6.1-28176974 | `2813D00F34D6` |
| `conda:m2-coreutils` | 有 | 3269522 | 3.6.1-28176974 | `2813D00F34D6` |
| `conda:m2-make` | 有 | 3269522 | 3.6.1-28176974 | `2813D00F34D6` |
| `conda:m2-grep` | 有 | 3269522 | 3.6.1-28176974 | `2813D00F34D6` |
| `conda:m2-gawk` | 有 | 3269522 | 3.6.1-28176974 | `2813D00F34D6` |
| `conda:m2-sed` | 有 | 3269522 | 3.6.1-28176974 | `2813D00F34D6` |
| `conda:m2-findutils` | 有 | 3269522 | 3.6.1-28176974 | `2813D00F34D6` |
| `conda:m2-diffutils` | 有 | 3269522 | 3.6.1-28176974 | `2813D00F34D6` |

8 个根、8 份内容完全相同的 DLL（`DISTINCT_DLL_COPIES=1`，即同版本同哈希）。这带来两个独立的坏后果：

**B1 — msys 的 `/usr/bin` 只映射到单一根，导致工具互相看不见。** msys 进程按自己 DLL 所在位置推导 POSIX 根。`sh.exe` 来自 `m2-bash` 的 prefix，于是它的 `/usr/bin` 只有 m2-bash 自己那些文件。实测在 make recipe 内：

```
$ sh -c 'command -v go'          → NO_GO
$ make ... @echo ...             → make: echo: No such file or directory
$ sh -c '... | tr ... | head'    → /bin/sh: tr: command not found
                                   /bin/sh: head: command not found
```

连 coreutils 的 `echo` / `tr` / `head` 都找不到，因为它们在 `m2-coreutils` 的另一个根里。这解释了那个误导性的 `make: go: No such file or directory`。

**B2 — 多份 msys 运行时混载进同一进程树。** 一条 recipe 里 `make.exe`（m2-make 根）启动 `sh.exe`（m2-bash 根）再调 `tr.exe`（m2-coreutils 根），三个不同路径的 `msys-2.0.dll` 被载入同一进程树。msys 的 fork 模拟依赖**单一运行时实例的全局共享内存区**，上游明确要求整个进程树只能有一份运行时。混载在正常负载下可能只是偶发异常，但叠加根因 A 的成千上万个进程后，共享区争用升级为内核级句柄/内存冲突。

**这就是三次蓝屏都指向 conda `m2-*` 构建的原因**：A 提供数量，B2 提供内核态的破坏面。单独看任何一个都不足以解释蓝屏。

### 根因 C：`hook-env` 的 POSIX 分支用 `:` 拼接 Windows 原生路径

`crates/osdk-core/src/activate/mod.rs` 的 `render_path_reset()` 在非 Fish / 非 PowerShell 分支（即 bash / zsh）里这样拼 PATH：

```rust
let joined = paths
    .iter()
    .map(|path| path.display().to_string())
    .collect::<Vec<_>>()
    .join(":");
out.push_str(&format!(
    "export PATH={}:\"$OSDK_ORIGINAL_PATH\"\n",
    shell_quote(shell, &joined)
));
```

在 Windows 上 `path.display()` 产出 `E:\osdk-data\data\shims`，用 `:` 连接后得到 `E:\a:E:\b`。POSIX shell 会把它切成 `E`、`\a`、`E`、`\b` 四个无意义条目 —— **PATH 直接损坏**。

因此 `osdk activate bash` 在 Windows（Git Bash / MSYS）上目前是不可用的：命令本身不报错，但注入的 PATH 无效。这正是用户提问「Windows 上能不能用 `osdk activate bash` 注入工具」的答案：机制存在、平台无关，但这个路径分隔缺陷让它在 Windows 上静默失效。

## 实测证据

根因 B1 的确定性证据（同一台机器、同一批 pin）：

| 动作 | 结果 |
| --- | --- |
| `make --version` | 正常，GNU Make 4.4.1，`Built for x86_64-pc-msys` |
| `make -n protocol-smoke` | 正常输出 `cd protocol && node scripts/validate.mjs` |
| make recipe 内 `@echo` | `make: echo: No such file or directory` |
| make recipe 内 `sh -c 'command -v go'` | `NO_GO` |
| make recipe 内 `tr` / `head` | `/bin/sh: tr: command not found`、`head: command not found` |
| 8 个 msys 根全部前置进 PATH 后同一 recipe | `go / tr / head / awk / grep / sed` 全部解析成功 |
| 8 个根前置后 `make protocol-smoke` | **通过**：`14 schemas, 13 valid fixtures, 35 invalid fixtures, 4 compatibility checks (52 total)` |

最后两行证明根因 B1 独立存在，且「聚合所有 msys 根」确实能让 recipe 跑通 —— 但它**不解决 B2**，多运行时仍在混载，所以不能作为最终方案。

修复后的验证（隔离 `OSDK_DATA_DIR`，未触碰用户真实 shims）：

| 动作 | 结果 |
| --- | --- |
| 隔离环境 `osdk install node@20.11.1` 后检查 shims | `node` / `npm` / `npx` 均为 `MZ` 开头的 PE，不再是 `#!/bin/sh` 脚本 |
| `osdk activate bash` | 正常输出 bash 集成片段，退出码 0 |
| `osdk hook-env --shell bash` | PATH 行输出 msys 形式；`CARGO_HOME` 等仍为原生路径（**这是正确的**，见下） |
| 放入 `sh` / `bash` shim 并置于 PATH 最前，跑 `SHELL := /bin/sh` 的 Makefile | make 退出码 0、recipe 真实执行（产出文件内容符合预期）、残留进程 0 |
| 同上但换成复刻的旧版 `#!/bin/sh` wrapper | **同样通过**，未触发递归 —— 故此对照不能作为「修复有效」的证据，见「复现状态」 |
| `cargo test --workspace` | osdk-core 847 项全过；osdk-cli 有 2 项在无 tty 环境下超时，已确认在**未含本次改动的基线上同样失败**，与本修复无关 |
| `cargo clippy --workspace --all-targets` | 零警告 |
| release 体积 | `osdk.exe` 11.97 MB、`osdk-shim.exe` 3.50 MB，与基准持平（分两次调用构建） |

**`$PATH` 转换而环境变量不转换是刻意的**：`$PATH` 由 shell 自己解析，必须用 shell 的方言；`CARGO_HOME` / `GOCACHE` / `JAVA_HOME` 由原生 Windows 可执行文件（`cargo.exe`、`go.exe`、JVM launcher）读取，它们只认 `C:\dir`。一并转换会反而弄坏这些工具。

蓝屏 3 次，均发生在 conda `m2-*` 构建期间；因当时系统假死无法采集 minidump，**故不声称已定位到具体 bugcheck code，也不声称本次修复消除了它**。

## 影响

- **严重（成因未完全确认）**：现场故障是任何 Windows 用户在通过 osdk 安装了 msys 系包（`conda:m2-*`）后执行 `make` 时，进程暴涨至系统假死，当天伴随 3 次蓝屏，**存在丢失未保存工作的实际风险**。根因 A 的自举结构是其中一条已确证存在的危险路径并已消除，但触发条件尚未完整定位。
- **中**：`osdk activate bash` / `zsh` 在 Windows 上注入的 PATH 恒为损坏值（根因 C，已修复），使 Git Bash 这条本可跨平台复用的通道此前不可用。
- **中**：`conda:m2-*` 组合出的 POSIX 环境天生残缺（根因 B1，待处理），用户即使避开递归，也会遇到「装了却互相看不见」。

## 修复

已完成两处，各自独立提交：

1. **`882198a` — 无扩展名 shim 不再是 shell 脚本**（根因 A）。改为放置 `osdk-shim.exe` 的无扩展名副本：优先硬链接以免每个工具各占一份完整二进制，链接不可用时（跨卷、不支持硬链接的文件系统）回退字节复制。msys 可直接 exec PE 映像，且 `osdk-shim` 本就按 argv[0] 推导工具名，与 Unix 侧的 symlink 完全同构，无需新增识别逻辑。清理逻辑同时识别 PE 副本与旧的 `#!/bin/sh` wrapper，使从旧版升级的机器不会永久留下危险 shim。

   **这一处是结构性加固，不等于已修复那次蓝屏** —— 见「复现状态」。

2. **`7606f19` — `hook-env` 的 POSIX 分支在 Windows 输出 msys 路径**（根因 C），使 `osdk activate bash` 在 Windows 上真正可用。刻意只转换 `$PATH`，不转换受管环境变量，理由见「实测证据」末尾。

待处理：

3. **根因 B（多 msys 根 / 多运行时混载）尚未处理。** 这不是 osdk 能在 PATH 层面消除的：`conda:m2-*` 每个包就是一个独立 prefix，各带一份 `msys-2.0.dll`。已完成方案调研，见 [docs/conda-backend-next-steps.zh-CN.md](../conda-backend-next-steps.zh-CN.md) —— 结论是不引入 conda env 管理，而是给 `[tools]` 增加 `with` 选项让一组平级包共装同一 prefix，并补上 `osdk doctor` 对多份 `msys-2.0.dll` 的检测。

4. **递归的真实触发条件仍未定位。** 需要在可安全崩溃的一次性环境（快照虚拟机）里复现并采集 minidump / bugcheck code。

## 回归防线

已落地（均经变异验证 —— 把产品代码退回旧行为后确认变红）：

- `windows_posix_launcher_is_never_a_shell_script`（`shim/mod.rs`）— 断言 Windows 生成的无扩展名 shim 不以 `#!` 开头、以 `MZ` 开头、且与 launcher 二进制逐字节一致。**特意对 `sh` 和 `bash` 两个名字断言**：其余工具名都能容忍旧实现，这正是缺陷长期存活的原因。变异：把生成逻辑换回 `#!/bin/sh` wrapper → 变红。
- `cleanup_removes_legacy_shell_wrappers_but_preserves_user_files`（`shim/mod.rs`）— 断言旧 wrapper 会被清理、用户自己写的同名脚本不会。此测试对生成逻辑的变异不敏感（它只测清理），这是预期的。
- `windows_posix_shells_receive_msys_paths_not_drive_letters`（`activate/mod.rs`）— 用**原生 Windows 路径**断言 bash / zsh 的 PATH 行。既有的 `hook_env_renders_path_and_vars` 抓不到根因 C，因为它喂进去的本就是 POSIX 路径、在任何平台都原样通过。变异：把转换退回 pass-through → 变红。
- `posix_path_conversion_only_rewrites_drive_letter_paths`（`activate/mod.rs`）— 断言 UNC 路径与已是 POSIX 形式的路径不被破坏。变异同上 → 变红。

仍缺（**这是本条最重要的缺口**）：

- **一个能真正抓到递归的端到端测试。** 递归的表现是「不返回」，任何检查生成物内容的断言都会全绿；而本次尝试构造的端到端场景**连旧实现都没能触发**，说明触发条件尚未被理解，因此目前写不出有效断言。在触发条件定位之前，这个缺口是敞开的 —— 上面四条静态断言只能保证「不会再生成那种自举结构」。
- `osdk doctor` 对多份 `msys-2.0.dll` 的检测（根因 B）。

## 教训

- **进程计数类验证必须先断言基线为 0。** 本次一度读到 185～243 个进程并据此认定「修复无效」，实际是前一次未清理的残留在累加。这与本目录 README 记录的「测性能时先确认测的是不是自己」是同一类错误的不同形态。
- **「在我这跑不出来」不等于「已经修好」。** 现场故障与隔离环境的差异本身就是待查线索，不能因为构造的场景通过了就把状态标成已修复。
