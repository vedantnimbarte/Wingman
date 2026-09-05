import { useState } from 'react'
import { replyPayload, tone, type Notification } from './wire'

/**
 * One notification.
 *
 * Every card can be dismissed and that dismissal is recorded, because "I have
 * seen this" is the answer to a card with no buttons — and it is what stops the
 * thing reappearing the next time the app starts.
 */
export function Card({
  n,
  onAnswer,
}: {
  n: Notification
  onAnswer: (id: string, action: string | null, text: string | null) => void
}) {
  const [chosen, setChosen] = useState<string | null>(null)
  const [text, setText] = useState('')

  function answer(action: string | null, typed = text) {
    const { action: a, text: t } = replyPayload(action, typed)
    onAnswer(n.id, a, t)
  }

  // A control button is a decision, not a draft: clicking Approve must not
  // wait for anything else on the card. A suggestion on a question is the
  // opposite — it fills the box so it can still be edited before sending.
  const commits = (a: { control?: unknown }) => a.control !== undefined && a.control !== null

  return (
    <div className={`card ${tone(n.severity)}`}>
      <div className="head">
        <span className="title">{n.title}</span>
        {n.project && <span className="project">{n.project}</span>}
        <button
          className="quiet"
          title="Dismiss"
          aria-label="Dismiss"
          onClick={() => onAnswer(n.id, null, null)}
        >
          ✕
        </button>
      </div>

      {n.body && <div className="body">{n.body}</div>}

      {n.actions.length > 0 && (
        <div className="row">
          {n.actions.map((a, i) =>
            commits(a) ? (
              <button
                key={a.id}
                className={i === 0 ? 'primary' : undefined}
                onClick={() => answer(a.id, '')}
              >
                {a.label}
              </button>
            ) : (
              <button
                key={a.id}
                className="chip"
                aria-pressed={chosen === a.id}
                onClick={() => {
                  setChosen(a.id)
                  // Fill the box rather than sending: a suggestion is a
                  // starting point, and the answer nobody listed is often the
                  // right one.
                  if (n.free_text) setText(a.label)
                  else answer(a.id, '')
                }}
              >
                {a.label}
              </button>
            ),
          )}
        </div>
      )}

      {n.free_text && (
        <form
          className="row"
          onSubmit={(e) => {
            e.preventDefault()
            answer(chosen)
          }}
        >
          <input
            type="text"
            autoFocus
            placeholder="Type an answer…"
            value={text}
            onChange={(e) => setText(e.target.value)}
          />
          <button className="primary" type="submit">
            Send
          </button>
        </form>
      )}
    </div>
  )
}
