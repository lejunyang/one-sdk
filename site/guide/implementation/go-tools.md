# Go 开发工具实现

本页描述用户侧 [Go 开发工具](../go-tools) 背后的
`go:<module-or-command-path>` 动态 backend。Go package provider 与裸 `go` runtime
backend 明确分离。

## 规范身份与版本选择

中央 `GO_SCHEMA` 先校验 module/command path、selector、build tags 和白名单构建环境，
动态 registry 再创建 `GoPackageBackend`。host 必须为小写 DNS 文本；后续 path component
保留大小写，并拒绝 URL 分隔符、路径穿越、隐藏 component、控制字符与空白。selector
支持 `latest`、一到两段数字前缀、精确语义版本与规范 Go 伪版本。

`latest` 请求使用候选 module 的 `@latest` endpoint；前缀请求读取有界的 `@v/list`；
精确请求必须由匹配的 `.info` 响应证明。module proxy 转义遵循 Go 的大写字母 `!x` 规则。
嵌套 command 会优先尝试最长 module 候选，并在每个候选内按来源排序依次尝试，避免较短
module 因 proxy 更快而遮蔽实际存在的较长 module。

默认 proxy 是 `proxy.golang.org` 与 `goproxy.cn`。来源选择复用 osdk 的探测排名/cache 或
配置的 order/pin。版本列表最多 4 MiB，精确/latest metadata 最多 64 KiB，deadline 为 30 秒；
只有全部在线候选失败后才接受 stale metadata。Proxy 必须是无凭据、query、fragment 和末尾
斜杠的规范 HTTPS URL；loopback HTTP 仅供测试。配置 custom source 且未 pin 时，Go 解析会
刻意只保留这些 custom candidate，避免把私有 module path 探测到公开默认 proxy。自定义
header 会被拒绝，因为原生 provider 无法保持其 credential scope。

## 精确受管 Go 依赖

当 `go:` 请求没有显式 runtime 时，CLI 编排会注入当前激活/配置的裸 `go` 请求。该 runtime
先于依赖工具安装或解析，随后其精确版本会写入私有 request metadata。全局 `use` 读取用户级
Go 选择，不会误用更近的项目覆盖。

backend 要求完整的 osdk 受管 Go 根，并为构建关键 inventory 计算 `b3-go-v1:` 身份：
`VERSION`、可选 `go.env`、`bin`、`pkg`、`src` 以及可选 `lib`/`misc`。普通文件会被 hash；
symlink 只有在解析到该 runtime 或 osdk store 内时才允许。带锁、原子写入的 receipt 会在
有序 path/size/mtime/symlink-target inventory 不变时复用身份。这是面向 osdk 受管不可变
runtime 的性能 cache，不是 same-user 安全边界；能同时改写 payload bytes、timestamp 和 osdk
状态的进程不在威胁模型内。

## Provider 环境

安装会无 shell 地只调用一次精确受管 `go` executable，并使用空 stdin、1 小时 timeout、
stdout/stderr 各 1 MiB 上限。`CommandSpec::clear_env` 清除环境后，osdk 注入：

```text
HOME, USERPROFILE = <stage>/home
GOROOT             = <精确受管 Go 根>
GOBIN              = <stage>/bin
GOPATH             = <stage>/gopath
GOMODCACHE         = <cache>/pkg/go-mod
GOCACHE            = <cache>/pkg/go-build
GOENV              = off
GOTOOLCHAIN         = local
GOPROXY             = <唯一选中的 proxy>
GONOPROXY           = none
GONOSUMDB           = <发现的 module root>
GOSUMDB             = off
PATH                = <精确受管 Go bin> + 清理后的系统路径
TMPDIR, TEMP, TMP   = <stage>/tmp
GIT_TERMINAL_PROMPT = 0
```

命令为 `go install [-tags TAGS] <command-path>@v<exact-version>`。provider 启动后只允许访问
该 proxy，不附加 `direct` 或 fallback 列表。关闭 checksum database 可避免独立网络/凭据边界，
因此完整性委托给所选 proxy。`CGO_ENABLED=1` 会被拒绝，直到 osdk 能选择 C compiler/linker
并把其身份纳入绑定。

Module/build cache 只在 osdk cache 根中共享；私有 home、GOPATH 与临时数据留在 stage 中，
发布前会删除。这些依赖 cache 都不会进入 osdk archive CAS。

## 发布与校验

`NativeToolLifecycle` 把规范 tool/version/platform、公开 `tags`/`env`、精确 Go 依赖及其
runtime 内容身份、所选 proxy、module root 与 command path 合成 `b3-v2:` install ID。
其锁覆盖候选校验、provider 执行与发布。

provider 成功后，osdk 删除私有 workspace，写入有界 `go-resolution.json`，拒绝 symlink 与
保留 metadata，枚举 `bin` 中的普通 executable 并记录其大小和 SHA-256，再写 native receipt、
动态 inventory，通过 no-replace rename 发布 sibling stage。相邻 metadata seal 绑定发布 metadata。
复用、activation、shim、`where`、list 和 uninstall 都复核相同身份、receipt、seal、binary 内容
与受管 Go 依赖。

## Lock schema 4

Go 工具使用共享的类型化 `native` 子表：

```toml
schema = 4

[platforms.linux-x64.tools.go]
request = "1.24"
version = "1.24.6"

[platforms.linux-x64.tools."go:golang.org/x/tools/gopls"]
request = "0.20"
version = "0.20.0"
options = { tags = "netgo", env = "CGO_ENABLED=0" }

[platforms.linux-x64.tools."go:golang.org/x/tools/gopls".native]
runtime = "go"
runtime_version = "1.24.6"
replay = "version-only"
source = "https://proxy.golang.org"
module = "golang.org/x/tools/gopls"
```

读取时只有在 backend、精确版本、同平台匹配 Go 条目、proxy、module-root 前缀与 replay
类型均通过校验后，才把类型化字段恢复为私有选项。用户注入 `__osdk_*` 会被拒绝，这些 key
也不会进入公开 options 表。schema 1 到 3 无法表达该依赖，会拒绝 `go:` 条目。

`version-only` 是刻意如实描述：lock 只记录顶层解析与来源身份，不记录传递 module graph
或 source payload。因此全新离线安装与修复会在 provider 启动前失败；只有完整且身份精确
匹配的既有安装可离线复用。
