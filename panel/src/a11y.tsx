import { useEffect, useRef } from 'react'

/**
 * The parts of a dialog that are not visible.
 *
 * Both overlays in the panel — the board's task drawer and the command
 * palette — shipped as a `role="dialog"` and nothing else. That is the half of
 * a dialog you can see. A keyboard user got the other half: focus left behind
 * on the page underneath, Tab walking out of the panel and through every
 * control it is covering, and nowhere to land when it closed. Escape was
 * handled, which made it look finished.
 *
 * One hook, because there is no version of this that is worth writing twice
 * and no version worth getting subtly different in two places.
 */

const FOCUSABLE = [
  'a[href]',
  'button:not([disabled])',
  'input:not([disabled])',
  'select:not([disabled])',
  'textarea:not([disabled])',
  '[tabindex]:not([tabindex="-1"])',
].join(',')

function focusable(root: HTMLElement): HTMLElement[] {
  return Array.from(root.querySelectorAll<HTMLElement>(FOCUSABLE)).filter(
    // `offsetParent` is null for anything `display: none`, which is how a
    // collapsed `<details>` hides its contents — tabbing into an invisible
    // control is the same bug as tabbing out of the dialog.
    (el) => el.offsetParent !== null || el === document.activeElement,
  )
}

/**
 * Attach to a dialog's outermost element.
 *
 * Moves focus in on open, keeps Tab inside, closes on Escape, and puts focus
 * back where it came from on close. If the element that opened the dialog is
 * gone from the document by then — a card deleted from the drawer that was
 * opened from it — focus is left where the browser put it rather than thrown
 * at a detached node.
 */
export function useDialog<T extends HTMLElement>(onClose: () => void) {
  const ref = useRef<T | null>(null)
  const opener = useRef<Element | null>(null)

  // `onClose` is usually an inline arrow, so it is a new function every
  // render. Held in a ref, the listeners below are installed once instead of
  // being torn down and rebuilt on every keystroke the parent re-renders on.
  const close = useRef(onClose)
  close.current = onClose

  useEffect(() => {
    opener.current = document.activeElement
    const root = ref.current
    if (root) {
      // The dialog itself first, not its first button: announcing the panel
      // before its controls is what tells a screen-reader user where they now
      // are, and it means Escape works before anything has been tabbed to.
      const first = root.hasAttribute('tabindex') ? root : (focusable(root)[0] ?? root)
      first.focus()
    }

    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') {
        e.stopPropagation()
        return close.current()
      }
      if (e.key !== 'Tab' || !ref.current) return
      const items = focusable(ref.current)
      if (items.length === 0) return e.preventDefault()
      const first = items[0]
      const last = items[items.length - 1]
      const active = document.activeElement
      // Wrap at both ends. Without the `!contains` arm, focus that has already
      // escaped — a browser autofill dropdown, a mid-render remount — never
      // comes back.
      if (!ref.current.contains(active)) {
        e.preventDefault()
        first.focus()
      } else if (e.shiftKey && active === first) {
        e.preventDefault()
        last.focus()
      } else if (!e.shiftKey && active === last) {
        e.preventDefault()
        first.focus()
      }
    }

    document.addEventListener('keydown', onKey, true)
    return () => {
      document.removeEventListener('keydown', onKey, true)
      const back = opener.current
      if (back instanceof HTMLElement && document.contains(back)) back.focus()
    }
  }, [])

  return ref
}

/**
 * A key sequence, for `g` then `b`.
 *
 * Two-key navigation is what a terminal-native audience reaches for before it
 * reaches for the mouse, and the whole mechanism is one timer: a prefix that
 * expires. Held longer than a second and the second key is a new intent, not a
 * continuation.
 */
export function useLeader(bindings: Record<string, () => void>, leader = 'g') {
  const armed = useRef<number | null>(null)

  // Same rebuild-avoidance as above: `bindings` is an object literal at every
  // call site.
  const map = useRef(bindings)
  map.current = bindings

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.metaKey || e.ctrlKey || e.altKey || typing(e.target)) return
      const now = Date.now()
      if (armed.current !== null && now - armed.current < 1000) {
        armed.current = null
        const run = map.current[e.key.toLowerCase()]
        if (run) {
          e.preventDefault()
          run()
        }
        return
      }
      armed.current = e.key.toLowerCase() === leader ? now : null
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [leader])
}

/**
 * A single key, ignored while the user is typing.
 *
 * `/` and `?` are the two the panel binds, and both are characters someone
 * will legitimately type into the composer — the check is the whole point of
 * the helper.
 */
export function useHotkey(key: string, run: () => void) {
  const fn = useRef(run)
  fn.current = run

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.metaKey || e.ctrlKey || e.altKey || typing(e.target)) return
      if (chord(e) !== key) return
      e.preventDefault()
      fn.current()
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [key])
}

/**
 * What was actually pressed, for the one pair that disagrees.
 *
 * A browser reports shift-and-slash as `?`. A synthesised event, and some
 * keyboard layouts, report `/` with `shiftKey` set instead — so binding `?`
 * naively works when you test it by hand and silently does nothing elsewhere.
 * Normalising here also keeps `/` from firing when shift is held, which is
 * what makes the two bindings distinct rather than overlapping.
 */
function chord(e: KeyboardEvent): string {
  return e.key === '/' && e.shiftKey ? '?' : e.key
}

/** Is the event coming from somewhere a bare letter means a letter? */
function typing(target: EventTarget | null): boolean {
  if (!(target instanceof HTMLElement)) return false
  const tag = target.tagName
  return tag === 'INPUT' || tag === 'TEXTAREA' || tag === 'SELECT' || target.isContentEditable
}

/**
 * Warn before leaving with unsaved edits.
 *
 * Covers closing or reloading the tab, and nothing else. The panel routes
 * in-page with `history.pushState`, which fires no cancellable event, so an
 * in-page navigation away from unsaved edits is **not** guarded — the config
 * form's save bar staying on screen is what carries that case today. Said
 * plainly here rather than left to look like a full guard.
 */
export function useUnsavedWarning(dirty: boolean) {
  useEffect(() => {
    if (!dirty) return
    const onLeave = (e: BeforeUnloadEvent) => {
      e.preventDefault()
      // Modern browsers show their own wording and ignore ours; the assignment
      // is what arms the prompt at all in the older ones.
      e.returnValue = ''
    }
    window.addEventListener('beforeunload', onLeave)
    return () => window.removeEventListener('beforeunload', onLeave)
  }, [dirty])
}

/**
 * `navigator.clipboard` with the reason it can fail.
 *
 * It is unavailable on a plain-HTTP origin that is not loopback — which is the
 * phone-on-the-LAN case the panel exists for — so the caller has to be told,
 * not left with a button that does nothing.
 */
export async function copyText(text: string): Promise<boolean> {
  try {
    await navigator.clipboard.writeText(text)
    return true
  } catch {
    return false
  }
}
