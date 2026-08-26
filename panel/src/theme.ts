import { useEffect, useState } from 'react'

/**
 * Light, dark, or whatever the machine says.
 *
 * The choice is written to `data-theme` on the root element; the stylesheet
 * defines the dark palette twice — once under `prefers-color-scheme` for
 * `system`, once under `[data-theme='dark']` for an explicit pick — so the
 * toggle wins in both directions and `system` is genuinely the OS's call
 * rather than a guess frozen at first paint.
 */
export type Theme = 'light' | 'dark' | 'system'

const KEY = 'wingman.theme'

export function readTheme(): Theme {
  const saved = window.localStorage.getItem(KEY)
  return saved === 'light' || saved === 'dark' ? saved : 'system'
}

/** Exported for `main.tsx`, which applies the saved choice before React mounts
    so a dark-mode reader never gets a frame of white page. */
export function applyTheme(theme: Theme) {
  const root = document.documentElement
  if (theme === 'system') root.removeAttribute('data-theme')
  else root.setAttribute('data-theme', theme)
}

export function useTheme() {
  const [theme, set] = useState<Theme>(readTheme)

  useEffect(() => {
    applyTheme(theme)
    if (theme === 'system') window.localStorage.removeItem(KEY)
    else window.localStorage.setItem(KEY, theme)
  }, [theme])

  return { theme, setTheme: set }
}

/** What the toggle does next, and what to call it while it is showing. */
export function nextTheme(theme: Theme): Theme {
  return theme === 'system' ? 'light' : theme === 'light' ? 'dark' : 'system'
}
