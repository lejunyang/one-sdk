# conda backend 的下一步：从「每包一 prefix」到「按需共装一 prefix」

**状态**：方案待评审 · **关联缺陷**：[docs/bugs/005](./bugs/005-windows-posix-shim-recursion.zh-CN.md) 根因 B · **调研日期**：2026-09-13

## 一句话结论

osdk 当前的 `conda:<pkg>` 每包一个独立 prefix，与 mise 的做法一致、在 Unix 上没有问题；但对 **msys2 系包（`m2-*`）在 Windows 上是错的**，因为这类包必须共享同一个 POSIX 根和同一份 `msys-2.0.dll`。解法不是引入 conda env 管理，而是让**一个 tool 条目可以声明「和哪些包装进同一个 prefix」**，用现有 `[tools]` 的结构化选项即可表达，无需新增顶层概念。

## 一、mise 是怎么处理 conda 依赖的

结论：**mise 和 osdk 现在同构，都是每个 tool 一个隔离 prefix。**

mise 官方 conda backend 文档明确写了三点["https://mise.jdx.dev/dev-tools/backends/conda.html"]：

1. **自己解依赖、直接下载**，不需要用户装 conda / mamba / micromamba。「It solves dependencies and downloads packages directly, so conda, mamba, and micromamba do not need to be installed.」——osdk 的做法相同。
2. **每个 tool 一个隔离 prefix，并且注入 `CONDA_PREFIX`**。「Commands from the selected package run inside that package's isolated conda prefix. mise sets `CONDA_PREFIX`, makes the prefix's executable directories available to the command process, and applies `etc/conda/activate.d` scripts before starting it.」
3. **明确不做通用 env**。Limitations 一节直说：「mise solves and installs transitive dependencies in an isolated prefix for each tool. **It does not import or maintain a general-purpose `environment.yml`.**」并且「Only commands belonging to the requested package are exposed to your shell. Dependency executables remain available inside that tool's launcher environment.」

配置形态上，mise 用 `[tools]` 里的结构化选项承载 backend 专属参数，例如指定 channel：

```toml
[tools]
"conda:my-tool" = { version = "latest", channel = "my-team" }
```

**两点值得 osdk 借鉴**：

- **`CONDA_PREFIX` 与 `activate.d`**：mise 会注入 `CONDA_PREFIX` 并执行包自带的 `etc/conda/activate.d` 激活脚本。osdk 目前没有这一步，而 conda-forge 约定这些脚本就放在 prefix 的 `etc/conda/`（Windows 上同样是这个路径，不是 `Library/etc/`）["https://conda-forge.org/docs/maintainer/understanding_conda_forge/directory-structure/"]。缺这一步会让部分包在需要环境变量时行为不完整。
- **「依赖的可执行文件不进用户 PATH，只在该 tool 的启动环境里可见」**：这个隔离取向是对的，应保留。

**但 mise 的做法并未解决 osdk 眼下的问题**：mise 文档讨论的是「一个 CLI 工具及其传递依赖」，而 `m2-bash` / `m2-make` / `m2-coreutils` 是**彼此平级、必须共存于同一 POSIX 根**的一组包，不构成传递依赖关系。mise 没有针对这个场景的答案，因为它的 conda backend 主要面向 `ruff` 这类自包含 CLI。

## 二、conda 自身是怎么处理的

结论：**conda 的单位是 env（= 一个 prefix），`conda install a b c` 把多个包解包进同一个 prefix。**

- Windows 上一个 conda env 的目录是混合布局：`Library/` 放常规 conda-forge 包，**`Library/usr/` 整棵子树专供 MSYS2 包使用**["https://conda-forge.org/docs/maintainer/understanding_conda_forge/directory-structure/"]。所以 `m2-bash`、`m2-make`、`m2-coreutils` 装进同一个 env 后，它们的 `sh.exe` / `make.exe` / `tr.exe` 天然落在**同一个 `Library/usr/bin`**，共享**唯一一份 `msys-2.0.dll`**。
- `m2-base` 就是官方为此提供的聚合元包（`repo.anaconda.com/pkgs/msys2` 的 `m2-base`）["https://repo.anaconda.com/pkgs/msys2/"]，一次拉齐一套 POSIX 基础工具。

### 本机实测对照

```
conda:m2-base@2022.6.1（单一 prefix）
  Library/usr/bin           365 个文件
  msys-2.0.dll              1 份
  sh bash echo tr head awk gawk grep sed find printf cat uname   全部齐备
  make                      缺失（m2-base 不含 make）

8 个拼装包（m2-bash / m2-coreutils / m2-make / m2-grep / m2-gawk / m2-sed / m2-findutils / m2-diffutils）
  msys-2.0.dll              8 份，SHA256 全为 2813D00F34D6（内容完全相同）
  LinkType                  False —— 是 8 个独立文件副本，不是硬链接
```

**「内容相同但是独立文件」是这里的要害**：Windows 按**文件路径**而非内容做 DLL 实例化，所以 8 份同哈希副本被载入同一进程树时，就是 8 个独立的 msys 运行时实例，各自持有自己的共享内存区与 fork 模拟状态。这正是 msys2 上游明确警告的场景 —— 官方 wiki 直言「mixing in programs from other MSYS2 installations, Cygwin installations, compiler toolchains or even various other programs **is not supported and will probably break things in unexpected ways**」["https://www.msys2.org/wiki/MSYS2-introduction/"]。

同时它也解释了那个误导性报错：msys 进程按**自己 DLL 所在位置**推导 POSIX 根，所以 `m2-bash` 的 `sh.exe` 眼中 `/usr/bin` 只有 m2-bash 自己的文件，`tr` / `head` / `echo` 全部 `command not found`。

## 三、如果 osdk 来处理：要做 env 管理吗？

**不需要引入 env 概念，但需要「共装同一 prefix」的能力。** 两者的区别很重要：

- **conda env 管理**（不做）：命名环境、`environment.yml` 导入导出、跨项目激活切换、env 内增删包。这会把 osdk 变成 conda 前端，与 osdk「按项目 pin 版本」的定位冲突，也是 mise 明确拒绝的方向。
- **共装同一 prefix**（要做）：一个 tool 条目声明它需要哪些额外包一起解包进**它自己那个** prefix。这仍然是「一个 tool 一个 prefix」，只是 prefix 的内容从「一个包 + 传递依赖」扩展为「一组显式声明的平级包 + 各自的传递依赖」。solve 与 install 的输入变了，**生命周期、identity、清理逻辑都不变**。

### 能写进 `[tools]` 吗？能，而且不需要改配置模型

`config/mod.rs` 里已有现成的扩展点：

```rust
pub enum ToolConfigEntry {
    Legacy(String),
    Structured(StructuredToolConfig),   // { version, options }
}

pub enum ToolConfigValue {
    String(String),
    Bool(bool),
    Array(Vec<String>),                 // ← 数组已经支持
}
```

`options: BTreeMap<String, ToolConfigValue>` 是通用的 backend 专属参数袋，且 `ToolConfigValue::Array` 已存在。所以下面的形态**不需要新增任何配置类型**：

```toml
[tools]
# 一个条目 = 一个 prefix，内含一整套共享同一 msys 根的 POSIX 工具
"conda:m2-base" = { version = "2022.6.1", with = ["m2-make"] }
```

语义：解一次依赖，把 `m2-base` 与 `m2-make` 及其传递依赖**解包进同一个 prefix**，于是 `Library/usr/bin` 下只有一份 `msys-2.0.dll`，`sh` 能看见 `make`，`make` 能看见 `tr`。

命名建议 `with`（而非 `packages` / `extra`）：它读起来就是「和谁装在一起」，且不暗示这是一个 env。

### 需要连带处理的三件事

1. **identity 与 solve 缓存必须纳入 `with`**。`conda.rs` 现在按 `backend_id@version` 的 solve 闭包算 identity，并且在闭包歧义时明确报错（「conda install identity for `...` is ambiguous across multiple solved closures」）。`with` 改变了闭包内容，必须进 identity 的哈希输入，否则改了 `with` 不会触发重装，或者两个不同 `with` 的安装互相覆盖。
2. **只暴露主包的命令**。沿用 mise 的取向：`with` 里的包是为了让主包在**自己的 prefix 内**能工作，它们的可执行文件不应进用户 shell 的 PATH，否则 `tr` / `head` 这些会污染全局命令空间。这一点对 `m2-*` 尤其重要 —— 你不希望 `find` 变成 msys 版。
3. **补上 `CONDA_PREFIX` 与 `etc/conda/activate.d`**。这是独立于 `with` 的既有缺口，见第一节。

## 四、「装完整 MSYS2 会影响 osdk 现有功能」这个担忧还成立吗

**部分成立，但它担心的是全局 PATH 污染，不是 MSYS2 本身。** 拆开看：

**成立的部分**：MSYS2 上游要求它自己管理环境，启动器会**主动裁剪 PATH**，只留 `C:\Windows\System32` 等少数项，并由 `MSYS2_PATH_TYPE` 控制["https://www.msys2.org/wiki/MSYS2-introduction/"]。如果把 `C:\msys64\usr\bin` 塞进全局 PATH，msys 版的 `find` / `sort` / `link` 会盖住 Windows 同名命令，实践中被反复警告会造成「调用到 MSYS 版工具而非预期版本」的诡异冲突["https://blog.csdn.net/2301_79692223/article/details/154347519"]。这个顾虑是真实的。

**不成立的部分 —— 也是当时选择的失误**：为了回避 PATH 污染而改用 conda 单包拼装，**恰好落进了 MSYS2 上游同一条警告的另一半**：混装多个 msys 运行时。换句话说，那次选择没有消除风险，只是把「PATH 污染」换成了「多运行时混载」，而后者的后果更严重（前者是找错命令，后者可能是内核级冲突）。

**并且 osdk 本来就有对付 PATH 污染的机制**：osdk 是通过 shims 目录 + `hook-env` 注入的，只有被 pin 的工具会出现在 PATH 上。一个通过 osdk 管理的 MSYS2 根，其 `usr/bin` **不需要**进全局 PATH —— 只需要在 make/recipe 的执行环境里可见即可，这正是上面第 3 节「只暴露主包命令」要保证的事。

所以三条路可选，推荐第 1 条：

| 方案 | msys 根数 | 说明 |
| --- | --- | --- |
| **1. `m2-base` + `with = ["m2-make"]`**（推荐） | **1** | 需要实现 `with`。完全在 osdk 管理下，无全局 PATH 污染，单一运行时 |
| 2. `m2-base` + 独立 `m2-make` | 2 | 今天就能用，风险从 8 降到 2，但仍是混载 |
| 3. 完整 MSYS2 / Git Bash 提供全套 | 1 | 单根最彻底，但 MSYS2 自带 pacman 会自管依赖，与 osdk 的 pin 模型重叠；且需要人工装 |

方案 3 的「自己管理依赖」冲突是真实的，但它不是安全问题而是**职责重叠**：pacman 更新会绕过 osdk 的 pin，破坏可重现性。这也是不推荐它的主因。

## 五、建议的落地顺序

1. **短期（今天可做）**：项目 `osdk.toml` 里把 8 个 `m2-*` 收敛为 `conda:m2-base` + `conda:m2-make`，msys 根数 8 → 2。同时考虑把这两条移出项目 pin、改为本机全局 pin —— macOS 同事自带 `/bin/sh`，pin `m2-*` 对他们是纯负担。
2. **中期**：实现 `[tools]` 的 `with` 选项（含 identity 纳入、只暴露主包命令），msys 根数 2 → 1。这是根因 B 的正解。
3. **并行补缺**：注入 `CONDA_PREFIX`、执行 `etc/conda/activate.d`，与 mise 对齐。
4. **诊断兜底**：`osdk doctor` 增加「PATH 上存在多个不同路径的 `msys-2.0.dll`」检测 —— 即使 `with` 落地，用户仍可能手工混入其他 msys 安装。

## 六、待确认

- `m2-base@2022.6.1` 与 `m2-base@1.0.0` 的工具集差异未逐一比对，选定版本前应确认所需命令齐备。
- `with` 是否需要支持跨 channel（mise 明确「one channel per tool」，其 solver 不支持多 channel 组合）。建议第一版同样限制为单 channel，与 mise 保持一致。
- 本次调研中，msys 子进程在当前执行环境下无法回传 stdout（多次实测退出码为 0 但无任何副作用），因此 `m2-base` 的**端到端 recipe 执行未能验证**，只验证了文件级齐备性与 DLL 单份。落地前需在正常终端里实跑一次 `make`。
