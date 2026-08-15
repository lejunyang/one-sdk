import { defineConfig, type DefaultTheme } from 'vitepress'

const github = 'https://github.com/lejunyang/one-sdk'

const zhNav: DefaultTheme.NavItem[] = [
  { text: '介绍', link: '/guide/introduction' },
  { text: '安装', link: '/guide/installation' },
  { text: '功能', link: '/guide/features' }
]

const enNav: DefaultTheme.NavItem[] = [
  { text: 'Introduction', link: '/en/guide/introduction' },
  { text: 'Installation', link: '/en/guide/installation' },
  { text: 'Features', link: '/en/guide/features' }
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
    items: [{ text: '详细功能', link: '/guide/features' }]
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
    items: [{ text: 'Feature Reference', link: '/en/guide/features' }]
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
