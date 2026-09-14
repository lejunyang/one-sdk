# Python 开发工具

osdk 通过 `pypi:` 命名空间从 PyPI 安装 Python 命令行工具，每个工具装在自己的虚拟
环境里。

::: tip 它和其他 backend 有什么本质不同
`github:` 和 `http:` 下载一个归档就完事。Python 工具不是自包含的：它需要一个
解释器、一个虚拟环境，以及一棵可能很深的依赖树。所以 osdk 不自己求解 Python
依赖——那需要实现 PubGrub、PEP 440 版本语义、PEP 425 wheel 标签选择和 PEP 517
源码构建，而做错的后果不是变慢，是**装错包**。

osdk 负责它真正更擅长的部分：索引镜像选择、缓存归置、fail-closed 门禁和安装
布局；求解与安装交给受管子进程。
:::

## 安装

```bash
osdk install pypi:cowsay@6.1
osdk use pypi:cowsay@6.1          # 声明版本，shim 才知道该跑哪个
cowsay -t hello
```

`install` 会自动生成 shim，不需要额外跑 `osdk reshim`。但和其他 backend 一样，
`install` 只是把东西装上，**并不固定版本**——`osdk use` 才写入配置。shim 在没有
选定版本时会明确告诉你该跑什么。

也可以固定到项目里：

```toml
[tools]
"pypi:cowsay" = "6.1"
```

::: warning 版本目前必须写字面量
`latest` 和版本范围需要读取索引，尚未接通，请求它们会得到明确报错而不是静默行为。
`pypi:ruff@0.6.9`、`pypi:cowsay@6.1` 这类字面版本可用。

注意 Python 版本号不是 semver、段数不固定：`6.1` 和 `2026.7.22` 都是完整版本号，
而不是前缀。
:::

## 名字与 extras

项目名按 PEP 503 规范化：`.`、`-`、`_` 的连续出现视为等价，比较不区分大小写。
所以下面几个写法是**同一个工具**，不会各占一个安装目录：

| 你写的 | 规范化后 |
| --- | --- |
| `pypi:Zope.Interface` | `pypi:zope-interface` |
| `pypi:zope_interface` | `pypi:zope-interface` |
| `pypi:Typing-Extensions` | `pypi:typing-extensions` |

PEP 508 extras 用选项表达，而不是写在名字里：

```bash
osdk install "pypi:httpx[extras=socks]@0.27.0"
```

extras 属于**安装身份**的一部分。`pypi:httpx` 和 `pypi:httpx[extras=socks]` 是两个
不同的安装——否则第二个请求会静默复用第一个环境，extra 永远装不上。extras 会
被排序（`[extras=b,a]` 与 `[extras=a,b]` 等价），因为它们是集合、没有优先级；
这一点和 conda 的 `channels` 刻意相反，后者的顺序就是求解优先级。

## 每个工具一个环境，依赖尽量共享

每个工具装在自己的虚拟环境里，这样两个需要互不兼容库版本的 CLI 不会互相打架。

隔离通常意味着每份共享依赖都要各存一份，但在 uv 下不是这样：uv 会把解包后的
文件从自己的缓存**硬链接**进各个环境，于是 N 个环境共享一份字节。Windows x64
实测，两个都装了 `certifi` 的环境：

| 安装器 | 两个环境是否共享 | 每个环境占用 |
| --- | --- | --- |
| uv | 是（两个环境 + 缓存共享同一 inode） | 762,964 B |
| pip | 否，各自完整拷贝 | 6,812,960 B |

同样内容差 8.9 倍。**osdk 不再自己做一层 wheel 级内容存储**：osdk 的 store 键的是
解包后的 SDK 归档，uv 的缓存键的是解包后的 site-packages 树，键语义与生命周期都
不同，重复实现只会得到两个都不完整的缓存。osdk 做的是把 uv 的缓存目录纳入
`<cache>/pkg/uv` 管理，让这份复用归 osdk 管而不是散落在各处。

## uv 与 pip 两条路径

装了 uv（`osdk install pypi:uv`）就走 uv；没装则退回 `python -m venv` 加该环境
自带的 pip。回退会**明确告知**你当前走的是哪条路、代价是什么、以及如何获得更快的
那条，因为静默回退会把可测量的性能与能力差异变成难以归因的困惑。

两条路径不等价，差异如实列出：

| | uv | pip 回退 |
| --- | --- | --- |
| 跨环境共享依赖 | 是 | **否** |
| 解析速度 | 快 | 慢 |
| `--relocatable` | 支持 | **不支持**，会报错而非静默忽略 |
| 环境自带 | pip 需 `--seed` | 总有 pip，从不含 setuptools/wheel |

需要强制走 uv（例如依赖 uv 独有能力）时可以要求 uv-only，此时缺少 uv 会直接
失败而不是降级。

::: tip 检测 uv 用的是「能否启动」而不是「路径存在」
Windows 上一个 `uv.ps1` 或只带 shebang 的文件能通过路径查找，却无法作为进程启动。
所以 osdk 实际运行一次 `uv --version` 来判断。找到但无法启动时，报的是「无法
启动」而不是「未安装」——文件就在那里，后者会把你引向错误的方向。
:::

## 索引与镜像

Python 索引在 `[registries.python]` 里配置，通过 osdk 自己的配置管理，不需要你手改
`pip.conf` 或 `uv.toml`：

```toml
[registries.python]
urls = ["https://pypi.tuna.tsinghua.edu.cn/simple/"]
```

osdk 会用一次匿名探测给候选排序，选最快的可用者。探测不只看 HTTP 200，还会校验
响应形态（PEP 503 的 anchor 或 PEP 691 的 `files` 数组），所以一个返回 200 的门户
欢迎页不会被当成健康镜像。

::: danger 镜像只映射默认索引，永不映射更高优先级的索引
镜像是 PyPI 的完整副本，因此必然包含上游的同名包——包括恶意包。把它排在默认
索引**之上**（uv 的 `--index`/`UV_INDEX`，或任一工具的 `--extra-index-url`）就是
依赖混淆通道。因此 osdk 只会把镜像映射为默认索引（`UV_DEFAULT_INDEX` /
`PIP_INDEX_URL`），并且在类型层面就无法表达「额外索引」。

会关掉校验的参数一律不透传：`--no-verify-hashes`、`--trusted-host`、
`--allow-insecure-host`、`--extra-index-url`、`--index`，裸写和 `--flag=value`
两种写法都会被拒绝。
:::

索引 URL 必须是 HTTPS，且不得内嵌凭据。这比 npm registry 的规则更严（后者仍接受
http），原因是索引是制品哈希的来源：可降级的传输会让攻击者既改制品、又改本该
发现改动的那个哈希。

pip 路径还会额外固定 `PIP_CONFIG_FILE`。uv 设计上不读 `pip.conf`，pip 会读，所以
你系统里遗留的 `pip.conf`（比如指向一个不受信任的索引）本来能悄悄覆盖上面所有
设置。

## 解释器

环境总是构建在 **osdk 管理的解释器**之上，而不是 PATH 上碰巧存在的那个 `python`。
未指定时取已安装的最新版本，也可以点名：

```bash
osdk install "pypi:ruff[python=3.12]@0.6.9"
```

这条约束是刻意的。用 PATH 上的 python 会让环境依赖 osdk 无法控制的机器状态——
很多机器上那个 `python` 是系统自带的旧版本，而 osdk 管的是另一个。同理，uv 路径
会设 `UV_PYTHON_DOWNLOADS=never`，禁止 uv 绕过 osdk 自行下载解释器。

## 暴露哪些命令

osdk 只暴露工具**自己的**命令。环境里的 `python`、`pip`、`activate` 等属于环境
管道，不会成为 shim——否则 `pypi:ruff` 会遮蔽你受管的 `python`。

命令名从磁盘实际内容发现，而不是假设 console script 与项目同名——两者经常不一致。
