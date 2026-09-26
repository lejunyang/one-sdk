# `osdk uninstall`

卸载一个明确的工具版本。

```bash
osdk uninstall <TOOL[@VERSION]> [-g|--global]
```

- 别名：`osdk rm`。
- `--global` 只支持用户全局 npm 动态包，其他 backend 会拒绝。
- 卸载对象与配置选择是两件事；需要改变项目选择时编辑 `[tools]` 或运行新的 `use`。

```bash
osdk uninstall node@20.11.1
osdk uninstall -g npm:prettier@3
```

删除前确认请求解析到了预期身份。仍被项目 lock/config 引用的工具以后执行 `install`
可能再次被物化。
