# 006 — conda 元包的命令归属判定退化，`m2-base` 把 251 个 msys 命令灌进 PATH

**状态**：待修复 · **严重度**：高 · **实测环境**：Windows x64、osdk 0.0.2、`conda:m2-base@2022.6.1`

## 现象

```
$ osdk where --bins conda:m2-base
published (251): [, agetty, arch, ash, awk, b2sum, ..., echo, ..., find, ..., link, ls,
                 ..., sh, sort, ..., test, ..., tr, ..., which, who, whoami, ...
withheld (1): .m2-ca-certificates-post-link
```

251 个命令被发布到 PATH，只 withhold 了 1 个（而且那 1 个是个 post-link 脚本）。发布集里包含 `find`、`sort`、`ls`、`link`、`echo`、`test`、`which`、`tr`、`sh` —— 这些会盖住 Windows 同名命令。

`conda.rs` 的 `bin_names` 设计意图恰恰相反，其文档注释明确说只发布请求的那个包自己的命令，并举了 `conda:clang` 不该把 `xmllint` / `zstd` 带进 PATH 的例子。

## 根因

`owned_bin_names()` 依赖 prefix 里的 `info/paths.json` 判断哪些文件属于主包。但这份文件在 prefix 根目录只有**一份**，每个包解包时都会覆盖它，所以最终留下的是**最后一个解包的包**的记录。

对 `m2-base` 这个元包，实测数据：

| 项 | 值 |
| --- | --- |
| `info/paths.json` 总条目 | 31 |
| 其中 `Library/usr/bin/` 下的条目 | **3**（`cmd`、`shell`、`start`） |
| `Library/usr/bin` 实际文件数 | 365 |
| 实际发布命令数 | **251** |

元包本身几乎不含文件（3 个），所以 owned 集合小到无意义。而 3 与 251 的巨大落差说明实际走的是注释中描述的退化分支：

> When that record is missing or empty the whole prefix is exposed instead: showing too much beats publishing nothing at all.

`conda-meta/` 目录不存在（实测 `Test-Path` 为 False），因此也没有 per-package 的元数据可回退。

## 为何严重

「暴露过多胜过什么都不发布」对单包 CLI（`conda:ruff`）是合理取舍，**但对 msys 系包是有害的**：

- msys 版 `find` / `sort` / `link` 与 Windows 同名命令行为不同，覆盖后会让不相关的脚本和构建以难以定位的方式出错 —— 这正是 MSYS2 上游反复警告、也是当初为回避而放弃完整 MSYS2 安装的那个问题。绕道 conda 单包并没有躲开它。
- 影响面随包数增长。计划中的 `with` 选项（见 [docs/conda-with-option-spec.zh-CN.md](../conda-with-option-spec.zh-CN.md)）会让更多包进入同一 prefix，**只会放大这个退化**，因此这是 `with` 的前置阻塞项。

## 修复方向

1. **按包累积 owned 集合**，不依赖 prefix 中残留的最后一份 `info/paths.json`：解包每个包时分别读取并记录归属，或从 solved records（rattler `RepoDataRecord` 链路上有文件清单）取得，彻底摆脱解包顺序的影响。
2. **收紧退化策略**：对 msys/cygwin 系包，「暴露整个 prefix」的代价远高于「一个都不发布」。退化时应发布空集并给出明确告警，让用户用 `[shims] include` 显式取回需要的命令，而不是静默灌满 PATH。

## 回归防线

- `conda:m2-base` 安装后断言 `find` / `sort` / `ls` / `link` **不在** published 列表中。**这个用例在修复前就应该是红的** —— 它现在是失败状态。
- 断言元包（`info/paths.json` 中 bin 条目数远小于实际 bin 文件数）触发的是「空集 + 告警」，而不是「全量暴露」。
- `conda:ruff` 这类单包 CLI 的现有行为不受影响（避免过度收紧）。

## 备注

本缺陷是在为 `with` 选项写规格、验证「命令暴露无需改动」这一假设时发现的。原本的假设是错的：现有机制在单包场景正确，但在元包场景静默退化。教训与 005 同类 —— **假设「现有实现已经正确」之前，先用真实数据核一遍**。
