# `osdk rust`

管理 osdk 隔离 rustup 状态中的组件、target、目录 override 和链接 toolchain。

```bash
osdk rust component add|remove|list [NAME] [--toolchain TOOLCHAIN]
osdk rust target add|remove|list [NAME] [--toolchain TOOLCHAIN]
osdk rust check [--repair]
osdk rust override import|export [PATH]
osdk rust toolchain link <NAME> <PATH>
```

- `component/target add|remove` 需要名称；`list` 不需要。`--toolchain` 默认 `stable`。
- `check` 比较隔离 rustup 状态与 osdk marker；`--repair` 修复陈旧/缺失 marker。
- `override import` 把 rustup 目录 override 写入项目 `osdk.toml`。
- `override export` 把当前 osdk Rust pin 显式写成 rustup override。
- `toolchain link` 将本地路径登记为 osdk/rustup toolchain。

```bash
osdk rust component add clippy --toolchain 1.98.0
osdk rust target add x86_64-pc-windows-gnu --toolchain 1.98.0
osdk rust check --repair
```

持久 Rust 版本、components、targets、profile 写在 `[tools].rust`，见
`reference/configuration/tools.md`。`targets` 只声明宿主额外需要安装的交叉目标。

Rust source probe 使用通用 stable manifest，只负责排序。安装 toolchain 或执行
`component/target add` 时，osdk 会依次用每个候选源运行完整 rustup 命令；目标版本在
首选镜像缺失或下载失败时自动回退。pin 仍是“优先尝试”，不是“禁止回退”。
