# Python

`python@3.14` 是默认 CPython 的简写。完整请求为
`python@IMPLEMENTATION-VERSION+VARIANT`：

```bash
osdk install python@3.14
osdk install python@cpython-3.14+freethreaded
osdk install python@cpython-3.14+debug
osdk install python@cpython-3.14+freethreaded+debug
osdk install python@pypy-3.11
osdk install python@graalpy-3.12
osdk install python@pyodide-3.14
```

| 实现 | 支持的变体 | identity 示例 |
| --- | --- | --- |
| `cpython` | `default`、`freethreaded`、`debug`、`freethreaded+debug` | `3.14.7`、`cpython-3.14.7+debug` |
| `pypy` | `default` | `pypy-3.11.x` |
| `graalpy` | `default` | `graalpy-3.12.x` |
| `pyodide` | `default` | `pyodide-3.14.x` |

| 选项 | 值 | 作用 |
| --- | --- | --- |
| `variant` | 上表中的变体 | 与请求中的 `+VARIANT` 等价；显式选项优先 |
| `tag` | python-build-standalone 发布日期，如 `20240224` | 为经典 CPython 下载固定历史 PBS release |

实现、精确 Python 版本、变体和 catalog artifact 都会进入 lock，不同 identity 可
并存。普通 CPython 使用内置 python-build-standalone 版本索引；多实现、变体和预发布
使用内置的已校验 catalog。自定义 catalog 必须同时指定内容摘要：

```toml
[settings.python]
catalog_url = "/approved/python-catalog.json"
catalog_sha256 = "0123456789abcdef..."
```

`catalog_url` 可为 HTTP(S) 或本地路径。只有 SHA-256、schema 与每个 artifact
checksum 全部有效才更新 last-good；刷新失败会尝试 last-good，再回退内置 catalog。
预发布策略见[预发布版本](../sources/offline#预发布版本)。

经典 CPython 的同一 release tag 会合并所有可达 source 的 `SHA256SUMS`，排序靠前来源
的同名条目优先；因此首个镜像缺少某个平台的归档时，后续来源仍可补齐并进入下载回退。

查找解释器：

```text
osdk python find [REQUEST]
```

```bash
osdk python find
osdk python find pypy-3.11
osdk python find 3.14+freethreaded
```

managed 结果按可选请求筛选；随后仍扫描 `PATH` 与系统候选并去重，按
`managed`、`PATH`、`system` 标记输出。完全找不到时返回错误。
