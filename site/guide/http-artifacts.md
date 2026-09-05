# 直接 HTTPS 制品

当一个工具只发布为路径可预测的 HTTPS 文件或归档、又没有专用 osdk backend 时，
可以使用内置 `http:` backend。声明会把 URL 模板、精确版本、SHA-256 摘要和可执行
文件布局放在一起。虽然 namespace 名为 `http`，但它不接受明文 HTTP。

## 必需约束

每个请求都采用以下结构，option block 位于版本选择器之前：

```text
http:https://host/path-{version}[sha256=64_HEX_CHARACTERS,...]@1.2.3
```

以下部分都必填：

- 绝对 `https://` URL 模板，且路径中包含 `{version}`；
- `1.2.3` 这样的精确语义化版本；
- `sha256`，内容是该制品恰好 64 位的十六进制摘要。

`latest`、范围、`1.2` 这样的部分版本，以及 `{os}`、`{arch}` 等其他模板字段都会被
拒绝。摘要对应模板渲染后的精确制品；版本或制品字节变化时必须一起更新。在 Shell 中
请给完整请求加引号，确保方括号等字符按原样传入。

## 安装单个可执行文件

单文件使用 `kind=file`。`rename` 可省略；省略时，URL 路径最后一段成为命令名。
执行前请把示例摘要替换为发布方提供的精确文件摘要。

```bash
osdk install \
  'http:https://downloads.example.com/acme-{version}[sha256=0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef,kind=file,rename=acme]@1.2.3'
```

文件会安装到该身份的 `bin/` 目录，并在 Unix 上设置为可执行。Windows 会拒绝
`.cmd` 和 `.bat` 名称，保留已有 `.exe`，并为其他最终名称补 `.exe`。这只是文件名
策略，并不检查 PE 文件内容。文件制品不接受 `bin`、`bins`、`subdir` 或
`strip-components`。

## 安装归档

归档类型支持 `tar.gz`、`tar.xz` 和 `zip`。每个归档请求都必须用 `bin` 或 `bins`
声明至少一个可执行文件。下面的例子假定归档只有一个顶层 `acme-1.2.3/` 目录，其中
包含 `bin/acme`：

```bash
osdk install \
  'http:https://downloads.example.com/acme-{version}.tar.gz[sha256=0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef,kind=tar.gz,strip-components=1,bin=bin/acme,rename=acme]@1.2.3'
```

简单安装时，`.tar.gz`、`.tar.xz` 或 `.zip` 文件名可以推断 `kind`。使用归档专属
布局选项时请显式声明 `kind`，避免校验阶段把未知制品当成文件。

| 选项 | 含义 |
| --- | --- |
| `sha256=HEX` | 下载字节的 SHA-256，必填；输入会规范化为小写 |
| `kind=tar.gz\|tar.xz\|zip\|file` | 制品类型；省略时识别归档后缀，否则按文件处理 |
| `bin=PATH` | 把归档内一个安全相对路径的可执行文件复制到安装的 `bin/` 目录 |
| `bins=P1,P2` | 复制多个归档可执行文件；与 `bin` 互斥 |
| `subdir=PATH` | 解压后只物化这个安全相对目录 |
| `strip-components=N` | 解析 `bin`/`bins` 前，逐层进入 `N` 个唯一包装目录 |
| `rename=NAME` | 重命名单文件制品或归档中唯一选中的可执行文件 |

`strip-components` 有意比 tar 同名选项更严格：每一层都必须恰好只有一个非 osdk
子项，而且该子项必须是目录；它不会分别删除每个归档成员的若干路径段。同时设置
`subdir` 时，会先物化该子树，再在其中执行唯一目录下钻。

归档请求不设置 `bin`/`bins` 会在安装前被拒绝，因此 executable inventory 始终是显式
的：每个选中的普通文件都会复制到安装根顶层 `bin/`。Unix 上会把复制后的输出设为
可执行；Windows 中，每个选中输出经过输出名规则后必须以 `.exe` 命名。归档使用
`rename` 时必须只选择一个 binary。

## 在 `osdk.toml` 中声明

完整动态 backend ID 含有特殊字符，必须用引号包住。结构化写法可以清晰分开精确版本
和制品选项：

```toml
[tools."http:https://downloads.example.com/acme-{version}.tar.gz"]
version = "1.2.3"
sha256 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
kind = "tar.gz"
strip-components = "1"
bin = "bin/acme"
rename = "acme"
```

需要多个命令时，把 `bins` 写成 TOML 字符串数组，并省略 `rename`：

```toml
bins = ["bin/acme", "bin/acmectl"]
```

所有公开 HTTP 选项都属于安装身份。修改摘要、制品类型、所选子树、binary 或重命名
会产生另一个隔离安装，不会静默复用不兼容的现有安装。

## 锁定与离线重放

项目中可以采用以下流程：

```bash
# 下载并校验 osdk.toml 声明的精确制品。
osdk install

# 记录精确版本、公开选项与已安装制品 receipt。
osdk lock

# 之后不访问网络，消费当前平台 lock。
osdk --offline install
```

成功安装后重新生成 lock，可以保存实际制品 URL、文件名和 `sha256:` checksum。重放
lock 时，这些字段优先于再次渲染模板。离线安装绝不回退到网络：它只复用该精确安装
身份对应的缓存，并重新验证 SHA-256；缺少这些字节时会以 offline artifact cache miss
明确失败。

已有 cache 条目在使用前会先校验。offline 模式发现 checksum 不匹配时立即失败，且不会
查询 DNS。online 模式遇到 checksum 错误的普通、未超限 cache 文件时，会删除它、只重新
下载一次并只校验一次替代文件，不存在进一步 HTTP 重试循环。超限、symlink 或非普通
cache 条目会 fail closed，不会自动修复。

Lock 不包含制品字节。机器需要离线重建安装时，应保留 osdk 下载缓存；
`osdk cache clean` 会删除该缓存。已经完成且身份匹配的安装仍可直接复用，不必再次下载。
无参数 `install` 如何消费当前平台 lock，见[可复现锁文件](./lockfiles)。

## 安全边界

该 backend 要求规范 URL 拼写，并拒绝 URL 凭据、query、fragment、非 HTTPS URL、
非公网 IP 字面量，以及跨源或 HTTPS 降级到 HTTP 的 redirect。联网请求前，它最多用
10 秒解析 host；只要 DNS 结果中有任一地址不符合其保守的公网地址策略，就拒绝整组
结果，否则把 client 固定到这些地址。系统和环境代理均禁用。同 host、同有效端口的
redirect 因而继续使用已固定的地址集合，不会解析新 host。

连接 timeout 是 15 秒，HTTP 请求 timeout 是 10 分钟；后者覆盖请求和 body 传输，
不是包含 DNS、本地 checksum、解压、复制和发布在内的总安装 deadline。下载或缓存制品
最大为 512 MiB，同时约束 `Content-Length` 与实际流式读取字节数；不完整下载不会发布
为 cache 条目。

它始终校验必填 SHA-256，包括缓存中的字节。归档最多包含 16,384 个条目，累计声明
展开/未压缩大小最多为 2 GiB。2 GiB 上限来自条目 metadata，而不是实际解压后磁盘占用；
目录也计入条目数。归档路径必须留在解压根内；tar link 及非普通文件/目录条目会被拒绝，
ZIP symlink 也会被拒绝；发布前还会再次扫描物化后的安装，拒绝 symlink。配置的可执行
文件必须解析为安装根内部的普通文件。最终没有发现 executable 时，发布失败并删除不完整
安装根。

这些检查不能证明摘要由谁发布。请从你信任且经过认证的渠道获取 SHA-256，并在信任
项目配置前审阅它。该 backend 当前不支持签名或 GitHub Artifact Attestation。

## 当前限制

- 没有远程版本列表或自动更新发现；必须使用一个精确语义化版本。
- 只支持 `{version}` 插值，同一声明不能按操作系统、架构或 libc 选择不同制品。
- 不支持认证 URL、签名 query URL、自定义 header 或跨源 CDN redirect。
- 会主动忽略环境和系统 HTTP proxy；目标必须能直接解析到公网地址。
- 每个声明只有一个 URL，没有 mirror/source 故障转移。
- 只支持裸文件、`tar.gz`、`tar.xz` 与 ZIP；不支持 `tar.zst`、安装器或磁盘镜像。
- 该 backend 只接受 SHA-256，且不能省略。
- 所有安装都使用 osdk 的隔离 scope。
- tool id 按段展开为目录，但只展开前五段；更长的 id 保留前四段，其余折叠为一个 `~t1~` 摘要。这样可以约束由 URL 派生的 id 的安装树深度（URL 有多少段由远端服务器决定），保证每个 receipt 都在 inventory 扫描范围内。

解析、身份、redirect、缓存、解压和发布的实现细节见
[HTTP 制品 backend 实现](./implementation/http-artifacts)。
