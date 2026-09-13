# 006 — conda 元包的命令归属判定退化，`m2-base` 把 251 个 msys 命令灌进 PATH

**状态**：已修复（`fix/006-conda-metapackage-bin-ownership`） · **严重度**：高 · **实测环境**：Windows x64、osdk 0.0.2、`conda:m2-base@2022.6.1`

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

**把「归属已知且为空」错当成「归属未知」，从而落入为「未知」准备的退化分支。**

osdk 的安装期归属捕获机制本身是正确的：`materialize` 循环在解包到主包时调 `read_package_paths`，并把结果落成 prefix 内的 `.osdk-conda-paths.json`，因此不受解包顺序影响（初版记录曾推断为「后包覆盖前包」，实测证伪）。

问题出在对这份记录的解读。`m2-base` 是元包，实测数据：

| 项 | 值 |
| --- | --- |
| `info/paths.json` 总条目 | 31 |
| 其中 `Library/usr/bin/` 下的条目 | **3**（`cmd`、`shell`、`start`，均无可执行扩展名） |
| `Library/usr/bin` 实际文件数 | 365 |
| `.osdk-conda-paths.json` 记录数 | **0** |
| 实际发布命令数 | **251** |

那 3 个条目在 Windows 上被可执行扩展名过滤掉，于是归属集合为空。而代码在三处把「空」与「未知」等同：

| 位置 | 原写法 | 后果 |
| --- | --- | --- |
| `owned_bin_names` 开头 | `if owned.is_empty() { return None }` | 空记录直接退化 |
| `owned_bin_names` 结尾 | `if names.is_empty() { None } else { Some(names) }` | **实际生效的那处**：过滤后为空即退化 |
| `finalize_conda_install` | `.unwrap_or(true)` 消费上面的 `None` | 251 条全部写成 `owned: true` |

`None` 被下游理解为「无法确定归属」，触发注释里的退化分支：

> When that record is missing or empty the whole prefix is exposed instead: showing too much beats publishing nothing at all.

`conda-meta/` 目录不存在（实测 `Test-Path` 为 False），因此也没有 per-package 的元数据可回退。

### 为什么 `where --bins` 是权威现场

`where --bins` 走 `routed_bin_names_for_version`，对 `conda:` 分支读的是安装清单 `.osdk-install.json` 的 `bins` + `owned`，而不是实时调 `bin_names`。`DynamicToolBin::owned` 的 serde 默认值是 `true`（`skip_serializing_if = "is_owned"`，为真时不落盘），所以旧清单里 251 条全部没有 `owned` 字段 —— 反序列化后全部为 `true`，全部发布。**这也意味着仅升级二进制不足以纠正已装的 prefix，必须重装该 tool 才会重写清单。**

## 为何严重

「暴露过多胜过什么都不发布」对单包 CLI（`conda:ruff`）是合理取舍，**但对 msys 系包是有害的**：

- msys 版 `find` / `sort` / `link` 与 Windows 同名命令行为不同，覆盖后会让不相关的脚本和构建以难以定位的方式出错 —— 这正是 MSYS2 上游反复警告、也是当初为回避而放弃完整 MSYS2 安装的那个问题。绕道 conda 单包并没有躲开它。
- 影响面随包数增长。计划中的 `with` 选项（见 [docs/conda-with-option-spec.zh-CN.md](../conda-with-option-spec.zh-CN.md)）会让更多包进入同一 prefix，**只会放大这个退化**，因此这是 `with` 的前置阻塞项。

## 修复

引入 `enum Ownership { Known(Vec<String>), Unknown }`，把「已知」与「未知」在类型层面分开，不再用集合是否为空来推断：

- `materialize` 在匹配到主包时写 `Ownership::Known(...)` —— **见到包**即归属已知，即便它自己不装任何命令（这正是元包的常态）。
- `write_owned_paths` 对 `Known` 写数组（可以是空数组），对 `Unknown` **删除记录文件**：以「文件不存在」表达未知，同时清掉旧安装可能留下的陈旧记录。
- `owned_bin_names` 去掉两处 `is_empty() -> None`，只在记录文件缺失时返回 `None`。
- `bin_names` 在归属为空时发 `tracing::warn!`，说明这是元包并给出 `osdk config set shims.include` 的取回方式，避免「装完一个命令都没有」看起来像故障。

退化策略按既定取舍收紧为**发布空集 + 告警**，而非全量暴露。命令本身仍留在清单里（withheld 而非删除），`[shims] include` 可随时取回。

## 验证

修复前后在同一台机器、同一 prefix 上实测（`conda:m2-base@2022.6.1` 用新二进制重装以重写清单）：

| | published | withheld | 清单 `owned: false` |
| --- | --- | --- | --- |
| 修复前 | **251** | 1 | 0 / 251 |
| 修复后 | **0** | 252 | **251 / 251** |

`find` / `sort` / `ls` / `link` / `sh` / `bash` / `echo` / `test` / `which` 均已确认不在 published 集合中。

单测：`cargo test -p osdk-core --lib` 850 项全过（基线 847 + 新增 3）；`cargo clippy -p osdk-core --all-targets` 零警告。

**变异验证**：把 `owned_bin_names` 结尾退回 `if names.is_empty() { None } else { Some(names) }`，`an_empty_ownership_record_publishes_nothing_instead_of_the_whole_prefix` 与 `manifest_ownership_marks_a_metapackages_commands_as_unowned` 两个用例准确变红；恢复后已确认无探针残留。

## 回归防线

- `an_empty_ownership_record_publishes_nothing_instead_of_the_whole_prefix` —— 先在 prefix 里造出 `find`/`sort`/`ls`/`link` 等真实可执行文件（否则「返回空」会因为无文件可列而恰好成立，断言失去意义），再断言空记录返回 `Some([])` 而非退化。
- `manifest_ownership_marks_a_metapackages_commands_as_unowned` —— 驱动真实的 `owned_bin_names`（而非复制一份判定表达式），覆盖「无记录 / 空记录 / 有记录」三种状态下 `finalize_conda_install` 写入清单的归属标记。
- `unknown_ownership_clears_a_stale_record` —— 断言 `Unknown` 会清掉旧记录，陈旧数据不会被当成本次安装的答案。
- `conda:ruff` 这类单包 CLI 的现有行为不受影响（避免过度收紧）—— 由既有用例守住。

## 备注

本缺陷是在为 `with` 选项写规格、验证「命令暴露无需改动」这一假设时发现的。原本的假设是错的：现有机制在单包场景正确，但在元包场景静默退化。教训与 005 同类 —— **假设「现有实现已经正确」之前，先用真实数据核一遍**。

修复过程中还有两条教训：

- **单测通过不等于缺陷消失**。第一版改完后 `owned_bin_names` 层的用例已全绿，但 `osdk where --bins` 实测仍是 251 —— 真正生效的判定在函数结尾的第二处 `is_empty()`，以及消费它的安装清单。只有跑到用户实际看到的那一层（重装 + 读清单 + `where --bins`）才暴露出来。
- **带默认值的 serde 字段会让旧数据静默沿用旧语义**。`owned` 默认 `true` 且为真时不落盘，使得旧清单与「全部自有」在磁盘上无法区分，因此归属类修复必须重装才生效，不能只换二进制。
