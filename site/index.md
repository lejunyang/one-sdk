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
      text: 实现方式
      link: /guide/implementation/
    - theme: alt
      text: GitHub
      link: https://github.com/lejunyang/one-sdk

features:
  - icon: ◈
    title: 一个命令，多种 SDK
    details: 用一致的命令管理 Node.js、Python、Java、Go、Rust、包管理器、`npm:<package>` 开发工具和 GitHub Release 工具。
  - icon: ⧉
    title: 跨版本内容去重
    details: 多个已安装版本可复用相同文件，减少重复磁盘占用；具体存储方式见实现说明。
  - icon: ⇄
    title: 自动选择最快镜像
    details: 探测官方源和权威镜像，按速度选择并在元数据或下载失败时自动切换。
  - icon: ⌁
    title: 可复现的项目环境
    details: 生成按平台分区的 osdk.lock，保存精确版本和各 backend 的复现信息；npm 工具引用随 lock 提交的内容寻址 graph sidecar。
  - icon: ✓
    title: 完整性与来源验证
    details: 支持上游校验和、严格校验策略，以及 GitHub Artifact Attestations 的 Sigstore 验证。
  - icon: ⬡
    title: 离线重装与共享缓存
    details: 复用元数据、制品和原生包缓存；npm 工具可结合已提交的 graph sidecar 与预热的 Aube 缓存离线重装。
---

## 三步开始

```bash
# 安装 osdk
curl --proto '=https' --tlsv1.2 -sSf \
  https://gh-proxy.com/https://raw.githubusercontent.com/lejunyang/one-sdk/main/install.sh |
  OSDK_DOWNLOAD_BASE_URL=https://gh-proxy.com/https://github.com sh

# 安装并设为全局默认
osdk use -g node@20

# 直接使用
node --version
```

[查看完整安装说明 →](/guide/installation)
