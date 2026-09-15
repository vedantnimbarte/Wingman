import { renderToStaticMarkup } from 'react-dom/server'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { api, ApiError } from './api'
import { listen, merge, MicButton, recognizer } from './voice'

/**
 * Phone steering: dictation and pairing.
 *
 * The recogniser is faked — what is under test is the panel's side of it:
 * that the button is absent where it cannot work, that speech lands in the
 * field without eating what was typed, and that nothing here sends.
 */

class FakeRecognition {
  static last: FakeRecognition | null = null
  lang = ''
  interimResults = false
  continuous = true
  onresult: ((e: { results: ArrayLike<ArrayLike<{ transcript: string }>> }) => void) | null = null
  onerror: ((e: { error: string }) => void) | null = null
  onend: (() => void) | null = null
  started = false
  start() {
    this.started = true
    FakeRecognition.last = this
  }
  stop() {}
  abort() {}
}

afterEach(() => {
  vi.unstubAllGlobals()
})

describe('feature detection', () => {
  it('finds the prefixed constructor, and nothing where there is none', () => {
    expect(recognizer({})).toBeNull()
    expect(recognizer({ webkitSpeechRecognition: FakeRecognition })).toBe(FakeRecognition)
  })

  it('renders no mic button when the browser cannot recognise speech', () => {
    expect(renderToStaticMarkup(<MicButton value="" onChange={() => {}} />)).toBe('')
  })

  it('renders a labelled toggle that cannot submit the form it sits in', () => {
    vi.stubGlobal('webkitSpeechRecognition', FakeRecognition)
    const html = renderToStaticMarkup(<MicButton value="" onChange={() => {}} />)
    expect(html).toContain('type="button"')
    expect(html).toContain('aria-pressed="false"')
    expect(html).toContain('aria-label="Dictate"')
  })
})

describe('merge', () => {
  it('appends speech after what was typed, with one space', () => {
    expect(merge('fix the', ' failing tests ')).toBe('fix the failing tests')
    expect(merge('fix the ', 'tests')).toBe('fix the tests')
    expect(merge('first line\n', 'second')).toBe('first line\nsecond')
  })

  it('leaves the field alone when nothing was heard', () => {
    expect(merge('draft', '   ')).toBe('draft')
    expect(merge('', 'hello')).toBe('hello')
  })
})

describe('listen', () => {
  it('shows interim text, replaces it with the final, and never sends', () => {
    const text = vi.fn()
    const end = vi.fn()
    const problem = vi.fn()
    listen(FakeRecognition, 'also', 'en-GB', { text, problem, end })
    const r = FakeRecognition.last!
    expect(r.started).toBe(true)
    expect(r.interimResults).toBe(true)
    expect(r.continuous).toBe(false)

    r.onresult!({ results: [[{ transcript: 'update the' }]] })
    r.onresult!({ results: [[{ transcript: 'update the' }], [{ transcript: ' changelog' }]] })
    r.onend!()

    // Interim guesses replace each other rather than piling up after `also`.
    expect(text.mock.calls.map((c) => c[0])).toEqual([
      'also update the',
      'also update the changelog',
    ])
    expect(end).toHaveBeenCalledTimes(1)
    expect(problem).not.toHaveBeenCalled()
  })

  it('says when the microphone is blocked, and stays quiet about silence', () => {
    const problem = vi.fn()
    listen(FakeRecognition, '', 'en', { text: () => {}, problem, end: () => {} })
    FakeRecognition.last!.onerror!({ error: 'no-speech' })
    expect(problem).not.toHaveBeenCalled()
    FakeRecognition.last!.onerror!({ error: 'not-allowed' })
    expect(problem).toHaveBeenCalledWith('Microphone blocked — allow it in the browser')
  })
})

describe('api.pair', () => {
  it('redeems the code, then hands the token to the cookie sign-in', async () => {
    const calls: { url: string; body: unknown }[] = []
    vi.stubGlobal(
      'fetch',
      vi.fn(async (url: string, init: RequestInit) => {
        calls.push({ url, body: JSON.parse(String(init.body)) })
        const payload = url === '/v1/pair/redeem' ? { token: 'secret-token' } : { auth_required: true }
        return new Response(JSON.stringify(payload), { status: 200 })
      }),
    )

    await api.pair('  the-code \n')

    expect(calls).toEqual([
      { url: '/v1/pair/redeem', body: { code: 'the-code' } },
      { url: '/v1/ui/session', body: { token: 'secret-token' } },
    ])
    // The token travels in a body, never a URL an access log would keep.
    expect(calls.some((c) => c.url.includes('secret-token'))).toBe(false)
  })

  it('stops at a refused code without trying to sign in', async () => {
    const fetch = vi.fn(
      async () => new Response(JSON.stringify({ error: 'invalid pairing code' }), { status: 401 }),
    )
    vi.stubGlobal('fetch', fetch)

    await expect(api.pair('wrong')).rejects.toEqual(new ApiError(401, 'invalid pairing code'))
    expect(fetch).toHaveBeenCalledTimes(1)
  })
})
