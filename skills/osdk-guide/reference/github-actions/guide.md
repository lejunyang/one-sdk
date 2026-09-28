# GitHub Actions

仓库根目录提供 composite Action：

```yaml
steps:
  - uses: actions/checkout@v4
  - uses: lejunyang/one-sdk@main
  - run: osdk run ci
```

生产 workflow 应将 Action 固定到 Release tag 或完整 commit SHA。默认会安装并校验 osdk、
恢复 osdk 的 data/cache、执行 `osdk install --no-deps` 和
`osdk deps --frozen --no-install-tools`，再把项目工具环境导出给后续 step。

常用输入：`version`、`working-directory`、`cache`、`cache-key`、`install-tools`、
`install-deps`、`frozen`、`allow-deps-tool-install`、`jobs`、`source-mode`、`offline`、
`require-checksums`、`attestations`。完整语义和缓存边界见站点的 GitHub Actions 指南。

缓存的是工具安装、CAS、下载与包管理器缓存，不直接缓存项目的 `node_modules` / `.venv`。
key 按 OS、架构、实际 osdk 版本、项目 manifest/lock 摘要隔离；不同 lock 之间可按前缀
复用旧内容，但最终身份仍由 osdk 校验。
