# Go

```bash
osdk install go@1.22
osdk use -g golang@1.23
```

Go 没有 backend 专用 `-o`。osdk 从 go.dev JSON index 选择当前 OS/架构的
`archive` 并验证索引中的 SHA-256；下载候选包括 go.dev、Aliyun 和
golang.google.cn。执行时导出 `GOROOT`，并提供 `go`、`gofmt`。
