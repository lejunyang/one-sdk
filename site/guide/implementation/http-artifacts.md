# HTTP 制品 backend 实现

内置 `http:` 动态 namespace 是一个有意保持精简的直接制品 backend。它的合约是
“一个精确语义化版本、一个 HTTPS URL 模板、一个必填 SHA-256，以及可选的安全布局
投影”，而不是 Release 服务、包 Registry 或通用认证下载器。用户语法见
[直接 HTTPS 制品](../http-artifacts)。

## 注册、解析与版本解析

[`dynamic.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/backend/dynamic.rs)
把 `HttpBackendFactory` 与其他内置动态 namespace 一起注册。完整 URL 模板就是 namespace
subject，因此规范 backend ID 形如 `http:https://host/tool-{version}.zip`。

[`tool.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/tool.rs)
定义 namespace schema。公开选项只有 `sha256`、`kind`、`bin`/`bins`、`subdir`、
`rename` 和 `strip-components`；用户请求不能注入内部 `__osdk_*` 重放字段。所有公开
选项都参与动态安装身份。`bin` 规范化为 `bins`，列表排序并去重，类型和摘要转成小写，
文件系统路径规范化为安全的斜杠分隔相对路径，并拒绝 Windows 保留名。

模板校验采用 fail-closed 策略：

- 输入最多 4096 个字符，不得有首尾/内嵌空白、控制字符、反斜杠或 `@`；
- 必须解析为带 host 和 path 的绝对 HTTPS URL，不得包含 userinfo、query 或 fragment；
- 解析后的 URL 必须保持完全相同的规范拼写，IP 字面量必须通过实现采用的保守公网地址策略；
- 允许一到八个 placeholder，每个都必须恰好是 `{version}`，并且只能位于 path；
- 渲染用的 selector 必须是最长 128 字符的精确语义化版本，只能包含 ASCII 字母数字和 `.`、`-`、`_`、`+`。

因此
[`HttpBackend::resolve_version`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/backend/http.rs)
不需要 metadata 请求，直接接受精确 selector 与已校验选项。`list_remote_versions` 始终
报错。该 backend 没有默认 source 或 probe URL，因此不参与 source 排序、`--source` 或
source 故障转移。

## 制品选择与身份

没有重放 metadata 时，backend 把精确版本替换到每个 `{version}`，再次校验渲染 URL，
从 path 最后一段读取安全文件名，并要求配置 64 位十六进制 SHA-256。`kind` 识别
`tar.gz`、`tar.xz`、`zip` 和 `file`；省略时识别三种归档后缀，其他安全文件名按裸文件
处理；`tar.zst` 会被明确拒绝。归档 option set 只有在规范 `bins` 至少包含一项时才合法
（`bin` 会规范化到同一字段）。

[`InstallIdentity`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/tool.rs)
包含规范 backend ID、精确版本、host 平台、隔离 scope、全部规范公开选项，以及所选
制品文件名、`sha256:<hex>` 和实际制品 URL 的 domain-separated BLAKE3 hash material。
生成的 `b3-v2:` install ID 让不同 URL、布局和摘要变体使用不同安装根。内部重放字段不
进入公开 option，但锁定 URL 仍通过该 material hash 绑定；完整实际 URL 保存在 receipt
和 lock 中。

## 网络与缓存边界

[`http.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/backend/http.rs)
只在解析 artifact host 后构造专用 client。DNS 在 blocking task 中运行，timeout 为
10 秒。IP 字面量和 DNS 返回的每个地址都使用同一套保守公网地址 predicate；一个禁止
地址就会拒绝整组结果。接受的地址经过排序、去重，再通过 `resolve_to_addrs` 固定；
`.no_proxy()` 禁用环境和系统 proxy。这会关闭通过 proxy 或 DNS rebinding 访问本地、
link-local、metadata service、documentation、transition 等被拒地址范围的路径。

client 的 connect timeout 为 15 秒，HTTP 请求 timeout 为 10 分钟，连接池 idle timeout
为 30 秒。请求 timeout 不是端到端安装 deadline：DNS 有独立上限，本地校验、归档扫描、
解压、复制和发布不在其中。redirect 必须保持 HTTPS，且 host 与有效端口和原始 origin
相同；credentials、query、fragment、loop 和 origin 变化都会被拒绝；已有十个 prior URL
时也会拒绝下一次 redirect。由于 host 不能变化，固定地址集合始终有效。backend 没有请求
header 或凭据选项。

下载 cache path 包含完整安装身份与文件名，所以不同 URL、checksum 或布局不会仅因 URL
文件名相同而共享未经校验的字节。cache 条目必须是最大 512 MiB 的普通非 symlink 文件。
`prepare_cached_artifact` 始终重新计算 SHA-256。有效 offline hit 不查询 DNS；offline miss
或 mismatch 会失败。online 模式下，已有普通且未超限 cache 条目摘要错误时，backend 会
删除它、恰好重新下载一次并校验替代文件一次；再次 mismatch 后不会循环或再次获取。
超限、symlink 或非普通条目直接失败，不会被自动驱逐。

`download_bounded` 会拒绝超过 512 MiB 的 `Content-Length`，同时独立统计流式字节并检查
溢出。它写入进程专属 partial 文件，成功 `sync_all` 后才 rename；失败会删除 partial。

准备完成后，两个分支都会以强制 offline 模式调用共享 pipeline helper。这是在内部保证
“只获取一次”：物化只能消费刚刚校验的 cache 条目，通用 helper 不会再发起第二个请求；
它不表示在线 HTTP 安装会跳过最初下载。

## 文件与归档物化

`kind=file` 会把准备好的字节复制到 `bin/<name>`；`<name>` 来自 URL 文件名或
`rename`。Unix 上设置可执行权限。Windows 不区分大小写地拒绝 `.cmd` 和 `.bat`，保留
已有 `.exe`，并为其他输出名补 `.exe`；这是名称约束，不校验 PE 字节。文件模式拒绝
归档专属布局选项。

归档模式先预扫描已校验的字节：

- tar 条目必须是安全相对路径，且只能是普通文件或目录；link 与特殊条目都会被拒绝；
- ZIP 条目必须是封闭的相对路径，且不能是 Unix symlink。
- 两种格式最多 16,384 个条目，累计声明展开/未压缩大小最多 2 GiB；目录计入条目数，展开大小上限不是实际解压后磁盘用量。

之后，共享解压 pipeline 使用 copy link mode 物化 `tar.gz`、`tar.xz` 或 ZIP。`subdir`
可选择一个安全的已解压子树。HTTP postprocessor 再拒绝任何残留 symlink；
`strip-components` 每层只能进入一个唯一的非 `.osdk-*` 目录。由于归档校验强制要求
`bin` 或 `bins`，postprocessor 始终把一组显式选择的普通文件复制到安装根的顶层
`bin/`，不存在隐式 archive 命令发现。若规范化后的 source 离开安装根则拒绝；
`rename` 要求归档中恰好选择一个 binary。声明的 Windows 归档 binary 采用同一 `.exe`
输出名规则并拒绝 `.cmd`/`.bat`，保证得到显式的 `.exe` 命名输出 inventory。

最终发布会验证生成的顶层命令 inventory。Unix candidate 必须带 executable bit；Windows
candidate 必须有对应 `.exe` 名称。空 inventory 仍作为纵深防御被拒绝。之后才原子写入
`.osdk-install.json` 并最后发布 `.osdk-complete`；任何 finalize 失败都会删除不完整安装根。

## Lock 重放、复用与并发

通用 artifact receipt 记录实际 URL、文件名、SHA-256 与可能存在的验证 evidence；CLI
lock 会另外保存规范公开选项。重放时，内部字段恢复 receipt；HTTP backend 优先使用其中
的 URL、文件名和 checksum，不再渲染模板，并要求锁定 checksum 是规范小写 SHA-256。
Lock 是 metadata，不是制品包，因此冷离线重装仍需要对应安装身份的下载缓存。

每次安装都会取得安装身份专属文件锁。已有完整安装只有在 install ID/根目录关系、普通
manifest、普通 receipt、完成标记、精确 option 与 material 身份、receipt 文件名/checksum、
已记录命令路径以及无 symlink 全部通过校验后才会复用。无效的完整根会 fail closed，
不会复用或覆盖；通过 inventory 查找时也会拒绝歧义。公共动态安装规则位于
[`backend/dynamic.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/backend/dynamic.rs)。

## 有意暂不支持的范围

该精简 backend 当前没有远程版本提取、范围、别名、平台 placeholder、认证请求、自定义
header、签名 query URL、proxy、私有/内网目标、跨源 redirect、mirror、size 声明、checksum 发现、签名、
attestation 或 SHA-256 之外的摘要算法。每个声明只支持一个制品 URL，格式仅限裸文件、
`tar.gz`、`tar.xz` 和 ZIP，且所有安装都是隔离 scope。

[`backend/http.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/backend/http.rs)
中的聚焦测试覆盖严格模板、公网地址拒绝、redirect 拒绝、归档穿越/link/资源上限、锁定
离线文件/归档重放、checksum 失败不发布、Windows 输出名策略，以及同身份并发串行化。
Namespace parser 与 option 交互测试位于
[`tool.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/tool.rs)。
