# 工具请求与 backend 选项

多数工具命令接受 `<tool>[@<version>]`。先确认是固定 backend 还是动态命名空间，
再决定版本选择子与 `-o KEY=VALUE`。

## 固定 backend

`node`、`npm`、`pnpm`、`yarn`、`go`、`python`、`java`、`maven`、`gradle`、
`kotlin`、`rust`、`deno`、`bun`、`zig`，以及：

- `android-ndk`
- `android-platform-tools`
- `android-build-tools`
- `android-cmdline-tools`
- `android-cmake`
- `android-platforms`
- `android-emulator`
- `android-sources`
- `android-system-images`

## 动态命名空间

| 形式 | 示例 | 关键选项 |
| --- | --- | --- |
| `npm:<package>` | `npm:prettier@3`、`npm:@antfu/ni` | `installer=npm\|pnpm`、`allow_builds=a,b` |
| `cargo:<crate>` | `cargo:ripgrep@14` | `features=a,b`、`no-default-features=true` |
| `cargo:<https-git-url>` | `cargo:https://github.com/o/r.git@tag:v1` | `package=`、`bins=` |
| `go:<module-or-command>` | `go:golang.org/x/tools/gopls@0.20.0` | `tags=`、`env=K=V,...` |
| `conda:<package>` | `conda:cuda-toolkit@12.8` | `channels=`、`with=` |
| `pypi:<project>` | `pypi:httpie@3` | `python=`、`extras=`、`index=`、`extra-index=`、`allow-build-from-source=true` |
| `github:owner/repo` | `github:sharkdp/fd@10` | `asset=`、`bin=` / `bins=`、`strip-components=` |
| `http:https://...{version}` | 直接 HTTPS 制品 | `sha256=`、`kind=file\|zip\|tar.gz\|tar.xz`、`bin=` / `bins=`、`subdir=`、`rename=`、`strip-components=` |

动态工具名作为 TOML 键时要加引号：

```toml
[tools]
"npm:prettier" = "3"
"go:golang.org/x/tools/gopls" = { version = "0.20.0", tags = "tools" }
```

## 版本选择子

- `@20`：版本前缀/约束，解析为满足条件的最高版本。
- `@=20.11.1`：精确版本，不漂移。
- `@latest`：最新稳定版。
- Git 来源可用 `tag:<ref>`、`branch:<ref>`、`rev:<40位提交>`。
- Rust 的 `stable` / `beta` / `nightly` 是浮动 channel；要可复现，使用明确版本或
  带日期 nightly 并写入 `osdk.lock`。

## 选项落盘

CLI 的 `-o KEY=VALUE` 对本次列出的所有工具生效；持久写法是结构化 `[tools]` 条目：

```toml
[tools]
rust = { version = "1.98.0", profile = "minimal", components = "rustfmt,clippy" }
java = { version = "21", distribution = "zulu", package-type = "jdk" }
```

不要把 lock 内部的 replay metadata 当作公开 `-o` 选项；未知或私有选项会被拒绝。
