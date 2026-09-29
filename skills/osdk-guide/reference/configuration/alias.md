# `[alias.tools]`

`[alias]` 是别名能力的命名空间；当前已实现的 `tools` 类别结构是
“工具 → 别名 → 版本请求”：

```toml
[alias.tools]
node = { default = "20", lts = "20" }
python = { work = "3.12" }
```

别名可以链式展开，但循环会被拒绝。命令等价物：

```bash
osdk alias set node default 20
osdk alias list node
osdk alias unset node default
```

仅声明 `[alias.tools]` 不需要 trust。别名会在 install/use/uninstall、激活、shim 和
全局 npm 工具等需要解析版本请求的路径上展开；它不重命名可执行文件、不复制安装，
也不创建另一份工具。`alias.shell` 等类别尚未实现，未知类别会明确报错。
