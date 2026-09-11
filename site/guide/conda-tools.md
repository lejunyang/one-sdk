# Conda 开发工具

osdk 通过 `conda:` 命名空间从 conda 频道安装工具链与库。它面向的是那些
**不以单个归档发布**的东西：CUDA、LLVM/clang、以及大量 C/C++ 交叉编译组件。

::: tip 它和其他 backend 有什么本质不同
`github:` 和 `http:` 下载一个归档就完事。conda 包不是自包含的：`conda:clang`
在 linux-64 上只是一个 30 KB 的元包，真正的编译器散落在十几个依赖里。所以
安装一个 conda 包必须先**求解依赖闭包**——这是 osdk 里唯一需要 SAT 求解器的
backend。
:::

## 安装

```bash
osdk install conda:ripgrep
osdk exec --tool conda:ripgrep -- rg --version
```

或者固定到项目里：

```toml
[tools]
"conda:ripgrep" = "15.2.0"
```

安装时 osdk 会解析出完整闭包、逐包校验 sha256、解包进同一个 prefix，全部成功
后才原子地移动到位。中途失败不会留下一个"看起来装好了"的半成品目录。

## 频道

默认频道是 `conda-forge`。用 `channels` 选项指定其他频道：

```bash
osdk install "conda:cuda-nvcc[channels='nvidia,conda-forge']"
```

**顺序即优先级**，不是集合。写在前面的频道优先提供包，osdk 不会替你排序——
排序会静默改变你装到的 build。因此下面三者是三个不同的安装身份：

| 请求 | 结果 |
| --- | --- |
| `conda:cuda-nvcc` | 只从 `conda-forge` 解析 |
| `conda:cuda-nvcc[channels='nvidia,conda-forge']` | 优先 `nvidia`，缺的再从 `conda-forge` 补 |
| `conda:cuda-nvcc[channels='conda-forge,nvidia']` | 优先 `conda-forge`，可能得到不同的 build |

这不是理论差异。在 win-64 上实测，`cuda-nvcc` 在 `nvidia,conda-forge` 下解析
出 27 个包的闭包，在 `conda-forge` 单频道下是 26 个——两者会作为不同指纹的
prefix 并存，互不覆盖。

::: warning cuda-toolkit 在 Windows 上必须用 nvidia 频道
`cuda-toolkit` 在 conda-forge 上**没有 win-64 构建**。Windows 用户必须显式写
`channels='nvidia,conda-forge'`，否则求解会直接失败。这也是本 backend 从第一版
就支持多频道的原因——单频道设计根本无法表达这个需求。
:::

频道名只接受普通名字（`conda-forge`、`nvidia`、`bioconda`）。URL、路径穿越和
带空格的名字会被拒绝，因为频道名会成为下载 URL 的一部分。

## 镜像与加速

osdk 内置 anaconda.org 上游和三个国内镜像（TUNA、BFSU、NJU），用常规的
`osdk source` 命令管理：

```bash
osdk source list conda:clang
osdk source test conda:clang
osdk source pin conda:clang tuna
```

**默认优先上游，这是刻意的**，和延迟测量给出的结论相反。原因是只有上游提供
CEP-16 分片索引（sharded repodata），镜像只有整个 subdir 的 `repodata.json`。
在北京实测 `osdk lsr conda:clang`：

| 来源 | 耗时 | repodata 下载量 |
| --- | --- | --- |
| 上游（分片） | 1.8 s | 1.6 MB |
| 镜像（全量） | 27.6 s | 445.9 MB |

镜像的单字节吞吐确实更快（4.5 对 3.4 MB/s），但快 1.3 倍抵不过多下 278 倍的
数据。镜像留在列表里是作为上游不可达时的故障转移——那才是它们真正有用的场景。

如果你显式 `osdk source pin` 了某个镜像，osdk 会尊重你的选择，老实从镜像下载
全量 repodata。

## 版本

```bash
osdk lsr conda:clang        # 列出远端版本
osdk list conda:clang       # 列出已安装
```

conda 的版本号不是 semver（存在 `2024.06.1`、`1!1.2` 这类 epoch 形式），osdk
使用 conda 自己的版本序来排序和判断预发布，因此 `9.0.1` 排在 `10.0.0` 前面而
不是按字典序。

## 平台支持

| 平台 | conda subdir |
| --- | --- |
| Linux x64 | `linux-64` |
| Linux arm64 | `linux-aarch64` |
| macOS x64 | `osx-64` |
| macOS arm64 | `osx-arm64` |
| Windows x64 | `win-64` |
| Windows arm64 | `win-arm64` |

conda-forge 不构建任何 32 位目标，请求这些平台会得到明确报错，而不是一个空的
求解结果。

win-arm64 是较新的 subdir，覆盖面明显小于其他平台：`clang` 只有 22.1.8 起的
16 个构建，而 linux-64 有 123 个版本。某个包在这里没有构建时，求解会失败并列出
原因，而不是静默装上别的架构。

## 导出哪些命令

一个 conda prefix 装的是整个依赖闭包，所以它的 `bin` 目录里远不止你要的那个包。
`conda:clang` 会解出 16 个包，`bin` 里除了编译器还有 `xmllint`、`zstd` 和一堆 ICU
工具。

**默认只导出请求包自己安装的命令**，依据是 conda 在 `info/paths.json` 里记录的
文件清单。`conda:clang` 因此只导出 3 个：

```bash
osdk where --bins conda:clang
# ...\installs\conda\clang\23.1.1\b3-v2-c48200a0...
# published (3): clang, clang-cl, clang-cpp
# withheld (21): clang++-23, clang-23, derb, ..., xmllint, zstd
```

被挡下的命令仍然装在 prefix 里，只是不生成 shim、不进 PATH。需要某一个时，用已有
的 `[shims] include` 把它加回来：

```toml
[shims]
include = ["conda:clang:xmllint"]
```

`include` 和 `exclude` 都支持 `*` 和 `?` 通配，`exclude` 在 `include` 之后生效，
所以可以先放宽再收窄。这套规则对所有 backend 通用，不是 conda 专有的。

::: tip 没有 paths.json 时会怎样
少数包不提供这份清单。这时 osdk 会导出整个 prefix 的命令，而不是一个都不导出——
多几个命令是可以再收窄的，一个都没有则会让安装直接失效。
:::

## 生命周期命令

```bash
osdk current conda:ripgrep
osdk where conda:ripgrep@15.2.0
osdk --yes uninstall conda:ripgrep@15.2.0
osdk reshim
```

求解、镜像选择、身份指纹与安装发布的实现细节见
[Conda 开发工具实现](./implementation/conda-tools)。
