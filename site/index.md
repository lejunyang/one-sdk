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
      text: 安装
      link: /guide/start/installation
    - theme: alt
      text: 使用指南
      link: /guide/
    - theme: alt
      text: 实现原理
      link: /internals/
    - theme: alt
      text: GitHub
      link: https://github.com/lejunyang/one-sdk

features:
  - icon: ◈
    title: 一套命令，所有运行时
    details: Node.js、Python、Java、Go、Rust、Zig、Android SDK，以及 npm、Cargo、Go、PyPI、Conda、GitHub Release 发布的工具，都用同样的 install / use / exec。
    link: /guide/runtimes/overview
    linkText: 运行时总览
  - icon: ⌁
    title: 可复现的项目
    details: osdk.toml 声明工具与任务，按平台分区的 osdk.lock 记录精确版本与制品身份，团队和 CI 一条 install 复现。
    link: /guide/project/lockfiles
    linkText: 锁文件
  - icon: ⇄
    title: 自动选择最快镜像
    details: 探测官方源与权威镜像，按实测速度排序，制品校验失败时自动切换下一个来源。
    link: /guide/sources/mirrors
    linkText: 下载源与镜像
  - icon: ✓
    title: 完整性与来源验证
    details: 校验上游 checksum 与签名，可按策略要求 GitHub Artifact Attestation；项目里的安全相关配置需要先审批。
    link: /guide/sources/integrity
    linkText: 完整性与签名
  - icon: ⧉
    title: 内容去重与离线重装
    details: 已校验文件写入内容寻址存储并跨版本复用；联网装过一次后，可用 --offline 从缓存重装。
    link: /guide/config/storage
    linkText: 目录与存储
  - icon: ⬡
    title: 模型、Skill 与系统集成
    details: 锁定 Hugging Face、ModelScope、Civitai 模型快照，为 AI Agent 安装 skill，诊断容器运行时与系统包管理器。
    link: /guide/ai/models
    linkText: 模型快照
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

[查看完整安装说明 →](/guide/start/installation)
