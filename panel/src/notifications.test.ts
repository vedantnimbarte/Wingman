import { describe, expect, it } from 'vitest'
import { commits, interrupts, replyPayload, tone } from './Notifications'
import type { WingmanNotification } from './api'

function card(over: Partial<WingmanNotification> = {}): WingmanNotification {
  return {
    id: 'n1',
    severity: 'info',
    title: 'something happened',
    created_at: 0,
    ...over,
  }
}

describe('tone', () => {
  it('maps severity onto the three status hues and nothing else', () => {
    expect(tone('escalation')).toBe('failed')
    expect(tone('decision')).toBe('asserted')
    expect(tone('progress')).toBe('proven')
  })

  it('falls back to neutral rather than inventing a fourth hue', () => {
    expect(tone('info')).toBe('neutral')
    // A severity from a newer writer must not crash an older panel.
    expect(tone('whatever-comes-next')).toBe('neutral')
  })
})

describe('replyPayload', () => {
  it('treats a blank box as no answer', () => {
    // Sending `""` would record that the user answered with an empty string,
    // which is not the same as dismissing.
    expect(replyPayload(null, '   ')).toEqual({ action: null, text: null })
  })

  it('trims what it does send', () => {
    expect(replyPayload('a', '  sqlite  ')).toEqual({ action: 'a', text: 'sqlite' })
  })

  it('keeps the action when the box is empty', () => {
    expect(replyPayload('approve', '')).toEqual({ action: 'approve', text: null })
  })
})

describe('commits', () => {
  it('is true only for a button carrying a control command', () => {
    expect(commits({ id: 'a', label: 'Approve', control: { cmd: 'approve' } })).toBe(true)
    expect(commits({ id: 'b', label: 'postgres' })).toBe(false)
  })
})

describe('interrupts', () => {
  it('rings for anything waiting on a person', () => {
    expect(interrupts(card({ free_text: true }))).toBe(true)
    expect(
      interrupts(card({ actions: [{ id: 'approve', label: 'Approve', control: { cmd: 'a' } }] })),
    ).toBe(true)
  })

  it('stays quiet for a card that only reports something', () => {
    // This is the noise the old run-event filter existed to avoid, kept as a
    // property of the card rather than of the event that produced it.
    expect(interrupts(card({ severity: 'progress', title: 'Run finished' }))).toBe(false)
    expect(interrupts(card({ actions: [] }))).toBe(false)
  })
})
