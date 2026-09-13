# 001 命令路由表未按 `owned` 过滤，依赖闭包制造假冲突

日期：2026-09-13

严重度：**高** — 34 个常用命令无法通过 shim 调用，且用户按错误提示做的配置无效。

## 现象

在一个声明了 11 个 `conda:` 工具的项目里，直接调用 `bash` 被拒绝：

```
osdk-shim: refusing to route `bash` because multiple installed tools provide it:
conda:m2-bash, conda:m2-diffutils, conda:m2-gawk, conda:m2-grep, conda:m2-make, conda:m2-sed
```

三个反常之处，正是它们暴露了这不是配置问题：

1. 只有 `conda:m2-bash` 是为了 bash 装的，其余五个分别是为 sed、grep、gawk、make、diffutils 装的。
2. 磁盘上**并没有**重复的 bash shim——`osdk where conda:m2-sed --bins` 明确显示 bash 处于 `withheld` 列表。
3. 按提示配 `shims.exclude` 排除那五个包，**冲突依旧**。

## 根因

`owned` 语义只落实在 shim 生成层，运行时路由层漏用了它。

`DynamicToolBin.owned` 的定义（`crates/osdk-core/src/inventory.rs:35-44`）说得很清楚：

```rust
/// Whether the requested package installed this command itself.
///
/// Backends that share one install root between a package and its
/// dependency closure -- conda resolves 16 packages for `clang` -- set this
/// false for the closure's commands. Those stay in the manifest so
/// `[shims] include` can still reach them and `where --bins` can list them;
/// they are only withheld from shims by default.
pub owned: bool,
```

生成层与 reconciliation 都遵守了它。`crates/osdk-cli/src/commands.rs:6093-6109` 还专门注释了为什么必须一致：

```rust
// Expected shims must be filtered exactly as generation filters
// them, or reconciliation deletes what generation just wrote --
// or keeps a shim the user has since excluded.
let owned = install.manifest.bins.iter()
    .find(|bin| bin.name == name)
    .is_none_or(|bin| bin.owned);
if !osdk_core::shim::shim_is_enabled_for(
    &app.ctx.config.settings.shims, &candidate.canonical_id, &name, owned,
) { return None; }
```

但构建路由 owner 表的 `build_bin_ownership_candidates`（`crates/osdk-core/src/inventory.rs:410`）把 manifest 里的 bin **全部**收进来，完全没看 `owned`：

```rust
for bin in &install.manifest.bins {
    owners.entry(bin.name.clone()).or_default().push(BinOwnerCandidate { ... });
}
```

`osdk-shim` 的 `dynamic_backend_for_bin`（`crates/osdk-shim/src/main.rs:651`）就是拿这张表判断冲突的。于是六个包全被算作 bash 的 owner，交集不为 1，路由拒绝执行。

### 为什么 `shims.exclude` 治不了

`ShimSettings`（`crates/osdk-core/src/config/mod.rs:96`）的注释是 "Which of an installed tool's executables get a shim" —— 它决定**生成哪些 shim 文件**，不参与运行时 owner 判定。`build_bin_ownership_candidates` 根本不读这个配置。

所以那句 `re-add one with 'osdk config set shims.include ...'` 的提示，在冲突场景下是误导：它指向的机制管不到这里。

## 实测证据

环境：Windows x64，`osdk 0.0.2`，`E:\osdk-data`。

扫描全部已装 conda 包的 `.osdk-install.json`，统计 bash 的 owner：

| 包 | 导出 bash | `owned` |
| --- | --- | --- |
| `conda:m2-bash` | 有 | **true** |
| `conda:m2-sed` | 有 | false |
| `conda:m2-grep` | 有 | false |
| `conda:m2-gawk` | 有 | false |
| `conda:m2-make` | 有 | false |
| `conda:m2-diffutils` | 有 | false |

**只有一个包真正 own bash。** 按 `owned` 过滤后的全局效果：

| | 冲突命令数 |
| --- | --- |
| 当前行为（不过滤） | **34** |
| 只收 `owned = true` | **0** |

受影响的 34 个命令包括 `bash`、`sh`、`kill`、`iconv`、`cygpath`、`chattr`、`getconf` 等 —— 全是 msys2/cygwin 运行时闭包里的公共命令，任意两个 m2 包同时安装就会互相冲突。

## 影响

- 用户装了 N 个 m2 系列包，就有 34 个命令无法通过 shim 调用，而这些包**单独**装时都正常。
- 冲突数随安装的包数增长，越用越坏。
- 错误提示指向一个治不了的配置项，用户会反复尝试无效配置。
- 唯一有效的绕法是 `osdk exec --tool <包> -- <命令>` 指名，或卸载其他包。

## 修复

`build_bin_ownership_candidates` 只把 `owned = true` 的 bin 计入 owner 集合，与生成层和 reconciliation 保持一致。

选择这个改法而不是新增「冲突时指定生效包」的配置项，理由是：这 34 个冲突**本来就不该存在**。它们不是「两个包都合法提供同名命令」的真冲突，而是把依赖闭包误判成了所有者。为假冲突提供绕法，等于把 bug 固化成需要用户理解的概念。

`owned = false` 的命令仍留在 manifest 里，`shims.include` 仍能显式召回它们——召回后它就是被点名的那个包的合法命令，此时若与别的包真冲突，才是需要消歧的场景。

真冲突（例如 `conda:gcc_win-64` 与 `conda:m2w64-gcc` 都提供 `x86_64-w64-mingw32-gcc`）不受影响，仍然报错，仍需用户卸载其一或用 `--tool` 指名。

## 回归防线

`crates/osdk-core/src/inventory.rs` 的单元测试：

- `unowned_closure_bins_do_not_claim_ownership` — 一个包 own、另一个不 own 时，owner 集合只含前者。这条不成立就会退回本缺陷。
- `two_real_owners_still_conflict` — 两个包都 own 同名命令时，owner 集合仍是 2，冲突不被这次修复掩盖。**没有这条，修复可能变成「无条件只取第一个」，把真冲突静默化，那比原缺陷更危险。**
- `ownership_ignores_manifest_order` — owner 集合与 manifest 中 bin 的排列顺序无关。
