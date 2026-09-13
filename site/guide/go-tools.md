# Go 开发工具

osdk 通过 `go:` 命名空间安装 Go command package。它与裸 `go` runtime 相互独立：
`go@1.24.6` 选择编译工具链，而 `go:golang.org/x/tools/gopls@0.20.0` 使用该精确的
受管工具链构建并安装命令。

## 安装并选择工具

先选择受管 Go 版本线，再添加 command package：

```bash
osdk use go@1.24
osdk use go:golang.org/x/tools/gopls@0.20.0
eval "$(osdk activate bash)"
gopls version
```

也可以只安装和临时执行，不修改项目配置：

```bash
osdk install go@1.24.6 go:golang.org/x/tools/gopls@0.20.0
osdk exec --tool go@1.24.6 \
  --tool go:golang.org/x/tools/gopls@0.20.0 -- gopls version
```

每个 `go:` 工具都要求恰好一个受管 Go 请求。显式请求优先；否则 osdk 会注入当前项目或
配置中激活的 `go` 选择，先安装 runtime，再把解析后的精确版本绑定到工具。不会使用
`PATH` 中环境自带的 `go` 作为 provider。

需要用户级选择时，把 runtime 和工具都写入全局配置：

```bash
osdk use --global go@1.24
osdk use --global go:golang.org/x/tools/gopls@0.20.0
```

工具仍安装在 osdk 管理的指纹化根目录中；global 表示选择被写入用户配置与用户 lock，
而不是当前项目。

## 路径与 selector

subject 是规范的 module 或嵌套 command-package 路径：

```text
go:<module-or-command-path>[@SELECTOR]
```

module host 必须是小写 DNS 文本，后续路径大小写保持不变。空 component、`.`/`..`、
隐藏 component、空白、URL 语法，以及版本前导 `v` 都会被拒绝。支持精确语义版本（包括
规范预发布版本）、规范伪版本、`latest` 和数字前缀：

| 请求 | 选择结果 |
| --- | --- |
| `go:golang.org/x/tools/gopls` 或 `@latest` | Proxy 的 `@latest` 结果 |
| `go:golang.org/x/tools/gopls@0` | Proxy 列表中最新稳定的 `0.x` |
| `go:golang.org/x/tools/gopls@0.20` | 最新稳定的 `0.20.x` |
| `go:golang.org/x/tools/gopls@0.20.0` | 精确语义版本 |
| `go:example.com/acme/tool@0.0.0-20240801123456-0123456789ab` | 精确规范 Go 伪版本 |

对于嵌套 command path，osdk 从最长候选路径开始查找 module，并记录第一个被 proxy metadata
证明存在的 module root；完整 command path 仍传给 `go install`。所选 module 是否实际包含
该 command package，最终由 provider 安装阶段验证。

## 构建选项

两个公开选项都会参与安装身份：

| 选项 | 行为 |
| --- | --- |
| `tags` | 逗号分隔的 Go build tags；规范化、排序并去重 |
| `env` | 分号分隔、白名单约束的构建环境赋值 |

构建环境白名单包括 `CGO_ENABLED=0`、`GOAMD64`、`GO386`、`GOARM`、`GOMIPS`、
`GOMIPS64` 和有长度限制的 `GOEXPERIMENT`。`CGO_ENABLED=1` 会被拒绝，因为 osdk 尚未
选择并绑定 C compiler/linker 身份。`GOPROXY`、`GONOSUMDB`、`GOMODCACHE`、`GOCACHE`、
`GOBIN`、`PATH` 等凭据或网络/缓存覆盖不能通过该选项传入。

```bash
osdk use 'go:example.com/acme/tool[tags=netgo,env=CGO_ENABLED=0]@1.2.3'
```

## Proxy 选择

内置候选为 `https://proxy.golang.org` 与 `https://goproxy.cn`。它们遵循通用的 `auto`、
`ordered` 和 pin 来源策略；auto 会先探测并缓存排序，再解析 metadata。精确版本仍通过
选中 proxy 的 `.info` endpoint 验证。

自定义 Go proxy 必须是规范的 HTTPS origin 或 path，不得包含凭据、query、fragment 或末尾
斜杠。存在 custom source 且未 pin 时只使用这些 candidate，避免把私有 module path 探测到
公开默认 proxy。自定义 source header 会被拒绝，因为后续原生 `go install` 无法维持 osdk
的逐请求凭据转发边界。选中的 proxy 会作为唯一 `GOPROXY` 条目传入，因此 provider 启动后
不会暗中改试另一个来源。

以上都属于 `go:<module>` 这类**包安装**路径。直接运行受管 `go`（`go build` /
`go test`）拉取依赖是另一条通道：`[sources.go]` 只决定 Go 工具链归档的下载来源，
模块下载由 go 命令自己按 `GOPROXY` 进行，因此镜像归档并不会让模块也走镜像。

受管 go 的 `exec_env` 会注入一份排好序的 `GOPROXY`，候选集在配置里用 `go-modules`
这个独立名字管理（默认为 `proxy.golang.org`、`goproxy.cn`、`mirrors.aliyun.com/goproxy`，
末尾追加 `direct`）。这里只做配置层排序、不做网络探测：这段代码在 shim 里每次命令
调用都会执行，一次探测往返会直接计入交互延迟；实时测速仍留在安装路径。

条目之间用 `|` 而非 `,` 连接。分隔符本身就是回退策略：逗号只在 404/410 时继续尝试
下一个，连接超时被视为终止错误，于是首个不可达的 proxy 就会让整条链失效 —— 而这
恰好是需要镜像的场景。逗号带来的 gatekeeper 语义（私有 proxy 返回 403 时停止查找，
避免把 module path 泄漏给下一个来源）在这里不适用，因为这组候选全是同一批公开模块
的公开镜像。用户自己设过 `GOPROXY`（含 `off`、`direct`）时不会被覆盖。

## 隔离与激活

osdk 在清空的环境中无 shell 执行一次 `go install <command>@v<version>`。它强制使用选中的
受管 `GOROOT`、分阶段 `GOBIN`、私有 home/GOPATH/temp、osdk 控制的共享 module/build cache、
`GOENV=off` 和 `GOTOOLCHAIN=local`。`GOSUMDB=off` 避免访问无关 checksum service，因此
source trust 由选中的 proxy 承担。

只有 stage 中 `bin` 下的普通可执行文件会发布；名称、大小和 SHA-256 都会记录。activation
和 shim 只暴露通过校验的命令。版本、tags、构建环境、proxy、module root、受管 Go 版本或
受管 runtime 内容任一不同，都会形成不同安装身份。

## Lock 与离线行为

`osdk.lock` schema 4 只保存紧凑 native metadata：精确 Go runtime 版本、`version-only`
重放类型、选中的 proxy 和发现的 module root，同时保留公开的 `tags`/`env`，并要求同一
平台表中存在匹配的精确 `go` 条目。它不会把 `go.sum` 或完整传递 module graph 写入
`osdk.lock`。

因此，身份完全匹配的既有完整安装可以离线校验并复用；即使共享 Go cache 恰好含有部分
依赖，也不支持全新离线构建或修复。

## 生命周期命令

```bash
osdk current go:golang.org/x/tools/gopls
osdk list go:golang.org/x/tools/gopls
osdk list-remote go:golang.org/x/tools/gopls
osdk where go:golang.org/x/tools/gopls@0.20.0
osdk outdated go:golang.org/x/tools/gopls
osdk upgrade go:golang.org/x/tools/gopls
osdk --yes uninstall go:golang.org/x/tools/gopls@0.20.0
osdk reshim
```

切换或删除选中的 Go runtime 会使依赖它的 Go 工具无法复用。如果存在多个来源/runtime
身份不同但其余条件相同的安装，应通过 `osdk.lock` 选择精确身份，避免歧义激活。
`--global` 当前只改变选择/lock 作用域；`where --global` 与 `uninstall --global` 仍是 npm
专属操作。对于当前环境选中的 Go 工具，请使用普通的精确 `where`/`uninstall` 形式。

runtime 身份、stage、provider 和 lock schema 的细节见
[Go 开发工具实现](./implementation/go-tools)。
