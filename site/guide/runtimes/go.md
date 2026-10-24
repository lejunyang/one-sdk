# Go

配置参考：[适用的 tool_order 来源](../config/tool-selection#go)、[项目来源诊断](../config/tool-selection#inspect-sources)、[本工具的选择器、托管变量与缓存覆盖](../config/tool-environment#go)。

```bash
osdk install go@1.22
osdk use -g golang@1.23
```

Go 没有 backend 专用 `-o`。osdk 从 go.dev JSON index 选择当前 OS/架构的
`archive` 并验证索引中的 SHA-256；下载候选包括 go.dev、Aliyun 和
golang.google.cn。执行时导出 `GOROOT`，并提供 `go`、`gofmt`。
