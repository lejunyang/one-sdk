# 系统包管理器的检查机制

`osdk pkg` 面对的是宿主自己的包管理器——winget、Homebrew。这类东西和 SDK 不同：
osdk 不拥有它们，只能询问。本页说明这个子系统为什么长成现在的样子。

## 为什么不是一个 backend

最直接的想法是给 winget 写一个 `Backend` 实现，但这条路会付出与收益不相称的代价。

`Registry::new()` 会把全部 backend 实例化为 `Arc<dyn Backend>`，于是 trait 上的
每个方法都进入 vtable，链接器无法证明它不可达、也就无法裁剪。历史上这件事让
`osdk-shim` 白背过 5.15 MB——同样 13 个 backend，`dyn` 分发 7.02 MB，静态分发 1.83 MB。

更根本的是概念不匹配：backend 描述的是"osdk 能安装和切换版本的 SDK"，而系统包管理器
是"osdk 只能询问的宿主设施"。`container/` 子系统面对 Docker、containerd、BuildKit 时
做过同样的判断，刻意不实现 `Backend`。`syspkg/` 与它同构。

实测结果印证了这个选择：新增该子系统后 `osdk` 从 11.92 MB 增至 11.95 MB（+0.25%），
`osdk-shim` **保持 3.5 MB 不变**。

## 为什么门控在 `install` feature 之后

shim 的职责是启动工具，永远不需要知道 winget 在做什么。因此 `syspkg` 模块整体挂在
`#[cfg(feature = "install")]` 之后，与 `self_update`、`verification` 同列。这保证
子系统连同它的进程探测代码完全不进入 shim 的构建。

验证时必须**分两次独立调用** cargo：

```powershell
$env:CARGO_TARGET_DIR="target\size-check"
cargo build --release -p osdk-cli
cargo build --release -p osdk-shim
```

单次 `--workspace` 构建会统一 feature，把 `install` 重新打开，测出的 shim 体积是错的。

## 为什么不解析包管理器的表格输出

这是本子系统最重要的一条设计约束，也最容易被低估。

在简体中文的 Windows 上，`winget list` 的列头是「名称 / ID / 版本 / 可用 / 源」，
`winget source list` 是「名称 / 参数 / 显式」。**按英文列头编写的解析器在那里不会报错，
而是静默失效**——找不到列就当作"包未安装"。这比崩溃危险得多。

因此输出层按三级优先设计，且**不含任何列头字面量**：

1. **退出码**。这是唯一与语言无关的通道：查询无匹配在任何界面语言下都是
   `0x8A150014`。因此分类只看 `exit_code`，不看消息文本。
2. **JSON 通路**。`winget source export` 直出 JSON Lines，键名为英文且不随界面语言
   变化，还带 `TrustLevel` 结构化字段；`winget export -o` 输出 schema 2.0 的包清单。
3. **不再有第三选择**。表格解析被排除在外。

`report.rs` 的测试用中文字面量做反向断言，确保这些文本永远不会泄漏进机器可读输出。

## 为什么不传 `--no-progress`

设计初稿假定需要 `--no-progress` 抑制 stdout 的进度污染。实机验证（Windows 11,
winget v1.29.290）推翻了这个假设：

- 重定向到文件后，加与不加该标志的输出**逐字节相同**，均无 ESC 序列。
- 该标志已从 osdk 调用的七个子命令的帮助文本中**移除**，但仍被静默接受。

依赖一个既无文档保证、又无实际收益的标志是纯粹的负债。调用约定改用
`--disable-interactivity` 与 `--nowarn`，两者都在当前文档化的选项列表中。
`winget.rs` 有一条测试专门断言 osdk 不会传 `--no-progress`。

## 探测的边界

- **只读**。发现阶段运行两条查询命令，不安装、不改配置、不提权。
- **有超时上限**。用户在等，因此无响应的管理器必须超时失败而不是挂起。
- **可注入**。探测通过 `CommandRunner` 执行，测试用脚本化的 runner 覆盖各种宿主状态，
  无需真的装上 winget。
- **记录目的而非命令行**。`ProbeRecord` 存的是探测目的与退出码，不存参数，
  这样既不会随 flag 变化而失效，也不会泄漏参数内容。

## 平台缺失与安装缺失是两回事

`ManagerStatus` 把 `NotApplicable` 和 `NotInstalled` 分开，因为两者的处置完全不同：
macOS 上没有 winget 不是需要修的问题，doctor 不应建议用户去安装。报告结构里所有
已知管理器恒定出现，不随宿主变化——消费者因此可以依赖固定的输出形状。
