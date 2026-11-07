import { useEffect, useId, useLayoutEffect, useMemo, useRef, useState } from 'react'
import { createPortal } from 'react-dom'
import { Icon } from './ui'

/**
 * A choice from a list, as a popover — the panel's replacement for `<select>`.
 *
 * A native select cannot show a second line, a branch name or a search box,
 * and on most desktops it opens a menu styled by the OS rather than by this
 * page. This keeps what a select does well and adds those:
 *
 * - **Keyboard**: ↑ ↓ Home End to move, Enter to choose, Esc to close, and
 *   typing jumps to the first match (or filters, when there is a search box).
 * - **Screen readers**: a `listbox` of `option`s, with the highlighted one
 *   announced through `aria-activedescendant`.
 * - **Phones**: below 40rem it is a bottom sheet with thumb-sized rows.
 *
 * The list is portalled to `<body>` so no `overflow: hidden` ancestor can clip
 * it, and placed below its trigger, or above when there is no room below.
 */

export type PickerOption = {
  value: string
  label: string
  /** A muted second line. */
  description?: string
  /** Something short on the right: a count, a branch, a state. */
  detail?: React.ReactNode
  /** Shown, and choosable, but set apart — e.g. a value the schema does not list. */
  muted?: boolean
}

/** Lists longer than this get a search box. */
const SEARCH_AT = 8

const PHONE = '(max-width: 40rem)'

export function Picker({
  value,
  options,
  onChange,
  label,
  id,
  disabled,
  placeholder = 'Choose…',
  className = 'select picker-trigger',
  title,
  trigger,
}: {
  value: string
  options: PickerOption[]
  onChange: (value: string) => void
  /** What is being chosen. Names the list for assistive tech and heads the phone sheet. */
  label: string
  id?: string
  disabled?: boolean
  placeholder?: string
  className?: string
  title?: string
  /** Custom trigger content; the default is the chosen option's label. */
  trigger?: (current: PickerOption | undefined) => React.ReactNode
}) {
  const [open, setOpen] = useState(false)
  const [query, setQuery] = useState('')
  const [active, setActive] = useState(0)
  const [sheet, setSheet] = useState(false)
  const [pos, setPos] = useState<React.CSSProperties>({ opacity: 0 })
  const button = useRef<HTMLButtonElement | null>(null)
  const pop = useRef<HTMLDivElement | null>(null)
  const list = useRef<HTMLUListElement | null>(null)
  const search = useRef<HTMLInputElement | null>(null)
  const typed = useRef({ text: '', at: 0 })
  const listId = useId()

  const current = options.find((o) => o.value === value)
  const searchable = options.length >= SEARCH_AT
  const shown = useMemo(() => {
    const q = query.trim().toLowerCase()
    if (!q) return options
    return options.filter((o) =>
      `${o.label} ${o.description ?? ''} ${o.value}`.toLowerCase().includes(q),
    )
  }, [options, query])
  const at = Math.min(active, Math.max(0, shown.length - 1))

  function openList() {
    if (disabled) return
    setQuery('')
    setActive(Math.max(0, options.findIndex((o) => o.value === value)))
    setSheet(window.matchMedia(PHONE).matches)
    setPos({ opacity: 0 })
    setOpen(true)
  }

  function close(refocus = true) {
    setOpen(false)
    if (refocus) button.current?.focus()
  }

  function choose(o: PickerOption | undefined) {
    if (!o) return
    if (o.value !== value) onChange(o.value)
    close()
  }

  // Place the popover against its trigger; flip above when there is no room.
  useLayoutEffect(() => {
    if (!open || sheet) return
    function place() {
      const b = button.current?.getBoundingClientRect()
      const p = pop.current
      if (!b || !p) return
      const gap = 6
      const h = p.offsetHeight
      const w = Math.max(p.offsetWidth, b.width)
      const below = window.innerHeight - b.bottom - gap - 8
      const above = b.top - gap - 8
      const up = h > below && above > below
      setPos({
        minWidth: b.width,
        maxHeight: Math.max(160, up ? above : below),
        left: Math.min(Math.max(8, b.left), window.innerWidth - w - 8),
        top: up ? Math.max(8, b.top - gap - Math.min(h, above)) : b.bottom + gap,
      })
    }
    place()
    window.addEventListener('resize', place)
    window.addEventListener('scroll', place, true)
    return () => {
      window.removeEventListener('resize', place)
      window.removeEventListener('scroll', place, true)
    }
  }, [open, sheet, shown.length])

  // Focus moves into the list (or its search box) on open.
  useEffect(() => {
    if (!open) return
    ;(search.current ?? list.current)?.focus({ preventScroll: true })
  }, [open])

  // A click anywhere else closes it, without stealing focus back.
  useEffect(() => {
    if (!open) return
    const onDown = (e: MouseEvent) => {
      const t = e.target as Node
      if (pop.current?.contains(t) || button.current?.contains(t)) return
      close(false)
    }
    window.addEventListener('mousedown', onDown)
    return () => window.removeEventListener('mousedown', onDown)
  }, [open])

  useEffect(() => {
    if (!open) return
    list.current
      ?.querySelector<HTMLElement>(`[data-index="${at}"]`)
      ?.scrollIntoView({ block: 'nearest' })
  }, [open, at])

  function onListKey(e: React.KeyboardEvent) {
    const last = shown.length - 1
    switch (e.key) {
      case 'ArrowDown':
        setActive(Math.min(last, at + 1))
        break
      case 'ArrowUp':
        setActive(Math.max(0, at - 1))
        break
      case 'Home':
        setActive(0)
        break
      case 'End':
        setActive(last)
        break
      case 'PageDown':
        setActive(Math.min(last, at + 8))
        break
      case 'PageUp':
        setActive(Math.max(0, at - 8))
        break
      case 'Enter':
        choose(shown[at])
        break
      case 'Escape':
        close()
        break
      case 'Tab':
        close()
        return
      default:
        // Typeahead, for a list without a search box: letters jump to the
        // first option that starts with what has been typed in the last second.
        if (!searchable && e.key.length === 1 && !e.metaKey && !e.ctrlKey && !e.altKey) {
          const now = Date.now()
          const text = (now - typed.current.at < 1000 ? typed.current.text : '') + e.key.toLowerCase()
          typed.current = { text, at: now }
          const hit = shown.findIndex((o) => o.label.toLowerCase().startsWith(text))
          if (hit !== -1) setActive(hit)
          break
        }
        return
    }
    e.preventDefault()
  }

  function onTriggerKey(e: React.KeyboardEvent) {
    if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
      e.preventDefault()
      openList()
    }
  }

  const activeId = shown[at] ? `${listId}-${at}` : undefined

  return (
    <>
      <button
        ref={button}
        id={id}
        type="button"
        className={className}
        disabled={disabled}
        title={title}
        aria-haspopup="listbox"
        aria-expanded={open}
        aria-label={trigger ? undefined : `${label}: ${current?.label ?? placeholder}`}
        onClick={() => (open ? close() : openList())}
        onKeyDown={onTriggerKey}
      >
        {trigger ? (
          trigger(current)
        ) : (
          <span className={`picker-value truncate${current ? '' : ' is-placeholder'}`}>
            {current?.label ?? placeholder}
          </span>
        )}
      </button>

      {open &&
        createPortal(
          <div className={`picker-layer${sheet ? ' is-sheet' : ''}`}>
            {sheet && <div className="picker-backdrop" aria-hidden="true" />}
            <div ref={pop} className="picker-pop" style={sheet ? undefined : pos}>
              {sheet && (
                <div className="picker-sheet-head">
                  <span>{label}</span>
                  <button
                    type="button"
                    className="button button-quiet button-sm button-icon"
                    aria-label="Close"
                    onClick={() => close()}
                  >
                    <Icon name="close" size={16} />
                  </button>
                </div>
              )}

              {searchable && (
                <label className="picker-search">
                  <Icon name="search" size={14} />
                  <input
                    ref={search}
                    type="text"
                    placeholder={`Search ${label.toLowerCase()}…`}
                    value={query}
                    role="combobox"
                    aria-expanded="true"
                    aria-controls={listId}
                    aria-activedescendant={activeId}
                    aria-autocomplete="list"
                    aria-label={`Search ${label.toLowerCase()}`}
                    spellCheck={false}
                    onChange={(e) => {
                      setQuery(e.target.value)
                      setActive(0)
                    }}
                    onKeyDown={onListKey}
                  />
                </label>
              )}

              {shown.length === 0 ? (
                <p className="picker-empty">Nothing matches “{query.trim()}”.</p>
              ) : (
                <ul
                  ref={list}
                  id={listId}
                  role="listbox"
                  aria-label={label}
                  aria-activedescendant={searchable ? undefined : activeId}
                  tabIndex={searchable ? undefined : -1}
                  className="picker-list"
                  onKeyDown={searchable ? undefined : onListKey}
                >
                  {shown.map((o, i) => {
                    const selected = o.value === value
                    return (
                      <li
                        key={o.value}
                        id={`${listId}-${i}`}
                        data-index={i}
                        role="option"
                        aria-selected={selected}
                        className={`picker-option${i === at ? ' is-active' : ''}${o.muted ? ' is-muted' : ''}`}
                        onMouseMove={() => i !== at && setActive(i)}
                        onClick={() => choose(o)}
                      >
                        <span className="picker-check" aria-hidden="true">
                          {selected && <Icon name="check" size={14} />}
                        </span>
                        <span className="picker-text">
                          <span className="picker-label">{o.label}</span>
                          {o.description && <span className="picker-desc">{o.description}</span>}
                        </span>
                        {o.detail != null && <span className="picker-detail">{o.detail}</span>}
                      </li>
                    )
                  })}
                </ul>
              )}
            </div>
          </div>,
          document.body,
        )}
    </>
  )
}
