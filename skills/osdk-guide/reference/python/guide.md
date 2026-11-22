# `osdk python`

查找可用 Python 解释器。

```bash
osdk python find [REQUEST]
```

搜索受管安装、PATH 与系统层；请求可为普通版本、PyPy 或 free-threaded 变体，例如：

```bash
osdk python find
osdk python find 3.12
osdk python find pypy-3.11
osdk python find 3.14+freethreaded
```

命令只查询，不安装。Python catalog 覆盖写在 `[settings.python]`；项目 Python 依赖用
`[deps.uv]` 或 `[deps.pip-requirements]`，PyPI CLI 工具则用 `pypi:<project>`。

`pypi:` 查版本和安装优先使用 pip 或 uv 配置的单个索引。osdk 匿名查询版本，索引无法确定或查询需要登录时明确报错。没有配置原生源时，使用 `[registries.python].urls` 或内置候选。
