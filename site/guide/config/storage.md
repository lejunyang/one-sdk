# 目录与存储

osdk 的状态都放在三个可覆盖的根目录下：数据、缓存和配置。本页列出默认位置与派生目录，以及安装文件如何经由内容寻址存储（CAS）物化。缓存的查看与回收见[缓存与清理](../maintenance/cache)。

## 目录布局与覆盖

| 用途 | Linux 默认位置 | 环境变量 |
| --- | --- | --- |
| 数据根 | `~/.local/share/osdk` | `OSDK_DATA_DIR` |
| 内容存储 | `<data>/store` | `OSDK_STORE_DIR` |
| SDK 安装 | `<data>/installs` | `OSDK_INSTALL_DIR` |
| 缓存根 | `~/.cache/osdk` | `OSDK_CACHE_DIR` |
| 配置目录 | `~/.config/osdk` | `OSDK_CONFIG_DIR` |

派生目录：

```text
<data>/models                模型快照
<data>/shims                 命令 shim
<data>/rustup                隔离 RUSTUP_HOME
<data>/cargo                 Rust backend 的隔离 CARGO_HOME；受控 cargo-binstall 位置
<data>/plugins               声明式 backend
<cache>/downloads            SDK/模型下载文件
<cache>/tmp                  临时解压
<cache>/remote               metadata 与证明缓存
<cache>/sources              来源测速缓存
<cache>/pkg                  下游工具原生缓存
```

`OSDK_BIN_DIR` 属于 osdk 二进制安装脚本，不是 SDK 状态目录；不要把它与
`OSDK_INSTALL_DIR` 混用。

安装脚本会在安装完成后询问 `OSDK_CONFIG_DIR`、`OSDK_DATA_DIR` 和 `OSDK_CACHE_DIR`，
并把选定值写进所选 shell 的启动文件，因此这三项通常不必手工设置。详见
[安装后的 shell 配置](../start/installation#安装后的-shell-配置)。

## CAS 与物化

验证、解压后的 SDK 文件和模型文件按 BLAKE3 内容哈希写入 CAS，路径形如
`store/<前2位>/<第3-4位>/<完整hash>`。安装/快照 manifest 记录相对路径、hash、Unix
mode 和 symlink 目标，再把对象物化到版本目录。

`settings.link_mode` 或 `OSDK_LINK_MODE` 接受：

| 模式 | 行为 |
| --- | --- |
| `auto` | 同文件系统优先 hardlink，再 reflink、copy；跨文件系统尝试 reflink 后 copy |
| `hardlink` / `hard` | 优先硬链接，失败回退 copy |
| `reflink` / `clone` / `cow` | 优先写时复制克隆，失败回退 copy |
| `copy` | 普通字节复制 |
| `symlink` / `sym` | 显式符号链接；失败不回退，且 `auto` 永不选择它 |

SDK/模型 CAS 不包含不同 package manager 的原生缓存；后者格式互不兼容，见
[原生缓存语义](../tools/package-managers#原生缓存语义)。
