# 009 — 裸 `tool` 操作数忽略项目 pin，按 latest 解析

**状态**：已修复 · **严重度**：高 · **实测环境**：Windows x64、PowerShell 7、osdk 0.0.2

## 现象

在一个 `osdk.toml` 里 pin 了 `java = "21.0.12.1+1"` 的项目中：

```
$ osdk exec -t java -- cmd /c echo %JAVA_HOME%
java@26.0.2.1+1 already installed
JAVA_HOME=E:\osdk-data\data\installs\java\26.0.2.1+1
```

于是 Gradle 报：

```
Cannot find a Java installation on your machine matching this tasks requirements:
{languageVersion=21, vendor=any, implementation=vendor-specific}
```

显式写 `-t java@21.0.12.1+1` 才正确。同一台机器上 `osdk current` 却是对的
（`java 21.0.12.1+1 (project ...\osdk.toml)`），这个矛盾是最强的定位线索：**读取
配置的路径是对的，消费配置的路径不对。**

同类表现（同一根因，影响面更大）：

| 命令 | 期望 | 实测 |
| --- | --- | --- |
| `osdk exec -t android-platforms` | pin 的 `android-36-ext19` | `android-37.2` |
| `osdk exec -t android-system-images` | pin 的 `android-35;google_apis;x86_64` | 去下载 `android-37.2-beta3;google_apis_ps16k;x86_64`（2.4 GB 预览镜像） |

最后一行是实际损害最大的一条：一条本应零下载的 `exec` 变成了几个 GB 的意外下载。

## 根因

`crates/osdk-cli/src/commands.rs` 的 `gather_requests()`：命令行上点名了工具时，
只走 `resolve_explicit_request()`，而它最终落到
`ToolRequest::parse()`（`crates/osdk-core/src/version/mod.rs`）：

```rust
pub fn parse(s: &str) -> Result<ToolRequest> {
    let parsed = crate::tool::ToolSpec::parse(s)?;
    Ok(ToolRequest {
        backend: parsed.id.to_string(),
        spec: VersionSpec::parse(parsed.selector().unwrap_or_default()),
        //                       ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
        //                       没有 selector -> ""  -> VersionSpec::Latest
        options: parsed.options.into_map(),
    })
}
```

`VersionSpec::parse("")` 返回 `Latest`。所以 `-t java` 与 `-t java@latest`
**在这一层完全不可区分**，后端于是拿 `Latest` 去 remote index 解析。

`gather_requests()` 的另一条分支（`tools` 为空、从 config pin 收集）本来就会读
`app.ctx.config.tools`，所以「不点名任何工具的 `osdk install`」一直是对的 ——
**只有点名工具时才丢 pin**。这解释了为什么这个缺陷能活这么久：最常用的
`osdk install`（无参）路径是正确的。

## 影响

`install` / `exec` / `lock` / `outdated` / `upgrade` 全部受影响 —— 它们都经
`gather_requests()`。后果按严重度排：

1. **`exec` 注入错误的运行时**，构建随后以「找不到符合要求的工具链」失败，而报错
   指向 Gradle，不指向 osdk，排查方向被带偏。
2. **意外的大体积下载**（上表最后一行）。
3. `lock` 会把 latest 而不是 pin 写进锁文件，把这次漂移固化下来。

## 修复

按**操作数文本**而不是解析后的 spec 判断，然后复用既有的解析顺序：

```rust
fn bind_configured_spec_for_bare_operand_at(
    app: &App, operand: &str, request: &mut ToolRequest, cwd: &Path,
) {
    if requested_spec_literal(operand).is_some() {
        return;                       // 用户显式写了 @xxx，包括 @latest
    }
    ...
    let Some(active) = resolver::resolve_active(
        backend.id(), cwd, &app.ctx.config.tools, backend.idiomatic_files(),
    ) else { return };                // 哪里都没配 -> 保持 Latest
    request.spec = /* parse(active.spec) */;
}
```

两个刻意的选择：

- **判据是操作数文本，不是 `VersionSpec`。** `Latest` 无法区分「没写 selector」和
  「显式写了 `@latest`」，而显式写 `@latest` 的用户是真的想要最新版。
- **复用 `resolve_active`，不自己读 `config.tools`。** 于是 `.tool-versions`、
  idiomatic 版本文件、全局 config 相对 `osdk.toml` 的优先级完全不变，`osdk current`
  与 shim 的答案也保持一致 —— 三者本来就该同源。

## 回归防线

- `bare_named_operand_inherits_the_project_pin` — 全局 pin 与项目 pin 刻意不同，
  所以「取了全局」和「取了项目」是可区分的结果；同时断言 `@latest` 与
  `@17.0.13+11` 不被改写。**变异验证**：把 helper 改成无条件 `return`（即恢复修复前
  行为）→ 变红（实测 `left: Latest, right: Prefix("21.0.12.1+1")`）。
- `bare_named_operand_without_any_pin_still_means_latest` — 锁住「哪里都没配时仍是
  latest」，防止修复把全新机器上的 `osdk install <tool>` 弄坏。

两个测试都用 `..._at(cwd)` 变体而不是 `set_current_dir`：后者是进程级状态，会与
测试二进制里的其他测试竞争。
