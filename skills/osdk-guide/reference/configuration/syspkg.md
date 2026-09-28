# `[syspkg]`

声明宿主系统需要的包。声明不会安装；`osdk pkg status` 只读，只有
`osdk pkg apply` 会修改系统。

```toml
[syspkg.packages]
"apt:build-essential" = "latest"
"dnf:gcc" = { version = "latest", os = "linux" }
"pacman:gcc" = { version = "latest", arch = ["x86_64", "arm64"] }
"apk:build-base" = "latest"
"winget:Git.Git" = { version = "latest", os = "windows" }
"brew:git" = { version = "latest", os = "macos" }
```

键必须是 `<manager>:<package>`；manager 支持范围取决于宿主。osdk 不做跨 manager
包名映射，每个平台应声明真实包名。`version` 是安装时愿望，不是可复现 lock。

对象支持 `version`、`os`、`arch`；`os` / `arch` 可为单值或数组。只有
`osdk pkg apply` 会执行系统命令，也只有它要求 trust；条目在 `os`/`arch` 不匹配，
或其管理器在本机不存在（如 Windows 上的 apt、Linux 上的 winget）时都不算适用。
`pkg status`/`plan`/`doctor` 只读，不要求信任。容器运行时不适合放进这里：安装包不等于
守护进程、用户组和存储驱动已配置。
