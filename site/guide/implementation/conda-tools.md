# Conda 开发工具实现

`conda:` 是 osdk 里唯一自带依赖求解器的 backend。这一页记录为什么必须如此，
以及几个和其他 backend 明显不同的取舍。

## 为什么躲不开求解

其他归档 backend 的模型是"一个请求对应一个归档"。conda 不成立：

- linux-64 的 `clang` 是 0.03 MB 的元包，一层展开就有 12 个依赖、累计约 40 MB，
  且远未到闭包（`libclang-cpp` 24 MB、`libstdcxx` 6 MB、`llvm-openmp` 6 MB……）
- 依赖约束形如 `clang-23 ==23.1.1 default_h7037f76_0`、`libstdcxx >=15`、
  `__glibc >=2.17,<3.0.a0`

最后一条尤其关键：`__glibc` 是**虚拟包**，描述的是宿主机能力而非某个可下载的
包。所以求解前必须先 `VirtualPackage::detect`，否则求解器无法评估这类约束，会
把本来能在本机运行的包判为不可用。

实现依赖 rattler + resolvo。走 `ChannelPriority::Strict`，这样 `channels` 里的
顺序才真正是优先级，而不是被求解器当作等价来源合并。

## 下载复用 osdk 自己的管线

rattler 自带 HTTP 入口，但它需要额外的 `reqwest_middleware` 依赖，而 osdk 已经
有断点续传、重试和进度显示。因此下载走 `pipeline::download` + `verify_file`，
rattler 只负责 `fs::extract` 解包。

摘要来源只能是 `repodata.json`：anaconda.org 的文件元数据 API 里 `sha256` 字段
是空的。**缺摘要即拒装**，不存在"没有校验就先装上"的分支。

解包放在 `spawn_blocking` 里执行，解完立即删归档。

## 元数据为什么优先上游而不是最近的镜像

通用 source 探测测的是单个小文件的 RTT，据此会选中延迟最低的国内镜像。但这个
backend 的成本不由延迟主导——只有 anaconda.org 提供 CEP-16 分片索引
（`repodata_shards.msgpack.zst`，0.41 MB），国内镜像的 shard index 一律 404，
rattler 会回落到整个 subdir 的 `repodata.json`（win-64 是 268 MB，zst 压缩后
35 MB）。

实测差距见用户指南里的表格：1.8 s / 1.6 MB 对 27.6 s / 445.9 MB。所以代码里
有一个 `serves_sharded_repodata()` 正列表和独立的 `metadata_base()`，默认走分片
源；只有用户显式 pin 或把 selection 调成非 `Auto` 时才尊重其选择。

SJTU 被刻意排除：`mirror.sjtu.edu.cn` 拒绝连接，`mirrors.sjtug.sjtu.edu.cn` 对
anaconda 路径 404，留着只会白等一次探测超时。

## 安装身份：一个 prefix 由 N 个包构成

动态安装契约是围绕"一个下载的制品"设计的，要求 `artifact-file` 与
`artifact-checksum` 一对材料。conda prefix 没有单一制品，因此身份绑定到**整个
闭包的摘要**：每个包的 URL 加它的 sha256，排序后哈希，`artifact-file` 记为
`conda-closure-<N>.json`。

排序是为了让求解器返回顺序不影响摘要；URL 和摘要都参与，是为了让频道无法在同
一个名字下换掉字节。

这样就得到了指纹目录真正的用途：同一版本的不同 build（换了频道顺序，或上游重新
构建过）会落到不同的安装根，而不是互相覆盖。

由于摘要只有求解后才知道，所有"不求解就要拿到 prefix"的路径——`bin_paths`、
`uninstall`、shim——都改为从 inventory 反查已安装身份。匹配到多个时报错而不是
猜一个：猜错就会运行到另一个 build。

## 为什么不复用 finalize_artifact_install

共享的 `dynamic::finalize_artifact_install` 会拒绝安装根下的任何符号链接。conda
包里符号链接是常态而非例外：conda-forge 的 linux-64 `zlib` 就带着
`lib/libz.so -> libz.so.1.2.13`，带版本号的动态库普遍如此。直接复用会让这个
backend 在 Linux 上基本不可用。

因此 conda 有自己的 finalize，保留了真正重要的防护：

- 归档名在**下载前**校验，且先 percent-decode 再判断，防止频道用
  `%2E%2E%2Fevil` 写到下载目录之外
- 每个发布的命令都必须**穿过符号链接解析后**仍落在 prefix 内部，即包不能导出
  一个指向文件系统其他位置的命令

`validate_dynamic_install` 同理：校验指纹根、完成标记、inventory 清单和 receipt
与身份一致，但不做全树符号链接扫描。

## Windows 上的 bin 目录

conda 在 Windows 的经典布局是 prefix 根、`Scripts\`、`Library\bin\`。但这不够：
从 unix 布局交叉构建的包（ripgrep 就是，它的 `rg.exe` 装在 `bin\rg.exe`）也用
`bin\`。只列经典三处会让装好的工具看起来一个命令都不导出。四处都要搜，且只返回
真实存在的目录，避免制造无效 PATH 条目。

## 命令归属：谁装的这个可执行文件

其他 backend 里"装了什么"和"该导出什么"是同一件事。conda 不是：prefix 是整个闭包
共用的，`conda:clang` 的 `bin` 里有 24 个命令，只有 3 个来自 clang 本身，其余是
libxml2、zstd、ICU 顺带装进来的。全导出会污染 PATH，多装几个包还会互相抢名字。

conda 在每个包的 `info/paths.json` 里记录了它安装的全部文件，这让归属可判定。但有
一个时序陷阱：所有包都解包进同一个 prefix，**后一个包的 `info/` 会覆盖前一个**。
所以必须在解包循环内、装完目标包的那一刻立即读取，写进 `.osdk-conda-paths.json`，
之后再想查就晚了。

判定命令的规则是"直接位于某个 bin 目录下"。这里要求目录**精确相等**而不是前缀匹配：
Windows 上 prefix 根本身就是命令目录（相对路径为空串），若用前缀匹配，
`lib/libclang.so` 会因为"以空串开头"被误判成命令。这个 bug 是单测抓出来的，不是推理
出来的。

manifest 和 `bin_names` 必须用同一套过滤。manifest 决定生成哪些 shim，只改
`bin_names` 会得到一个"报告 3 个、却仍生成 24 个 shim"的自相矛盾状态。

没有 `paths.json` 或清单为空时回退到导出整个 prefix。方向是刻意的：多导出几个命令
用户可以再收窄，一个都不导出会让安装彻底失效。

用户要找回某个依赖的命令时，复用已有的 `[shims] include`——它本来就支持
`backend:name` 形式的 glob，不需要为 conda 新造一套配置。

## 二进制体积

这是 osdk 里代价最大的一个 backend。引入求解与 repodata 栈后：

| 二进制 | 变化 |
| --- | --- |
| `osdk` | 9.118 → 11.849 MB（+29.9%） |
| `osdk-shim` | 3.487 → 3.489 MB（+0.06%） |

超出仓库 10% 的阈值。这笔开销就是特性本身：依赖求解是 conda 包的刚需，也是其他
backend 做不到的事。

需要注意的是，**中间过程测到的 +0.66% 是假象**——那时 backend 还没被真正调用，
链接器把整段代码剥掉了。用 `linkcheck` 扫二进制里的符号可以区分：0 次出现说明
被剥离，真正链接后 resolvo 出现 83 次、rattler 248 次。

shim 侧的红线始终成立：6 个 rattler crate 全部 `optional = true` 且只进
`install` feature，shim 的依赖图恒为 427 行，rattler / resolvo / bzip2 出现次数
恒为 0。
