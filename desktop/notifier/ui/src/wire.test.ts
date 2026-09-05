import { describe, expect, it } from 'vitest'
import {
  AUTO_DISMISS_MS,
  VISIBLE,
  autoDismisses,
  expired,
  replyPayload,
  tone,
  view,
  type Notification,
} from './wire'

function card(over: Partial<Notification> = {}): Notification {
  return {
    id: 'n1',
    severity: 'info',
    title: 'Run finished',
    body: '',
    project: 'wingman',
    run_dir: null,
    created_at: 1_757_068_923,
    expires_at: null,
    actions: [],
    free_text: false,
    ...over,
  }
}

describe('tone', () => {
  it('maps severity onto the three status hues and nothing else', () => {
    expect(tone('escalation')).toBe('failed')
    expect(tone('decision')).toBe('asserted')
    expect(tone('progress')).toBe('proven')
    expect(tone('info')).toBe('neutral')
    // A severity from a newer wingman must render, not crash.
    expect(tone('something-new')).toBe('neutral')
  })
})

describe('autoDismisses', () => {
  it('never lets a card someone must answer vanish on a timer', () => {
    expect(autoDismisses(card({ severity: 'decision', free_text: true }))).toBe(false)
    expect(
      autoDismisses(
        card({ severity: 'decision', actions: [{ id: 'approve', label: 'Approve' }] }),
      ),
    ).toBe(false)
    // A failure has no buttons but still has to be seen.
    expect(autoDismisses(card({ severity: 'escalation' }))).toBe(false)
  })

  it('lets plain news go', () => {
    expect(autoDismisses(card({ severity: 'info' }))).toBe(true)
    expect(autoDismisses(card({ severity: 'progress' }))).toBe(true)
    expect(AUTO_DISMISS_MS).toBeGreaterThan(3000)
  })
})

describe('expired', () => {
  it('is true once the asker has stopped listening', () => {
    expect(expired(card({ expires_at: 100 }), 101)).toBe(true)
    expect(expired(card({ expires_at: 100 }), 100)).toBe(true)
    expect(expired(card({ expires_at: 100 }), 99)).toBe(false)
  })

  it('is never true without a deadline', () => {
    expect(expired(card({ expires_at: null }), 1e12)).toBe(false)
  })
})

describe('view', () => {
  const many = Array.from({ length: 5 }, (_, i) => card({ id: `n${i}` }))

  it('keeps the newest on screen and folds the rest into a count', () => {
    const { shown, hidden } = view(many, false)
    expect(shown.map((n) => n.id)).toEqual(['n2', 'n3', 'n4'])
    expect(hidden).toBe(2)
    expect(shown).toHaveLength(VISIBLE)
  })

  it('renders the newest last, so it sits nearest the corner', () => {
    expect(view(many, false).shown.at(-1)?.id).toBe('n4')
  })

  it('shows everything once expanded', () => {
    const { shown, hidden } = view(many, true)
    expect(shown).toHaveLength(5)
    expect(hidden).toBe(0)
  })

  it('does not fold a stack that already fits', () => {
    const few = many.slice(0, 3)
    expect(view(few, false)).toEqual({ shown: few, hidden: 0 })
    expect(view([], false)).toEqual({ shown: [], hidden: 0 })
  })
})

describe('replyPayload', () => {
  it('treats a blank box as no text, so the button stands', () => {
    expect(replyPayload('sqlite', '   ')).toEqual({ action: 'sqlite', text: null })
    expect(replyPayload(null, '\n\t ')).toEqual({ action: null, text: null })
  })

  it('keeps what was typed, trimmed', () => {
    expect(replyPayload(null, '  sqlite, WAL on  ')).toEqual({
      action: null,
      text: 'sqlite, WAL on',
    })
  })

  it('carries both when a suggestion was clicked and then edited', () => {
    expect(replyPayload('sqlite', 'sqlite, WAL on')).toEqual({
      action: 'sqlite',
      text: 'sqlite, WAL on',
    })
  })

  it('records a dismissal as an answer with nothing in it', () => {
    // This is what stops a dismissed card coming back after a restart.
    expect(replyPayload(null, '')).toEqual({ action: null, text: null })
  })
})
