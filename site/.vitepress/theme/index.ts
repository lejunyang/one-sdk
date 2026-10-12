import DefaultTheme from 'vitepress/theme'
import type { Theme } from 'vitepress'
import { inBrowser } from 'vitepress'
import redirects from '../redirects.json'
import './style.css'

const base = '/one-sdk'
const table = redirects as Record<string, string>

/**
 * Pages were regrouped into functional directories. A retired URL (README
 * links on older tags, bookmarks, search results) is served GitHub Pages'
 * 404 document, which still boots this app; send it to the page's new home
 * before the 404 content is read. In-site links already point at new paths.
 */
function redirectRetired(path: string): string | null {
  const local = path.startsWith(base) ? path.slice(base.length) || '/' : path
  const clean = local.replace(/\.html$/, '')
  return table[clean] ?? table[`${clean}/`] ?? null
}

export default {
  extends: DefaultTheme,
  enhanceApp() {
    if (!inBrowser) return
    const target = redirectRetired(window.location.pathname)
    if (target) window.location.replace(`${base}${target}${window.location.hash}`)
  }
} satisfies Theme
