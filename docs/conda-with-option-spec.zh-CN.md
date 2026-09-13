# `with`：把一组平级 conda 包装进同一个 prefix

**状态**：语义已定稿，待实现 · **动因**：[docs/bugs/005](./bugs/005-windows-posix-shim-recursion.zh-CN.md) 根因 B · **背景调研**：[docs/conda-backend-next-steps.zh-CN.md](./conda-backend-next-steps.zh-CN.md)

## 1. 要解决的问题

`conda:m2-bash`、`conda:m2-make`、`conda:m2-coreutils` 是**彼此平级**的包：它们之间没有依赖关系，因此现有的依赖闭包机制不会把它们放到一起。osdk 每个 `conda:<pkg>` 一个独立 prefix，于是这三个包各自解包、**各带一份 `msys-2.0.dll`**。

后果有二（实测见根因 B）：

- msys 进程按自己 DLL 所在位置推导 POSIX 根，所以 `m2-bash` 的 `sh.exe` 眼中 `/usr/bin` 只有 m2-bash 自己的文件 —— `tr` / `head` / `echo` 全部 `command not found`；
- 多份 `msys-2.0.dll`（内容相同但**是独立文件**，Windows 按路径而非内容做实例化）被载入同一进程树，属 msys 上游明确不支持的场景。

`with` 让用户显式声明「这些包必须住在一起」，从而在**一个** prefix 内解决。

## 2. 语义

### 2.1 一句话定义

`with` 在**主包所在的那一个 prefix** 内额外装入若干平级包。它**不创建** conda env，不改变「一个 tool 条目 ↔ 一个 install」这个基本关系。

### 2.2 配置形态

```toml
[tools]
"conda:m2-base" = { version = "2022.6.1", with = ["m2-make"] }
```

请求串形态（与现有 `channels` 一致的 `[k=v]` 语法）：

```
conda:m2-base[with='m2-make,m2-diffutils']@2022.6.1
```

### 2.3 精确语义

| 方面 | 规定 |
| --- | --- |
| **solve 输入** | 主包 spec 与每个 `with` 包 spec 一起作为 `SolverTask::specs` 提交，**一次求解**。这是 `with` 与「装两个 tool」的根本差别：同一次求解保证版本相容，且共享传递依赖只出现一次 |
| **`with` 项的版本** | 不接受版本约束，只写包名。求解器按主包版本与 channel 集自行选择相容版本 |
| **prefix 归属** | 只有一个 prefix，属于主包这个 tool 条目。`with` 包不是独立的 tool，不出现在 `osdk list`、不能被单独 `use` / `uninstall` |
| **命令暴露** | 规则不变：仍只发布主包自己的命令，`with` 包的命令与传递依赖同等对待、由 shim 层 withhold，可用 `[shims] include` 显式取回。**但现有实现对元包会退化成「发布整个 prefix」，必须先修，见 2.4** |
| **PATH** | 整个 prefix 的 bin 目录仍然构成该 tool 的 `bin_paths`，因此 `with` 包在**主包命令的执行环境内**可见。这正是 `make` 能看见 `tr` 的机制 |
| **identity** | `with` 的规范化值**进 identity**。理由见 2.5 |
| **顺序无关** | `with = ["b", "a"]` 与 `with = ["a", "b"]` 是同一个安装 |
| **不与自身重复** | `with` 不得包含主包自己；重复项去重 |

### 2.4 命令暴露：现有机制对元包会退化，这是 `with` 的前置阻塞项

设计意图上，`conda.rs` 的 `bin_names()` 通过 `owned_bin_names()` 读取 conda 在 `info/paths.json` 里的记录，只发布**主包 owned 的**命令；闭包内其余命令留在 manifest 中由 shim 层 withhold。注释写得很清楚：

> A conda prefix holds the whole dependency closure, so its `bin` directories contain far more than the requested package: `conda:clang` unpacks 16 packages and its `bin` ends up with `xmllint`, `zstd` and the ICU tools alongside the compiler.

**但对 `m2-base` 这类元包，这个机制实测失效了。** 本机实测 `osdk where --bins conda:m2-base`：

```
published (251): [, agetty, arch, ash, awk, ..., echo, ..., find, ..., link, ls, ..., sh, sort, ..., test, ..., tr, ..., which, who, ...
withheld (1): .m2-ca-certificates-post-link
```

**251 个命令被发布，只 withhold 了 1 个**。其中包含 `find`、`sort`、`ls`、`link`、`echo`、`test`、`which`、`tr` —— 全部会进 PATH 并盖住 Windows 同名命令。

原因在 prefix 里看得很清楚：

| 项 | 值 |
| --- | --- |
| `info/paths.json` 总条目 | **31** |
| 其中 `Library/usr/bin/` 下的条目 | **3**（`cmd`、`shell`、`start`） |
| `Library/usr/bin` 实际文件数 | **365** |
| 实际发布命令数 | **251** |

`info/paths.json` 记录的是**最后一个解包的包**（`m2-base` 元包自身，只带 3 个文件）留下的内容 —— 前面每个包的记录都被后一个覆盖了。注释里说这份记录是「captured during extraction before the next package overwrites it」，但对元包场景，被捕获的恰好是那个几乎不含文件的元包，于是 owned 集合小到无意义。而 3 条 owned 记录与 251 个已发布命令完全不匹配，说明实际走的是注释里描述的退化路径：

> When that record is missing or empty the whole prefix is exposed instead: showing too much beats publishing nothing at all.

「暴露过多胜过什么都不发布」这个取舍对单包 CLI 是合理的，**但对 `m2-*` 这类包是有害的**：它会用 msys 版 `find` / `sort` / `link` 覆盖 Windows 命令，正是当初为回避而放弃完整 MSYS2 的那个问题。

**因此这是 `with` 的前置阻塞项，不是可选优化。** `with` 会让更多包进入同一 prefix，只会放大这个退化。必须先修好命令归属判定：

- 正确做法是在**每个包解包时**分别捕获其 `info/paths.json` 并累积 owned 集合，而不是依赖 prefix 里最终残留的那一份；
- 或者从 solved records 拿到每个包的文件清单（rattler 的 `RepoDataRecord` 链路上有这个信息），不依赖解包顺序；
- 退化策略也应收紧：对 msys 系包，「暴露整个 prefix」的代价远高于「一个命令都不发布」，至少应改为发布空集并给出明确告警，而不是静默灌满 PATH。

修好之后，`with` 包在 owned 判定上与传递依赖同等对待，`bin_names` 无需为 `with` 增加任何特殊逻辑。

### 2.5 `with` 为何必须进 identity

`conda_install_locator` 已按 solved 闭包指纹化 install root，且 `conda_installed_locator` 在同一 `backend_id@version` 出现多个闭包时**直接报错**：

> conda install identity for `{backend_id}@{version}` is ambiguous across multiple solved closures; uninstall the unwanted prefix or pin the channels that produced the one you want

如果 `with` 不进 identity，会出现两种坏情况：

1. 改动 `with` 后闭包变了但 identity 未变 —— 要么不触发重装，要么新旧安装互相覆盖；
2. 同一 `m2-base@2022.6.1` 在「带 `with`」与「不带 `with`」两种配置下产生两个闭包，命中上面那条歧义错误，用户会收到一个与真实原因无关的报错（提示他去 pin channels）。

所以 `with` 与 `channels` 完全对称：`OptionEffect::Artifact` + `identity: true`。

### 2.6 与 `[shims] include` 的关系

`with` 决定「什么被装进同一个 prefix」；`[shims] include` 决定「哪些命令被发布到 PATH」。两者正交，**不要用 `with` 去表达「我想要 make 命令可直接调用」** —— 那是 `include` 的职责。

需要 `make` 在 shell 里直接可用时：

```toml
[tools]
"conda:m2-base" = { version = "2022.6.1", with = ["m2-make"] }

[shims]
include = ["make"]
```

## 3. 在哪些 backend 生效

**只在 `conda:` 生效。** 这不是保守，而是语义前提决定的。

### 3.1 判定依据

`with` 的语义依赖三个条件同时成立：

1. **有「prefix」这个共享安装根的概念** —— 多个包解包进同一个目录树并共享其 `bin` / `lib`；
2. **有能同时对多个平级 spec 求解的求解器** —— 保证版本相容；
3. **包之间有共享同一运行时或根目录的真实需求** —— 否则装在一起没有意义。

现有 backend 的对照：

| backend | 有共享 prefix | 多 spec 联合求解 | 结论 |
| --- | --- | --- | --- |
| `conda:` | ✅ prefix + `Library/usr` 子树 | ✅ resolvo，`SolverTask::specs` 已是 `Vec` | **支持** |
| `npm:` | ⚠️ `node_modules` 形似 | ⚠️ 由 npm/pnpm 自行解析 | 不支持，见 3.2 |
| `cargo:` / `go:` | ❌ 每个二进制独立安装 | ❌ | 不支持 |
| `github:` / `http:` | ❌ 就是一个压缩包 | ❌ 无求解器 | 不支持 |
| `node` / `python` / `java` / `golang` 等运行时 | ❌ 官方发行包 | ❌ | 不支持 |

### 3.2 为什么 npm 看似可行却不做

`npm:` 有 `node_modules` 这个共享树，表面上像 prefix。但：

- npm 的多包共装已由 `package.json` 表达，osdk 再提供一层 `with` 会与之语义重叠、来源竞争；
- npm 生态没有「必须共享同一份原生运行时」这个约束（这正是 msys 的特殊之处），`with` 要解决的问题在 npm 侧不存在；
- npm 侧真正的同类需求是 peer dependency，那属于包管理器职责。

若将来出现真实需求，应作为独立设计讨论，**不要因为「看起来能装在一起」就扩大 `with` 的适用面**。

### 3.3 实现层面的强制

`with` 注册在 `CONDA_OPTIONS` 中，因此其他 namespace 天然不接受它 —— 现有 schema 机制会把未知选项判为错误。**不需要额外的黑名单**，只需保证不把它加进别的 `*_OPTIONS`。

回归测试应显式断言：`npm:foo[with=bar]` / `cargo:foo[with=bar]` / `github:o/r[with=x]` 全部**解析失败**，且错误信息指出该选项不被该 backend 接受。

## 4. 校验规则

沿用 `canonical_conda_channels` 的形状（去重、上限、严格字符集），因为 `with` 的值同样是「一组 conda 包名」：

| 规则 | 值 | 理由 |
| --- | --- | --- |
| 字符集 | `[a-z0-9._-]`（conda 包名规范，小写归一） | 与 conda 包名一致；拒绝 `:` `@` `[` `]` `/` `\` 可防止把版本约束或另一个 backend id 塞进来 |
| 单项长度上限 | 128 | 同 `channels` |
| 数量上限 | 32 | 比 `channels` 的 8 宽松：一套 POSIX 工具集可能十几个包；但仍需上限以防误用成 env 清单 |
| 空值 | 拒绝，报「must name at least one package」 | 空 `with` 是配置错误而非「无操作」，静默忽略会掩盖笔误 |
| 含主包自身 | 拒绝并明确报错 | 与「不得重复」区分：这是概念错误，不是冗余 |
| 排序 | 规范化时排序 + 去重 | 保证 identity 稳定（`sqlite,netgo,sqlite` → `netgo,sqlite` 是既有先例） |

`validate_conda_options` 目前是空实现，主包自包含检查放在这里（它同时能看到 `id` 与 canonical 值）。

## 5. 实现要点

**顺序很重要**：第 0 步是阻塞项，不修它就上 `with` 会让 PATH 污染变得更严重。

0. **【前置】修正命令归属判定**（见 2.4）：按包累积 owned 集合，不依赖 prefix 中残留的最后一份 `info/paths.json`；收紧退化策略，对 msys 系包不再静默暴露整个 prefix。**这一步独立于 `with`，本身就是缺陷修复，应单独提交。**
1. **`tool.rs`**：`CONDA_OPTIONS` 增加 `with` 定义（`Artifact` / `identity: true` / `canonical_conda_with`）；实现 `canonical_conda_with`；在 `validate_conda_options` 中拒绝 `with` 含主包。
2. **`conda.rs::solve`**：把 `with` 包 spec 与主包 spec 一起放入 `SolverTask::specs`，并同样加入 `gateway.query` 的 spec 列表（否则 repodata 里没有它们的候选）。注意 `query(...).recursive(true)` 需覆盖全部 spec。
3. **`conda.rs::list_remote`**：**不受影响** —— 它只列主包版本，`with` 与「这个包有哪些版本」无关。
4. **`bin_paths`**：不改。`bin_names` 在第 0 步修好后无需为 `with` 增加特殊逻辑。
5. **`osdk use` CLI**：支持 `--with` 或直接透传 `[with=...]`，写入 `osdk.toml` 时保持规范化形态。

## 6. 回归防线

按本仓库的教训，必须区分「生成物长什么样」与「真的按预期工作」：

- **命令归属（对应第 0 步）**：`conda:m2-base` 安装后断言 `find` / `sort` / `ls` / `link` **不在** published 列表中。这是当前实测失败的用例，修好前它就该是红的。
- **规范化**：`with = ["b","a","b"]` → `a,b`；大小写归一；非法字符、超长、超量、空值、含主包各自报错。
- **identity**：同一 `version` 下 `with` 不同 → identity 不同；`with` 顺序不同 → identity 相同。**这条最关键**，直接对应 2.5 的两种坏情况。
- **backend 隔离**：`npm:` / `cargo:` / `go:` / `github:` / `http:` 带 `with` 一律解析失败。
- **solve 输入**：断言 `with` 包确实进入了提交给求解器的 spec 集合（而不是只被解析、未被使用）。这是纯字符串断言抓不到的一类缺陷。
- **端到端（必须在正常终端跑）**：`m2-base` + `with=["m2-make"]` 安装后，断言该 prefix 下 `msys-2.0.dll` **恰好一份**，且 `make` 能在 recipe 中调用到同 prefix 的 `tr` / `head`。

  当前执行环境无法回传 msys 子进程输出（退出码 0 但无副作用），此项**不能在该环境内验证**，需人工在真实终端确认并记录结果。

## 7. 明确不做

- 不做 conda env 管理：命名环境、`environment.yml` 导入导出、跨项目激活切换、env 内增删包。这会让 osdk 变成 conda 前端，与「按项目 pin 版本」的定位冲突，也是 mise 明确拒绝的方向。
- `with` 项不支持版本约束（第一版）。若放开，需同时定义它与主包版本冲突时的报错语义。
- 不支持跨 channel 组合：与 mise 一致（其文档明确 one channel per tool），`with` 包必须在同一 channel 集内可解。
