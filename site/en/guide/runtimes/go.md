# Go

Configuration reference: [applicable tool_order sources](../config/tool-selection#go), [project source diagnostics](../config/tool-selection#inspect-sources), and [this tool’s selectors, managed variables and cache overrides](../config/tool-environment#go).

```bash
osdk install go@1.22
osdk use -g golang@1.23
```

Go has no backend-specific `-o`. osdk selects the host OS/architecture archive
from the go.dev JSON index and verifies its SHA-256. Download candidates include
go.dev, Aliyun, and golang.google.cn. Execution exports `GOROOT` and exposes
`go` and `gofmt`.
