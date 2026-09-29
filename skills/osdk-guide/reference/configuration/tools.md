# `[tools]`

声明项目或用户全局要使用的工具。字符串适合仅给版本，对象可同时给 backend 选项和
平台过滤。

```toml
[tools]
node = "20"
python = "3.12"
rust = { version = "1.98.0", components = "rustfmt,clippy", targets = "x86_64-pc-windows-gnu" }
java = { version = "21", distribution = "zulu", package-type = "jdk" }
"npm:prettier" = "3"
"go:golang.org/x/tools/gopls" = { version = "0.20.0", tags = "tools" }
zig = { version = "0.15", when = { os = ["linux", "macos"] } }
```

对象字段：

- `version`：必填字符串。
- `when`：可选 `{ os = ..., arch = ... }`。
- 其余字段：backend 专属选项，等价于命令行 `-o KEY=VALUE`。

同名配置按层级整体替换，不做字段级拼接。被 `when` 排除的工具仍会保留排除原因，
命令点名时会报告平台不匹配。仅声明 `[tools]` 不需要 trust。

`osdk use TOOL@VERSION` 会更新项目 `[tools]` 并同步当前平台 `osdk.lock`；直接手工编辑本段
后运行 `osdk lock`，或用裸 `osdk install` 在当前平台尚无 lock 时安装并记录精确结果。

工具名、动态命名空间、版本选择和常见 backend 选项见
`reference/tool-requests/guide.md`。
