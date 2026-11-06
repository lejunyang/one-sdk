# `osdk alias`

管理用户定义的版本别名。

```bash
osdk alias set <TOOL> <NAME> <TARGET>
osdk alias list [TOOL]
osdk alias unset <TOOL> <NAME>
```

```bash
osdk alias set node default 20
osdk alias set python work 3.12
osdk alias list node
osdk alias unset node default
```

持久配置是 `[alias.tools]`，见 `reference/configuration/alias.md`。别名可链式展开但禁止环；
它只改变解析，不创建额外安装副本。形如版本号的名称（`20`、`v20`、`20.1`）会被拒绝，
因为别名先于版本匹配展开；`unset` 一个不存在的名称会报错。
