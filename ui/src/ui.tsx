/**
 * The shared loading, failure, and empty states.
 *
 * Defined once, here, on purpose: five feature phases will otherwise each
 * invent their own, and a panel where every section fails differently is a
 * panel nobody trusts.
 *
 * The voice: say what happened and what to do about it. No apologies, no
 * vagueness about which thing broke, and an empty screen is an invitation to
 * act rather than a shrug.
 */

export function Loading({ what }: { what: string }) {
  return (
    <div className="state">
      <p className="muted">Loading {what}…</p>
    </div>
  )
}

/**
 * `action` names what the button does, because "Try again" on a page that was
 * never going to exist is a lie about what the click will accomplish. A
 * control says exactly what happens when it is used.
 */
export function Failed({
  title,
  detail,
  action,
}: {
  title: string
  detail?: string
  action?: { label: string; onClick: () => void }
}) {
  return (
    <div className="state">
      <h2 className="is-failed dot">{title}</h2>
      {detail && <p className="figure">{detail}</p>}
      {action && (
        <button type="button" className="button" onClick={action.onClick}>
          {action.label}
        </button>
      )}
    </div>
  )
}

/**
 * A section the panel routes to but has not built yet.
 *
 * Naming the phase and what already works is the difference between "this
 * product is unfinished" and "this part arrives next" — and it stops someone
 * filing a bug against a screen that was never claimed to exist.
 */
export function NotYet({
  title,
  phase,
  children,
}: {
  title: string
  phase: string
  children: React.ReactNode
}) {
  return (
    <div className="view">
      <span className="eyebrow">{phase}</span>
      <h1>{title}</h1>
      <div className="state">
        <p>{children}</p>
        <p>
          Everything here works from the terminal today — see <code>docs/WEB-UI-PLAN.md</code> for
          what lands when.
        </p>
      </div>
    </div>
  )
}
