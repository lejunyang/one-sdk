import { defineConfig, type DefaultTheme } from 'vitepress'

const github = 'https://github.com/lejunyang/one-sdk'

type Item = DefaultTheme.SidebarItem

/** One guide group, with every link prefixed by the locale's guide root. */
function group(text: string, root: string, items: [string, string][], collapsed = false): Item {
  return {
    text,
    collapsed,
    items: items.map(([label, path]) => ({ text: label, link: `${root}${path}` }))
  }
}

// ---------------------------------------------------------------- 中文

const zhGuide = '/guide/'
const zhGuideSidebar: Item[] = [
  group('开始', zhGuide, [
    ['项目介绍', 'start/introduction'],
    ['安装', 'start/installation'],
    ['快速上手', 'start/quick-start']
  ]),
  group('配置', zhGuide, [
    ['配置文件', 'config/files'],
    ['工具选择与来源诊断', 'config/tool-selection'],
    ['各工具环境变量', 'config/tool-environment'],
    ['osdk 环境变量', 'config/environment'],
    ['命令行全局参数', 'config/cli-options'],
    ['目录与存储', 'config/storage'],
    ['项目配置信任', 'config/trust']
  ]),
  group('基本使用', zhGuide, [
    ['安装、切换与查询', 'basics/commands'],
    ['Shell 激活与补全', 'basics/shell'],
    ['控制哪些命令进 PATH', 'basics/path-control']
  ]),
  group('项目工作流', zhGuide, [
    ['项目工具链', 'project/discovery'],
    ['可复现锁文件', 'project/lockfiles'],
    ['项目任务', 'project/tasks'],
    ['应用依赖', 'project/deps'],
    ['GitHub Actions', 'project/github-actions']
  ]),
  group('语言与运行时', zhGuide, [
    ['运行时总览', 'runtimes/overview'],
    ['Node.js', 'runtimes/node'],
    ['Python', 'runtimes/python'],
    ['Java 与 JVM 工具', 'runtimes/java'],
    ['Go', 'runtimes/go'],
    ['Rust', 'runtimes/rust'],
    ['Zig', 'runtimes/zig'],
    ['Android SDK', 'runtimes/android']
  ], true),
  group('包管理器与开发工具', zhGuide, [
    ['包管理器', 'tools/package-managers'],
    ['npm 工具', 'tools/npm'],
    ['Cargo 工具', 'tools/cargo'],
    ['Go 工具', 'tools/go'],
    ['Python 工具', 'tools/python'],
    ['Conda 工具', 'tools/conda'],
    ['GitHub Release 工具', 'tools/github-releases'],
    ['直接 HTTPS 制品', 'tools/http-artifacts'],
    ['声明式 Backend', 'tools/declarative-backends']
  ], true),
  group('模型与 AI', zhGuide, [
    ['模型快照', 'ai/models'],
    ['Agent skills', 'ai/skills']
  ], true),
  group('下载源与安全', zhGuide, [
    ['下载源与镜像', 'sources/mirrors'],
    ['离线与预发布', 'sources/offline'],
    ['完整性、签名与 Attestation', 'sources/integrity']
  ], true),
  group('系统集成', zhGuide, [
    ['容器运行时', 'system/containers'],
    ['系统包管理器', 'system/packages']
  ], true),
  group('维护', zhGuide, [
    ['缓存与清理', 'maintenance/cache'],
    ['诊断与安装校验', 'maintenance/diagnostics']
  ], true)
]

const zhInternals = '/internals/'
const zhInternalsSidebar: Item[] = [
  group('概览', zhInternals, [['实现总览', '']]),
  group('核心流程', zhInternals, [
    ['项目发现与版本解析', 'resolution'],
    ['安装管线与并发', 'installation'],
    ['Shim、激活与锁文件', 'activation-lockfile'],
    ['内容存储与原生缓存', 'storage-cache'],
    ['完整性与来源验证', 'verification'],
    ['可靠性与跨平台', 'reliability']
  ]),
  group('来源与 Registry', zhInternals, [
    ['SDK 来源与依赖 Registry', 'sources-registries'],
    ['HTTP 制品 backend', 'http-artifacts']
  ]),
  group('Backend 实现', zhInternals, [
    ['Backend 与模型快照', 'backends-models'],
    ['npm 开发工具实现', 'npm-tools'],
    ['Cargo 开发工具实现', 'cargo-tools'],
    ['Go 开发工具实现', 'go-tools'],
    ['Conda 开发工具实现', 'conda-tools'],
    ['应用依赖实现', 'deps']
  ]),
  group('系统集成', zhInternals, [
    ['原生容器诊断与操作', 'containers'],
    ['系统包管理器的检查机制', 'system-packages']
  ]),
  group('工程', zhInternals, [['发布流水线', 'releasing']])
]

// ---------------------------------------------------------------- English

const enGuide = '/en/guide/'
const enGuideSidebar: Item[] = [
  group('Getting Started', enGuide, [
    ['Introduction', 'start/introduction'],
    ['Installation', 'start/installation'],
    ['Quick Start', 'start/quick-start']
  ]),
  group('Configuration', enGuide, [
    ['Configuration Files', 'config/files'],
    ['Tool Selection and Diagnostics', 'config/tool-selection'],
    ['Tool Environments', 'config/tool-environment'],
    ['osdk Environment Variables', 'config/environment'],
    ['Global Command-line Options', 'config/cli-options'],
    ['Directories and Storage', 'config/storage'],
    ['Project Configuration Trust', 'config/trust']
  ]),
  group('Basics', enGuide, [
    ['Install, Switch, and Inspect', 'basics/commands'],
    ['Shell Activation and Completions', 'basics/shell'],
    ['Choosing Which Commands Reach PATH', 'basics/path-control']
  ]),
  group('Project Workflows', enGuide, [
    ['Project Toolchains', 'project/discovery'],
    ['Reproducible Lockfiles', 'project/lockfiles'],
    ['Project Tasks', 'project/tasks'],
    ['Application Dependencies', 'project/deps'],
    ['GitHub Actions', 'project/github-actions']
  ]),
  group('Languages and Runtimes', enGuide, [
    ['Runtimes Overview', 'runtimes/overview'],
    ['Node.js', 'runtimes/node'],
    ['Python', 'runtimes/python'],
    ['Java and JVM Tools', 'runtimes/java'],
    ['Go', 'runtimes/go'],
    ['Rust', 'runtimes/rust'],
    ['Zig', 'runtimes/zig'],
    ['Android SDK', 'runtimes/android']
  ], true),
  group('Package Managers and Tools', enGuide, [
    ['Package Managers', 'tools/package-managers'],
    ['npm Tools', 'tools/npm'],
    ['Cargo Tools', 'tools/cargo'],
    ['Go Tools', 'tools/go'],
    ['Python Tools', 'tools/python'],
    ['Conda Tools', 'tools/conda'],
    ['GitHub Release Tools', 'tools/github-releases'],
    ['Direct HTTPS Artifacts', 'tools/http-artifacts'],
    ['Declarative Backends', 'tools/declarative-backends']
  ], true),
  group('Models and AI', enGuide, [
    ['Model Snapshots', 'ai/models'],
    ['Agent Skills', 'ai/skills']
  ], true),
  group('Sources and Security', enGuide, [
    ['Download Sources and Mirrors', 'sources/mirrors'],
    ['Offline Mode and Pre-releases', 'sources/offline'],
    ['Integrity, Signatures, and Attestation', 'sources/integrity']
  ], true),
  group('System Integration', enGuide, [
    ['Container Runtimes', 'system/containers'],
    ['System Package Managers', 'system/packages']
  ], true),
  group('Maintenance', enGuide, [
    ['Caches and Cleanup', 'maintenance/cache'],
    ['Diagnostics and Install Verification', 'maintenance/diagnostics']
  ], true)
]

const enInternals = '/en/internals/'
const enInternalsSidebar: Item[] = [
  group('Overview', enInternals, [['Implementation Overview', '']]),
  group('Core Flow', enInternals, [
    ['Project Discovery and Resolution', 'resolution'],
    ['Install Pipeline and Concurrency', 'installation'],
    ['Shims, Activation, and Lockfiles', 'activation-lockfile'],
    ['Content Store and Native Caches', 'storage-cache'],
    ['Integrity and Provenance', 'verification'],
    ['Reliability and Portability', 'reliability']
  ]),
  group('Sources and Registries', enInternals, [
    ['SDK Sources and Registries', 'sources-registries'],
    ['HTTP Artifact Backend', 'http-artifacts']
  ]),
  group('Backend Implementations', enInternals, [
    ['Backends and Model Snapshots', 'backends-models'],
    ['npm Developer Tool Implementation', 'npm-tools'],
    ['Cargo Developer Tool Implementation', 'cargo-tools'],
    ['Go Developer Tool Implementation', 'go-tools'],
    ['Conda Developer Tool Implementation', 'conda-tools'],
    ['Application Dependency Implementation', 'deps']
  ]),
  group('System Integration', enInternals, [
    ['Native Container Diagnostics and Operations', 'containers'],
    ['How System Package Managers Are Inspected', 'system-packages']
  ]),
  group('Engineering', enInternals, [['Release Pipeline', 'releasing']])
]

const zhNav: DefaultTheme.NavItem[] = [
  { text: '使用指南', link: '/guide/', activeMatch: '^/guide/' },
  { text: '实现原理', link: '/internals/', activeMatch: '^/internals/' },
  { text: '更新日志', link: `${github}/releases` }
]

const enNav: DefaultTheme.NavItem[] = [
  { text: 'Guide', link: '/en/guide/', activeMatch: '^/en/guide/' },
  { text: 'Internals', link: '/en/internals/', activeMatch: '^/en/internals/' },
  { text: 'Releases', link: `${github}/releases` }
]

export default defineConfig({
  base: '/one-sdk/',
  cleanUrls: true,
  lastUpdated: true,
  sitemap: {
    hostname: 'https://lejunyang.github.io/one-sdk/'
  },
  head: [
    ['meta', { name: 'theme-color', content: '#1f6f5c' }],
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
        sidebar: {
          '/guide/': zhGuideSidebar,
          '/internals/': zhInternalsSidebar
        },
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
        sidebar: {
          '/en/guide/': enGuideSidebar,
          '/en/internals/': enInternalsSidebar
        },
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
