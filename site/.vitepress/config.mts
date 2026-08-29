import { defineConfig, type DefaultTheme } from 'vitepress'

const github = 'https://github.com/lejunyang/one-sdk'

const zhNav: DefaultTheme.NavItem[] = [
  { text: '介绍', link: '/guide/introduction' },
  { text: '安装', link: '/guide/installation' },
  { text: '使用指南', link: '/guide/features' },
  { text: '实现方式', link: '/guide/implementation/' }
]

const enNav: DefaultTheme.NavItem[] = [
  { text: 'Introduction', link: '/en/guide/introduction' },
  { text: 'Installation', link: '/en/guide/installation' },
  { text: 'Guides', link: '/en/guide/features' },
  { text: 'Internals', link: '/en/guide/implementation/' }
]

const zhSidebar: DefaultTheme.Sidebar = [
  {
    text: '开始使用',
    items: [
      { text: '项目介绍', link: '/guide/introduction' },
      { text: '安装', link: '/guide/installation' }
    ]
  },
  {
    text: '使用指南',
    items: [
      { text: '功能总览', link: '/guide/features' },
      { text: '开始使用与通用命令', link: '/guide/getting-started' },
      { text: '项目配置与信任', link: '/guide/projects' },
      { text: '锁文件与环境复现', link: '/guide/lockfiles' },
      { text: '语言与运行时', link: '/guide/runtimes' },
      { text: 'JavaScript 包管理器', link: '/guide/package-managers' },
      { text: 'npm 开发工具', link: '/guide/npm-tools' },
      { text: 'Cargo 开发工具', link: '/guide/cargo-tools' },
      { text: '直接 HTTPS 制品', link: '/guide/http-artifacts' },
      { text: '模型快照', link: '/guide/models' },
      { text: '来源、离线与安全', link: '/guide/sources-security' },
      { text: '容器运行时、Registry 与原生操作', link: '/guide/containers' },
      { text: '存储、Shell 与诊断', link: '/guide/storage-shell' }
    ]
  },
  {
    text: '实现方式',
    items: [
      { text: '实现总览', link: '/guide/implementation/' },
      { text: '项目发现与版本解析', link: '/guide/implementation/resolution' },
      { text: '安装管线与并发', link: '/guide/implementation/installation' },
      { text: 'Shim、激活与锁文件', link: '/guide/implementation/activation-lockfile' },
      { text: 'SDK 来源与依赖 Registry', link: '/guide/implementation/sources-registries' },
      { text: 'HTTP 制品 backend', link: '/guide/implementation/http-artifacts' },
      { text: '原生容器诊断与操作', link: '/guide/implementation/containers' },
      { text: '内容存储与原生缓存', link: '/guide/implementation/storage-cache' },
      { text: '完整性与来源验证', link: '/guide/implementation/verification' },
      { text: 'Backend 与模型快照', link: '/guide/implementation/backends-models' },
      { text: 'npm 开发工具实现', link: '/guide/implementation/npm-tools' },
      { text: 'Cargo 开发工具实现', link: '/guide/implementation/cargo-tools' },
      { text: '可靠性与跨平台', link: '/guide/implementation/reliability' }
    ]
  }
]

const enSidebar: DefaultTheme.Sidebar = [
  {
    text: 'Getting Started',
    items: [
      { text: 'Introduction', link: '/en/guide/introduction' },
      { text: 'Installation', link: '/en/guide/installation' }
    ]
  },
  {
    text: 'Guide',
    items: [
      { text: 'Feature Overview', link: '/en/guide/features' },
      { text: 'Getting Started and Commands', link: '/en/guide/getting-started' },
      { text: 'Projects, Configuration, and Trust', link: '/en/guide/projects' },
      { text: 'Lockfiles and Reproducibility', link: '/en/guide/lockfiles' },
      { text: 'Languages and Runtimes', link: '/en/guide/runtimes' },
      { text: 'JavaScript Package Managers', link: '/en/guide/package-managers' },
      { text: 'npm Developer Tools', link: '/en/guide/npm-tools' },
      { text: 'Cargo Developer Tools', link: '/en/guide/cargo-tools' },
      { text: 'Direct HTTPS Artifacts', link: '/en/guide/http-artifacts' },
      { text: 'Model Snapshots', link: '/en/guide/models' },
      { text: 'Sources, Offline, and Security', link: '/en/guide/sources-security' },
      { text: 'Container Runtimes, Registries, and Native Operations', link: '/en/guide/containers' },
      { text: 'Storage, Shell, and Diagnostics', link: '/en/guide/storage-shell' }
    ]
  },
  {
    text: 'Internals',
    items: [
      { text: 'Implementation Overview', link: '/en/guide/implementation/' },
      { text: 'Project Discovery and Resolution', link: '/en/guide/implementation/resolution' },
      { text: 'Install Pipeline and Concurrency', link: '/en/guide/implementation/installation' },
      { text: 'Shims, Activation, and Lockfiles', link: '/en/guide/implementation/activation-lockfile' },
      { text: 'SDK Sources and Registries', link: '/en/guide/implementation/sources-registries' },
      { text: 'HTTP Artifact Backend', link: '/en/guide/implementation/http-artifacts' },
      { text: 'Native Container Diagnostics and Operations', link: '/en/guide/implementation/containers' },
      { text: 'Content Store and Native Caches', link: '/en/guide/implementation/storage-cache' },
      { text: 'Integrity and Provenance', link: '/en/guide/implementation/verification' },
      { text: 'Backends and Model Snapshots', link: '/en/guide/implementation/backends-models' },
      { text: 'npm Developer Tool Implementation', link: '/en/guide/implementation/npm-tools' },
      { text: 'Cargo Developer Tool Implementation', link: '/en/guide/implementation/cargo-tools' },
      { text: 'Reliability and Portability', link: '/en/guide/implementation/reliability' }
    ]
  }
]

export default defineConfig({
  base: '/one-sdk/',
  cleanUrls: true,
  lastUpdated: true,
  sitemap: {
    hostname: 'https://lejunyang.github.io/one-sdk/'
  },
  head: [
    ['meta', { name: 'theme-color', content: '#356859' }],
    ['link', { rel: 'icon', href: '/one-sdk/logo.svg', type: 'image/svg+xml' }]
  ],
  markdown: {
    lineNumbers: true
  },
  locales: {
    root: {
      label: '简体中文',
      lang: 'zh-CN',
      title: 'osdk',
      titleTemplate: 'one SDK manager',
      description: '一个跨平台、一站式、多语言 SDK 版本管理器',
      themeConfig: {
        nav: zhNav,
        sidebar: zhSidebar,
        editLink: {
          pattern: `${github}/edit/main/site/:path`,
          text: '在 GitHub 上编辑此页'
        },
        footer: {
          message: '基于 MIT 许可发布',
          copyright: 'Copyright © 2026 osdk contributors'
        },
        outline: { label: '本页目录', level: [2, 3] },
        lastUpdated: { text: '最后更新于' },
        docFooter: { prev: '上一篇', next: '下一篇' },
        darkModeSwitchLabel: '外观',
        lightModeSwitchTitle: '切换到浅色模式',
        darkModeSwitchTitle: '切换到深色模式',
        sidebarMenuLabel: '菜单',
        returnToTopLabel: '返回顶部',
        langMenuLabel: '切换语言',
        notFound: {
          title: '页面未找到',
          quote: '你访问的页面不存在或已被移动。',
          linkLabel: '返回首页',
          linkText: '返回首页'
        }
      }
    },
    en: {
      label: 'English',
      lang: 'en-US',
      link: '/en/',
      title: 'osdk',
      titleTemplate: 'one SDK manager',
      description: 'One cross-platform manager for all your language SDKs',
      themeConfig: {
        nav: enNav,
        sidebar: enSidebar,
        editLink: {
          pattern: `${github}/edit/main/site/:path`,
          text: 'Edit this page on GitHub'
        },
        footer: {
          message: 'Released under the MIT License',
          copyright: 'Copyright © 2026 osdk contributors'
        },
        outline: { label: 'On this page', level: [2, 3] },
        lastUpdated: { text: 'Last updated' },
        docFooter: { prev: 'Previous', next: 'Next' }
      }
    }
  },
  themeConfig: {
    logo: '/logo.svg',
    socialLinks: [{ icon: 'github', link: github }],
    search: {
      provider: 'local',
      options: {
        locales: {
          root: {
            translations: {
              button: {
                buttonText: '搜索文档',
                buttonAriaLabel: '搜索文档'
              },
              modal: {
                noResultsText: '未找到相关结果',
                resetButtonTitle: '清除查询条件',
                footer: {
                  selectText: '选择',
                  navigateText: '切换',
                  closeText: '关闭'
                }
              }
            }
          }
        }
      }
    }
  }
})
