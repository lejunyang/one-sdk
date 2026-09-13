# 003 — `exec` 无条件重跑安装路径，即使工具已装好

**状态**：待修复 · **严重度**：中 · **实测环境**：Windows x64、PowerShell 7、osdk 0.0.2

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

中间没有"磁盘上是否已有满足条件的安装"这一步。`install_requests`（`:595`）本身也不是快路径：它会做 `inject_managed_dependencies`、`apply_source_override`、`preflight_android_licenses`，然后进入完整的解析与安装流程。

## 影响

- **每次调用固定 7 秒**。`exec` 的定位本应是"临时用一下某个工具"，这个开销让它在脚本和循环里不可用。
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

## 怎么抓到它

功能测试抓不到：结果完全正确，只是慢和多写磁盘。要抓它需要断言**副作用**而非结果：

- 对已装好的工具执行 `exec`，断言 stdout **不含** `installing`；
- 断言 `.osdk-complete` 的 mtime 在执行前后**不变**——这条最直接，且不依赖日志文案。

第二条同时也是修复的验收标准：修好之后 mtime 必须保持不变。
