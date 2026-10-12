# Go

```bash
osdk install go@1.22
osdk use -g golang@1.23
```

Go has no backend-specific `-o`. osdk selects the host OS/architecture archive
from the go.dev JSON index and verifies its SHA-256. Download candidates include
go.dev, Aliyun, and golang.google.cn. Execution exports `GOROOT` and exposes
`go` and `gofmt`.
