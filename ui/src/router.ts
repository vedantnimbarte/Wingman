import { useEffect, useState } from 'react'

/**
 * Path routing in fifteen lines, because that is all this needs.
 *
 * `serve::ui` answers every non-`/v1` GET with the app shell, so real paths
 * deep-link and the back button works. A routing library would add a
 * dependency and a bundle for `pathname` plus one event listener.
 */
export function useRoute(): string {
  const [path, setPath] = useState(() => window.location.pathname)

  useEffect(() => {
    const onPop = () => setPath(window.location.pathname)
    window.addEventListener('popstate', onPop)
    // `navigate` dispatches this so a click updates the view without a reload.
    window.addEventListener('wingman:navigate', onPop)
    return () => {
      window.removeEventListener('popstate', onPop)
      window.removeEventListener('wingman:navigate', onPop)
    }
  }, [])

  return path
}

export function navigate(to: string) {
  if (window.location.pathname === to) return
  window.history.pushState(null, '', to)
  window.dispatchEvent(new Event('wingman:navigate'))
}
