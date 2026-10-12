# 控制哪些命令进 PATH

一个安装往往带来比你想要的更多可执行文件：conda prefix 装着整个依赖闭包，Android
NDK 有 172 个可执行文件。`[settings.shims]` 决定其中哪些生成 shim、进入 PATH。

不生成 shim 不等于没装：文件仍在 install 目录里，`osdk exec` 和激活的 shell 里照样
可用。

## 三个列表

| 设置 | 语义 | 作用域 |
| --- | --- | --- |
| `include` | **白名单**：非空时，未列出的名字一律不生成 | 全体工具 |
| `expose` | **增量**：额外放行默认被挡下的名字，不影响其他任何工具 | 全体工具 |
| `exclude` | 排除，最后生效，可修剪前两者的结果 | 全体工具 |

判定顺序：`include` 决定候选集合 → `expose` 追加 → `exclude` 最后剪除。

`expose` 会盖过别处的窄 `include`。两者命中同一个名字时，一边说「要这个」、一边说
「不在名单里」，让 `include` 赢就等于显式要求被静默丢弃。

## `include` 是全局白名单，不是「加回一个」

这是最容易出错的一点：

```bash
# 危险：想取回 make，结果 cargo / go / node 全部失去 shim
osdk config set shims.include "conda:m2-base:make"
```

`include` 一旦非空就成为对**全体工具**生效的白名单，列出 1 个名字等于声明「其余都不
要」。实测这一条命令把 646 个 shim 变成了 0。

要取回被挡下的命令，用 `expose`：

```bash
# 安全：只加不减
osdk config set shims.expose "conda:m2-base:make"
```

`include` 本身没有问题，它表达的是「只要这几个」——当你真的想大幅收窄时才用它，并且
优先用下面的按工具形式。

## 按工具覆盖

三个列表都可以限定到单个工具，键的形式是 `shims.<tool>.<字段>`：

```bash
osdk config set shims.conda:m2-base.expose  "make,sh,bash,tr,awk"
osdk config set shims.android-ndk.include   "clang,clang++,llvm-strip"
osdk config set shims.conda:m2-base.exclude "ls,test"

osdk config get   shims.conda:m2-base.expose
osdk config unset shims.conda:m2-base.expose
```

写进 TOML 的形态：

```toml
[settings.shims.tools."conda:m2-base"]
expose = ["make", "sh", "bash", "tr", "awk"]
```

按工具限定让 `include` 变得安全：作用域收窄之后，`android-ndk` 下的 `include` 只能
影响 NDK 自己的命令，不可能收走 `cargo`。**需要收窄某个工具时，优先用这个形式，而不
是全局 `include`。**

覆盖是**逐字段**的：写了哪个字段就覆盖哪个，没写的继承全局列表。因此「只调 expose」
不会顺手清掉全局的 exclude。`config get` 对未指定的字段显示 `inherit`，以区别于显式
设成空列表。

工具 key 按 backend id **精确匹配**，不支持通配；跨工具的模式请用全局列表。

## 模式语法

三个列表的元素都是模式，规则一致：

| 写法 | 匹配对象 | 例 |
| --- | --- | --- |
| 不含 `:` | 命令名 | `make`、`clang*` |
| 含 `:` | `<backend>:<命令名>` | `conda:m2-base:make`、`android-ndk:*` |

- `*` 匹配任意长度，`?` 匹配单个字符；
- 大小写不敏感（Windows 可执行文件本就如此）；
- 匹配整个名字，不是子串。

在按工具的列表里，两种写法都可用，但既然作用域已经限定，直接写命令名更清楚。

## 元包：默认一个 shim 都没有

`conda:m2-base` 这类元包自身不安装任何命令——prefix 里的东西都属于它拉进来的包，所以
默认不生成任何 shim。这是刻意的：否则 msys 版的 `ls`、`test`、`sort` 会盖住 Windows
同名命令。

需要其中几个时用 `expose` 点名：

```bash
osdk config set shims.conda:m2-base.expose "make,sh,bash,tr,awk,grep,printf"
osdk reshim
```

只暴露真正用到的命令，未列出的不会干扰系统命令。

## 改完要 reshim

`config set` 只改配置，不动磁盘上的 shim。改完执行：

```bash
osdk reshim
```

`osdk where --bins <tool>` 可以核对结果，它分别列出 `published`（生成 shim）与
`withheld`（挡下）两组。

::: tip 这些设置不需要信任
`settings.shims` 只决定哪些命令生成 shim，既不执行代码也不改变下载来源，因此写进项目
`osdk.toml` 不会要求信任。需要信任的键见[哪些键需要信任](../config/trust#哪些键需要信任)。
:::
