# `[aliases]`

结构是“工具 → 别名 → 版本请求”：

```toml
[aliases]
node = { default = "20", lts = "20" }
python = { work = "3.12" }
```

别名可以链式展开，但循环会被拒绝。命令等价物：

```bash
osdk alias set node default 20
osdk alias list node
osdk alias unset node default
```

仅声明 `[aliases]` 不需要 trust。别名影响版本解析，不复制安装，也不创建另一份工具。
