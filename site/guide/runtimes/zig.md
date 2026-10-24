# Zig

配置参考：[适用的 tool_order 来源](../config/tool-selection#zig)、[项目来源诊断](../config/tool-selection#inspect-sources)、[本工具的选择器、托管变量与缓存覆盖](../config/tool-environment#zig)。

```bash
osdk install zig@latest
osdk use zig@0.16
osdk exec --tool zig -- zig version
```

版本来自 `ziglang.org/download/index.json`，该索引把每个发布的归档 URL 与 SHA-256
放在一起，因此每次安装都经过校验、也可被锁定。Zig 的 GitHub release 只提供源码和
bootstrap 归档，所以通用 `github:` backend 无法安装它。index 中虽然写的是绝对
tarball URL，osdk 仍会按相对发布路径把它重映射到每个排序 source；镜像 index 正常但
目标归档缺失、损坏或无法解包时会继续后备源。

`master` 是滚动 nightly 而非发布版本，按预发布处理：`zig@latest` 只会选中带 tag 的
版本；需要 nightly 时在允许预发布的[策略](../tools/github-releases)下显式执行
`osdk install zig@master`。

## 用 Zig 做 C / C++ 交叉编译

`zig cc` 和 `zig c++` 是自带 libc 的 Clang 驱动：musl、多个 glibc 版本、mingw-w64
和 wasi-libc 都已内置。因此**装一份就能交叉编译到多个目标，不需要为每个目标准备
sysroot** —— 这正是 [Android NDK 在 Android 之外做不到的事](./android#ndk-不是通用交叉编译器)。

```bash
# 同一份源码，无需额外下载，也不用指定 sysroot。
osdk exec --tool zig -- zig cc -target aarch64-linux-musl -o hello hello.c
osdk exec --tool zig -- zig cc -target x86_64-windows-gnu -o hello.exe hello.c
```

在 Windows x86-64 主机上用包含 `<stdio.h>` 的源码实测，以下目标均产出了对应架构的
二进制：

| `-target` | 产物 |
| --- | --- |
| `x86_64-linux-gnu` | ELF，x86-64 |
| `aarch64-linux-gnu` | ELF，aarch64 |
| `x86_64-linux-musl` | ELF，x86-64，静态 |
| `aarch64-linux-musl` | ELF，aarch64，静态 |
| `riscv64-linux-musl` | ELF，riscv |
| `x86_64-windows-gnu` | PE |
| `wasm32-wasi` | wasm |

把 `CC` 指向 zig 后，它也可以充当其他构建系统的 C 编译器，这对带 C 依赖的 Rust
crate 很有用：

```bash
osdk exec --tool zig -- cargo build   # 环境中设置 CC="zig cc"
```

`ZIG_GLOBAL_CACHE_DIR` 会指向 osdk 缓存内的目录，避免构建产物堆积在用户主目录；
若该变量已有值则不覆盖。
