# `osdk list`

列出本机已安装版本。

```bash
osdk list [TOOL]
```

- 别名：`osdk ls`。
- 不带参数列出全部；带工具名只列该 backend/动态工具。
- 这是本地只读查询，不访问远端；可安装版本使用 `list-remote`。

```bash
osdk list
osdk list node
osdk list npm:prettier
```

安装记录来自 osdk data/install 目录，不等同于当前目录最终选择；后者用 `osdk current`。
