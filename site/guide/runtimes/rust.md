# Rust

配置参考：[适用的 tool_order 来源](../config/tool-selection#rust)、[项目来源诊断](../config/tool-selection#inspect-sources)、[本工具的选择器、托管变量与缓存覆盖](../config/tool-environment#rust)。

Rust backend 委托给安装在 osdk 隔离目录中的 rustup：

```bash
osdk install rust@stable
osdk install rust@nightly -o profile=minimal \
  -o components=clippy,rustfmt \
  -o targets=wasm32-unknown-unknown,x86_64-pc-windows-gnu
```

| 选项 | 值 | 默认值 |
| --- | --- | --- |
| `profile` | rustup 接受的 profile | `default` |
| `components` | 逗号分隔的 rustup component | 空 |
| `targets` | 逗号分隔的 rustup target | 空 |

`latest`、`lts` 和 `system` 都映射为 `stable`；`stable`、`beta`、`nightly` 与精确
工具链基本原样交给隔离 rustup。rustup bootstrap 自身固定使用 `minimal` 且不装默认
toolchain。运行时导出 `RUSTUP_HOME=<data>/rustup` 和 `CARGO_HOME=<data>/cargo`。
Lock 也会原样保存这些浮动 channel，因此以后重装 `stable`、`beta` 或 `nightly` 可能
得到更新 toolchain；需要不可变结果时请写明确版本或带日期的 toolchain。

source probe 只用通用 stable manifest 排序，不代表镜像已经同步请求的精确 toolchain、
component 或 target。所有会下载的 rustup 操作都会按排序逐个运行完整命令；某个源返回
404、下载失败或 rustup 拒绝其内容时会继续下一个源，直到命令真正成功。

## 暴露的命令与 `cargo install`

安装和 `osdk reshim` 时，osdk 会为活动工具链 `bin` 与隔离 `CARGO_HOME/bin` 里的**全部**
可执行文件生成 shim，而不只是 `rustc`、`cargo`、`rustup`、`rustfmt`、`clippy-driver`
五个核心命令：`rustdoc`、`rust-analyzer`、`cargo-miri` 等 rustup 代理，以及之后用受管
`cargo` 安装的 CLI 都会被暴露。这些 shim 运行时同样注入隔离的 `RUSTUP_HOME`、`CARGO_HOME`，
不会误连到系统 rustup。

用受管 `cargo install` 安装的第三方工具进入 `<data>/cargo/bin`（而不是系统 `~/.cargo`），
装完执行一次 `osdk reshim` 即可让新命令在 shell 中直接可用；`cargo <子命令>` 形式
（如 `cargo tauri`）无需 reshim，cargo 会直接在隔离 `CARGO_HOME/bin` 找到对应程序。

## 与系统 rustup 的管理边界

osdk 只驱动数据目录下的隔离 rustup，不接管已经安装在系统 `PATH` 上的 rustup：

- 未执行过 `osdk install rust` 时，隔离 rustup 不存在，`osdk rust *` 会明确报错并
  提示先安装；它不会转而调用系统 rustup。
- `osdk source pin rust <源>` 与一次性的 `--source <源>` 只改变 osdk 驱动受管
  rustup 时注入的 `RUSTUP_DIST_SERVER`，不会修改外部 rustup 的环境变量或配置；未安装
  受管 Rust 时命令会额外打印这条作用域提示。要让系统 rustup 走镜像，请自行配置
  `RUSTUP_DIST_SERVER`、`RUSTUP_UPDATE_ROOT` 等环境变量。
- 会下载的受管操作（`osdk install rust`、`osdk rust component add`、
  `osdk rust target add`）共用同一套源选择，因此固定的源对补装 component、target
  同样生效；pin 只把该源排到第一位，目标在该源不可用时仍会回退。
- 受管 rustup 始终使用 osdk 选定的源：shell 中已经导出的 `RUSTUP_DIST_SERVER`、
  `RUSTUP_UPDATE_ROOT` 不会影响受管操作，避免外部镜像覆盖 osdk 的选择。若要临时改用
  其他源，请使用 `--source <源>`，而不是导出环境变量。

## Component 与 target

```text
osdk rust component add NAME [--toolchain TOOLCHAIN]
osdk rust component remove NAME [--toolchain TOOLCHAIN]
osdk rust component list [--toolchain TOOLCHAIN]
osdk rust target add NAME [--toolchain TOOLCHAIN]
osdk rust target remove NAME [--toolchain TOOLCHAIN]
osdk rust target list [--toolchain TOOLCHAIN]
```

`--toolchain` 默认 `stable`，命令直接作用于隔离 rustup。

```bash
osdk rust component add rustfmt --toolchain stable
osdk rust target add wasm32-unknown-unknown --toolchain stable
```

## 状态、override 与本地工具链

```text
osdk rust check [--repair]
osdk rust override import [PATH]
osdk rust override export [PATH]
osdk rust toolchain link NAME PATH
```

- `check` 先执行隔离 `rustup check`；`--repair` 再按真实 toolchain 目录补建缺失 marker，
  并移除没有对应 toolchain 的 marker。
- `override import` 从可选目录（默认当前目录）的隔离 rustup override 读取 toolchain，
  写入最近项目 `osdk.toml`。
- `override export` 把该目录的活动 osdk Rust pin 写成隔离 rustup 的目录 override。
- `toolchain link` 要求 `PATH` 可规范化且包含 `bin/`；`NAME` 不能有空白、斜杠，
  也不能是 `.` 或 `..`。

linked toolchain 可被 shim 和 shell 使用，但它是本机路径，`osdk lock` 会拒绝把它
写成可复现 artifact。
