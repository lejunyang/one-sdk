---
layout: home

hero:
  name: osdk
  text: 一站式管理所有语言 SDK
  tagline: 一个跨平台 CLI，统一版本、镜像、缓存、锁文件与项目环境
  image:
    src: /logo.svg
    alt: osdk 标志
  actions:
    - theme: brand
      text: 开始使用
      link: /guide/installation
    - theme: alt
      text: 了解功能
      link: /guide/features
    - theme: alt
      text: GitHub
      link: https://github.com/lejunyang/one-sdk

features:
  - icon: ◈
    title: 一个命令，多种 SDK
    details: 用一致的命令管理 Node.js、Python、Java、Go、Rust、pnpm、Yarn、Deno、Bun 和 GitHub Release 工具。
  - icon: ⧉
    title: 跨版本内容去重
    details: 基于 BLAKE3 的内容寻址存储只保留一份相同文件，并通过硬链接、reflink 或复制安全物化版本。
  - icon: ⇄
    title: 自动选择最快镜像
    details: 探测官方源和权威镜像，按速度选择并在元数据或下载失败时自动切换。
  - icon: ⌁
    title: 可复现的项目环境
    details: 读取项目版本文件并生成按平台分区的 osdk.lock，固定精确版本、下载地址和校验信息。
  - icon: ✓
    title: 完整性与来源验证
    details: 支持上游校验和、严格校验策略，以及 GitHub Artifact Attestations 的 Sigstore 验证。
  - icon: ⬡
    title: 离线优先与共享缓存
    details: 缓存元数据和下载包，并统一 npm、pip、Cargo、Go、Gradle 等下游包缓存。
---

## 三步开始

```bash
# 安装 osdk
curl --proto '=https' --tlsv1.2 -sSf \
  https://raw.githubusercontent.com/lejunyang/one-sdk/main/install.sh | sh

# 安装并设为全局默认
osdk use -g node@20

# 直接使用
node --version
```

[查看完整安装说明 →](/guide/installation)
