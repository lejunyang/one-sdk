# FFmpeg 的 OneSDK Backend 选择（2026-10-28）

默认建议 `conda:ffmpeg`：语法简单，默认 conda-forge，跨 Linux/macOS/Windows 选择本机
构建，OneSDK 负责求解依赖闭包并逐包校验 SHA-256。若只需 Linux/Windows 独立 CLI、
希望明确选用某个第三方静态构建，`github:BtbN/FFmpeg-Builds` 也适合，但必须处理
资产选择、校验清单及平台边界。

本次没有安装 FFmpeg，也没有下载 FFmpeg 归档；只查公开文档、发布清单、SHA-256 清单，
用当前实现校验请求语法，并在临时目录完成离线静态 catalog 的 lock 解析。
代码基准 `3139626`；认证修复提交 `c1a1c01` 未修改这些 Backend。

## 推荐命令

```sh
osdk use -g conda:ffmpeg
```

该命令安装并设置用户全局选择。项目可用 `osdk use conda:ffmpeg`，成功后会更新项目
配置与当前平台的 lock。仅安装而不改选择用 `osdk install conda:ffmpeg`。
查看已安装版本暴露的命令用 `osdk where --bins conda:ffmpeg`；不要加 `--global`，
`where --global` 当前只支持 npm 工具。

这是 conda-forge 的 FFmpeg 二进制包，不是 `pypi:ffmpeg` / `npm:ffmpeg` 名称下的
封装库。各平台可用构建和编解码器组合应以实际 channel 包为准。

## 对比

| 项目 | `conda:ffmpeg` | `github:BtbN/FFmpeg-Builds` |
| --- | --- | --- |
| 依赖 | 求解整个 conda 闭包，逐包校验、链接到独立 prefix；默认只公开根包拥有的命令 | 单个归档；static 适合独立 CLI，shared 还依赖包内 DLL/so 与布局，不能只搬走两个可执行文件 |
| 体积 | 闭包依赖数和总大小随平台/构建变化，本次未求解，不能报确定数字 | 选定 Linux x64 GPL 8.1 归档的发布页显示约 144.1 MiB；解压后的大小未测，也不能断言比 conda 总量小 |
| 版本与复现 | 通过 conda 版本/构建/闭包解析；项目 lock 分平台记录，摘要来自 repodata | 精确 tag + 精确资产 + SHA-256/static catalog；`latest` 是浮动的，固定 tag 也不能防止上游删除资产 |
| 多命令 | 通过 package ownership 导出 ffmpeg/ffprobe 等根包命令，实际名单可用 `where --bins` 查看 | `bins=bin/ffmpeg,bin/ffprobe` 支持多个文件；不只支持一个二进制 |
| 平台 | 当前包页列 Linux x64/arm64、macOS x64/arm64、Windows x64；Backend 认识 win-arm64 不代表 FFmpeg 在该平台一定有包 | BtbN 自动构建 Linux/Windows x64、arm64，没有 macOS；Linux 要求 glibc ≥ 2.28、内核 ≥ 4.18，不是 musl/Alpine 通用构建 |
| 运维成本 | 单个短命令即可使用 | 必须选择 OS/arch/分支/GPL或LGPL/static或shared；自动评分不保证选中想要的组合 |

FFmpeg 官方只分发源码，下载页面链接外部编译版本；不要把 `github:FFmpeg/FFmpeg`
当作预编译 Release 来源。BtbN 是第三方构建，不是 FFmpeg 官方二进制发布。

## GitHub 多二进制与准确语法

公开选项是 `asset-template` 或 `asset-regex`，**不是 `asset`**。
`bin` 与 `bins` 互斥，`rename` 只支持最终一个 bin。`strip-components=1` 指向归档里
唯一顶层目录，随后从其 `bin/` 复制选中的文件到 OneSDK 发布目录；该布局来自 BtbN
`build.sh` 的 `pkgroot/$BUILD_NAME` 打包逻辑。本次未下载归档检查，所选两个实际文件是否
存在仍由安装时的 fail-closed 校验负责；没有把 ffplay 的存在当作已验证事实。

普通 GitHub 请求语法如下（只是说明参数；更推荐下面带摘要的 catalog）：

```sh
osdk use -g 'github:BtbN/FFmpeg-Builds@=autobuild-2026-10-27-18-38' \
  -o asset-template=ffmpeg-n8.1.3-24-g92611900bb-linux64-gpl-8.1.tar.xz \
  -o bins=bin/ffmpeg,bin/ffprobe \
  -o strip-components=1
```

`@=` 经解析器确认产生 `VersionSpec::Pinned`，严格匹配 tag；它不是裸 `@latest`。
这条资产只适合 Linux x64 glibc。Windows 要选对应 ZIP 并写 `.exe` 路径；arm64 要换
资产；不要用 `os=` / `arch=` 覆盖把别的平台文件强行装到本机。macOS 优先用 conda。
shared 归档的库相对路径可能受复制布局影响，本次没有实跑 shared 构建。

## 当前摘要发现限制与可校验方案

BtbN 发布的共享清单名为 `checksums.sha256`。当前 `pipeline/verify.rs` 只尝试
单文件 `.sha256` / `.sha256sum` / `.sha256.txt`，以及 `SHASUMS256.txt`、`SHA256SUMS`、
`sha256sums.txt`、`checksums.txt`，没有覆盖前者。因此不能保证普通 GitHub 路径自动发现
BtbN 摘要；若没有其他可验证证据，默认允许无摘要安装，`--require-checksums` 会拒绝。
`github:` 也不接受 `-o sha256=...`；这个公开选项属于 `http:`。

随报告提供 [Linux x64 静态 catalog](ffmpeg-linux64-catalog-2026-10-28.json)，里面是
从公开 `checksums.sha256` 核实的摘要：

```text
292e10a463befababcd31e0c7d0c1e068d46342398d9674534eb07517c1ba38e
```

在仓库根目录，完整的校验安装命令为（本次未执行）：

```sh
osdk --require-checksums use -g 'github:BtbN/FFmpeg-Builds@=autobuild-2026-10-27-18-38' \
  -o catalog-url=docs/research/ffmpeg-linux64-catalog-2026-10-28.json \
  -o catalog-sha256=f8bc2855fd9dff7c8dfe5db41d78ef29f42f4f9b69245fe8077040b5c1557e4f \
  -o asset-template=ffmpeg-n8.1.3-24-g92611900bb-linux64-gpl-8.1.tar.xz \
  -o bins=bin/ffmpeg,bin/ffprobe \
  -o strip-components=1
```

catalog 固定了 OS/arch/libc、资产 URL 与 SHA-256，catalog 自身的摘要另行校验。
本次把同一组选项交给新构建的 CLI 执行 `--offline --require-checksums lock`，只写入临时
`osdk.lock`，确认精确 tag、两个 bins、URL、归档 SHA-256 都正确，没有创建 FFmpeg 安装目录。
Core 自带的 `static_catalog_locked_artifact_installs_offline_with_multiple_bins` 测试也在
完整 Linux/Wine 套件中通过；它使用合成归档，不等于实际 FFmpeg 安装验收。

BtbN 保留最近 14 个日构建，以及每月最后构建两年；示例 tag 将来可能被删除。
要长期复现，需要保留可信的已校验归档/镜像，不能只保留 URL 或 lock。
本次没有修改摘要发现逻辑，属于调研中记录的已确认兼容性限制。

## 依据

- [FFmpeg 下载说明](https://ffmpeg.org/download.html)：官方源码及外部二进制入口。
- [conda-forge FFmpeg](https://anaconda.org/conda-forge/ffmpeg)：包及平台列表。
- [BtbN README](https://github.com/BtbN/FFmpeg-Builds)：平台、glibc 下限、变体、保留策略。
- [固定 Release](https://github.com/BtbN/FFmpeg-Builds/releases/tag/autobuild-2026-10-27-18-38)：资产名称与尺寸。
- [公开 SHA-256 清单](https://github.com/BtbN/FFmpeg-Builds/releases/download/autobuild-2026-10-27-18-38/checksums.sha256)。
- [BtbN 打包脚本](https://github.com/BtbN/FFmpeg-Builds/blob/master/build.sh)：顶层归档目录。
- 本地实现：`backend/conda.rs`、`backend/github.rs`、`pipeline/verify.rs`、`tool.rs`、
  `version/mod.rs`、`osdk-cli/src/cli.rs`；使用指引 `site/en/guide/tools/{conda,github-releases}.md`。
