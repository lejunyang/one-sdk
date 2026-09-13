# 003 — `exec` 无条件重跑安装路径，即使工具已装好

**状态**：已修复 · **严重度**：中 · **实测环境**：Windows x64、PowerShell 7、osdk 0.0.2

## 现象

对一个已经装好的工具连续执行 `osdk exec`，每次都会打印 `installing`，并花掉约 7 秒：

```
第 1 次:   9203 ms   installing conda:m2-bash@5.2.37.2 ... | installed ... | ok
第 2 次:   7399 ms   installing conda:m2-bash@5.2.37.2 ... | installed ... | ok
第 3 次:   6994 ms   installing conda:m2-bash@5.2.37.2 ... | installed ... | ok
```

而 `.osdk-complete` 标记的 mtime **每次都被重写**：执行前 `17:18:32`，执行后 `17:18:39`。说明不只是多打了一行日志，磁盘状态确实被改写了。

## 根因

`exec_cmd`（`osdk-cli/src/commands.rs:167`）第二行就无条件走安装路径：

```rust
let requests = gather_requests(app, tools)?;
let resolved = install_requests(app, requests, Vec::new(), false, false).await?;
```

中间没有"磁盘上是否已有满足条件的安装"这一步。但真正的根因在更下一层。

`install_one_without_shims`（`:1035`）本来**有**一条"已装则跳过"的快路径，可它把动态工具排除在外：

```rust
if !force
    && osdk_core::pipeline::is_installed(&app.ctx.dirs, backend.id(), &tv.version)
    && !backend.id().contains(':')          // <- 动态工具被排除
```

这个排除不是疏忽，而是在绕开一处**判定失配**：`is_installed` 查 `install_path(tool, version)` 下的完成标记，而 `install_path`（`dirs.rs:312`）只到 `<tool>/<version>`；动态安装实际在 `<tool>/<version>/<install_id>/`（`dirs.rs:70`），深一级。所以对动态工具，`is_installed` 查的是**上一级目录**，那里根本没有标记，永远返回 false。加上 `!contains(':')` 只是让这个恒假的判断不再参与，行为上等价。

## 修复

新增 `installed_dynamic_match`：先用 `scan_installs_for_tool`（只扫单个 tool 的子树，很便宜）读已装 manifest，再按能在本地确定的两项匹配——`version` 与 `material_options`。

为什么不直接修 `is_installed` 让它认得动态布局？因为**判定"这个 install 就是我要的"需要 install_id，而它算不出来**：`install_id` 是对 tool+version+platform+scope+material_options+dependencies+**materials** 的指纹（`tool.rs:176`），其中 `materials` 是解析后的归档摘要，只有联网下载过才知道。反过来匹配已装 manifest 就绕开了这一点：manifest 里存着算好的 identity，而 `version` 与 `material_options`（由 `dynamic_identity_options` 从请求本地投影而来）两项就足以确定要哪一个。

三个配套要点：

- **快路径放在 `resolve_version` 之前**。它是网络调用，放在之后就仍要付这次往返，而且离线时照样失败。
- **先展开别名再匹配**。`expand_request_alias` 会把 `@lts` 这类别名换成具体 spec，用未展开的 spec 去比对目录名永远不中——那样 bug 只是"看起来修了"，实际静默落回慢路径。
- **`--force` 与 `--refresh` 不走快路径**。前者要真重装，后者要重新解析。

原先那个 `!contains(':')` 保留：动态工具在上面就被处理了，能走到那里的只有 force/refresh，而对它们来说 `is_installed` 的失配判断仍必须保持无效。

## 效果

| | 修复前 | 修复后 |
| --- | --- | --- |
| `exec` 已装工具 | 10,908 ms | **2,415 ms**（4.5x） |
| 输出 | `installing ...` | `already installed` |
| `.osdk-complete` mtime | 每次重写 | **不变** |
| `--force` | 重装 | 仍重装（mtime 变、打印 installing） |

连续三次执行均为 2.4 s 左右，且 `bash -c 'echo RESULT-OK'` 的回显确认命令真的跑了，不是快路径把工作跳空了。

## 影响

- **每次调用固定 7~10 秒**。`exec` 的定位本应是"临时用一下某个工具"，这个开销让它在脚本和循环里不可用。
- **无谓重写磁盘**。完成标记被反复改写，会干扰任何依据 mtime 判断新鲜度的逻辑（包括缓存与增量构建），也让"这个安装是什么时候装的"这一信息失真。
- **离线不可用**。安装路径要联网做版本解析，所以断网时 `exec` 一个已装好的工具也会失败。

## 相关观察：`exec --tool` 不做完整激活

排查时顺带确认的另一件事，与嵌套调用挂死有关：`exec --tool X` 只把 X 的 bin 目录加进 `PATH`，不做整套激活。在 `exec --tool conda:m2-bash -- bash -c '...'` 里观察到：

| 变量 | 值 |
| --- | --- |
| `PATH` 含 shims | 是 |
| `OSDK_ENV` | 未设置 |
| `gcc` 可见 | 否 |

所以在这个 bash 里再调 osdk，它看不到已激活状态，会当成"未安装"而尝试安装——这正是嵌套 osdk 会弹出安装提示的原因。

## 回归防线

功能测试抓不到这个缺陷：结果完全正确，只是慢和多写磁盘。所以两个测试锁的是修复**所依赖的不变量**，而不是表面行为：

- `a_dynamic_install_root_sits_below_the_fixed_tool_install_path` — 动态 install root 必须严格深于 `install_path(tool, version)` 一级。若哪天两者相等，`is_installed` 的标记检查就会开始对动态工具生效，本修复的前提随之失效。
- `the_install_id_depends_on_materials_that_only_a_download_can_supply` — 同样的 tool+version+options，带 materials 与不带 materials 算出的 `install_id` 必须不同；而 `version` 与 `material_options` 必须相同。这条拦的是"把本地判定简化成重算 install_id"这种改法：它能编译、功能测试全绿，但快路径会永远不命中，`exec` 悄悄退回每次重装。

端到端验收标准仍是 mtime：对已装工具执行 `exec` 后 `.osdk-complete` 的 mtime 必须不变。这比断言日志文案可靠，也不随文案翻译而失效。

### 变异测试记录

又踩到一次「变异存活 ≠ 断言无效」。第一版测试用测试模块里的 `write_install` 辅助构造路径，而它自己拼 `sanitize_tool_id` / `sanitize_version_component` / `install_id_component` 三段，**完全没经过 `InstallLocator`**——于是改 `InstallLocator` 的变异对它是空操作，变异存活。

按 AGENTS.md 先核实"这个变异真的改变了程序的可观察行为吗"，发现是探针打偏，而不是断言不足。改成经 `InstallLocator` 取路径（与生产代码同源）后，两个变异都被抓到：

| 变异 | 结果 |
| --- | --- |
| `install_id` 不再纳入 `materials` | 2 个测试变红 |
| 动态 install root 不再多 `install_id` 一级 | 1 个测试变红 |
