import { useEffect, useRef, useState } from 'react'

/**
 * Dictation into a text field, with the browser's own speech recognition.
 *
 * No dependency and no server: `SpeechRecognition` is the platform's, and on
 * Chrome it streams audio to the browser vendor's recogniser — which is the
 * user's browser choice, not something the daemon sees or configures.
 *
 * It only ever writes into the field. Sending stays a separate, deliberate
 * press: a misheard "abort the run" should be something you read before it
 * goes, not something that already went.
 *
 * Browsers expose it only in a secure context, so over plain-HTTP LAN the
 * button simply is not there — see docs/WEB-UI.md for putting TLS in front.
 */

type Alt = { transcript: string }
type RecognitionEvent = { results: ArrayLike<ArrayLike<Alt>> }
type Recognition = {
  lang: string
  interimResults: boolean
  continuous: boolean
  onresult: ((e: RecognitionEvent) => void) | null
  onerror: ((e: { error: string }) => void) | null
  onend: (() => void) | null
  start(): void
  stop(): void
  abort(): void
}
type RecognitionCtor = new () => Recognition

/** The constructor, prefixed or not, or null where the browser has none. */
export function recognizer(scope: object = globalThis): RecognitionCtor | null {
  const s = scope as { SpeechRecognition?: RecognitionCtor; webkitSpeechRecognition?: RecognitionCtor }
  return s.SpeechRecognition ?? s.webkitSpeechRecognition ?? null
}

/**
 * What the field reads while dictating: whatever was typed before the mic
 * was tapped, then the transcript so far. Recomputed from `base` on every
 * result, so interim guesses replace each other instead of piling up.
 */
export function merge(base: string, transcript: string): string {
  const said = transcript.trim()
  if (!said) return base
  if (!base || /\s$/.test(base)) return base + said
  return `${base} ${said}`
}

/** Every result so far, interim included, as one string. */
export function transcriptOf(e: RecognitionEvent): string {
  return Array.from(e.results, (r) => r[0]?.transcript ?? '').join('')
}

/**
 * Start one dictation. Its only outputs are the text for the field and the
 * end of listening — there is deliberately no path from here to sending.
 */
export function listen(
  Ctor: RecognitionCtor,
  base: string,
  lang: string,
  on: { text: (t: string) => void; problem: (p: string) => void; end: () => void },
): Recognition {
  const r = new Ctor()
  r.lang = lang
  r.interimResults = true
  // Not continuous: the recogniser ends on its own after a pause, which is
  // the "silence stops it" half of the interaction.
  r.continuous = false
  r.onresult = (e) => on.text(merge(base, transcriptOf(e)))
  r.onerror = (e) => {
    if (e.error === 'not-allowed' || e.error === 'service-not-allowed')
      on.problem('Microphone blocked — allow it in the browser')
    else if (e.error !== 'no-speech' && e.error !== 'aborted') on.problem(`Dictation failed: ${e.error}`)
  }
  r.onend = on.end
  r.start()
  return r
}

/**
 * The mic toggle. Renders nothing when the browser cannot recognise speech,
 * so an unsupported phone shows a normal composer rather than a dead button.
 *
 * `type="button"` is load-bearing: it sits inside the composer's `<form>`,
 * where a bare `<button>` is a submit button and tapping the mic would send.
 */
export function MicButton({
  value,
  onChange,
  disabled,
}: {
  value: string
  onChange: (text: string) => void
  disabled?: boolean
}) {
  const [listening, setListening] = useState(false)
  const [problem, setProblem] = useState<string | null>(null)
  const rec = useRef<Recognition | null>(null)

  // Same rebuild-avoidance as `a11y.tsx`: the handlers below read the latest
  // callback without being reinstalled on every keystroke.
  const change = useRef(onChange)
  change.current = onChange

  useEffect(() => () => rec.current?.abort(), [])

  const Ctor = recognizer()
  if (!Ctor) return null

  function toggle() {
    if (rec.current) return rec.current.stop()
    setProblem(null)
    try {
      rec.current = listen(Ctor!, value, navigator.language, {
        text: (t) => change.current(t),
        problem: setProblem,
        end: () => {
          rec.current = null
          setListening(false)
        },
      })
      setListening(true)
    } catch (e) {
      // `start()` throws synchronously on some refusals rather than erroring.
      setProblem(`Dictation failed: ${e instanceof Error ? e.message : String(e)}`)
    }
  }

  return (
    <>
      <span className="sr-only" role="status">
        {listening ? 'Listening' : ''}
      </span>
      {problem && (
        <span className="mic-problem is-failed" role="alert">
          {problem}
        </span>
      )}
      <button
        type="button"
        className={`button button-icon mic${listening ? ' is-listening' : ''}`}
        aria-pressed={listening}
        aria-label={listening ? 'Stop dictation' : 'Dictate'}
        title={listening ? 'Stop dictation' : 'Dictate — you still press Send'}
        disabled={disabled && !listening}
        onClick={toggle}
      >
        <MicGlyph />
      </button>
    </>
  )
}

function MicGlyph() {
  return (
    <svg
      width="16"
      height="16"
      viewBox="0 0 20 20"
      fill="none"
      stroke="currentColor"
      strokeWidth="1.5"
      strokeLinecap="round"
      aria-hidden="true"
      focusable="false"
    >
      <path d="M10 2.5a2.5 2.5 0 0 1 2.5 2.5v4.5a2.5 2.5 0 0 1-5 0V5A2.5 2.5 0 0 1 10 2.5ZM5 9.5a5 5 0 0 0 10 0M10 14.5v3" />
    </svg>
  )
}
