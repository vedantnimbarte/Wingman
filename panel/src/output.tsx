import { isTextOutput } from './api'

/**
 * Rendering what a command printed.
 *
 * Its own module because four screens need it — Insights runs the report
 * table, Changes runs diff/explain/review/attest, the Overview runs the
 * maintenance list, and a pilot run shows the orchestrator's stdout. It lived
 * in `Insights` until the run screen needed it too, at which point importing
 * it from there made `Runs` and `Insights` import each other. A cycle that
 * happens to work because function declarations hoist is still a cycle.
 */

/**
 * Render whatever a table route returned.
 *
 * The routes promise exactly this: output that parses as JSON comes back as
 * JSON, anything else as `{stdout, stderr, exit}` which "is honest about being
 * text". So this checks which it got instead of assuming.
 */
export function Output({ value }: { value: unknown }) {
  if (isTextOutput(value)) {
    return (
      <>
        {value.exit !== 0 && <p className="is-failed dot figure">exited {value.exit}</p>}
        {value.stdout ? (
          <Report text={value.stdout} />
        ) : (
          <pre className="report figure">(no output)</pre>
        )}
        {value.stderr.trim() && <pre className="report figure is-failed">{value.stderr}</pre>}
      </>
    )
  }
  return <pre className="report figure">{JSON.stringify(value, null, 2)}</pre>
}

/**
 * A command's stdout, with its verdicts carrying the palette.
 *
 * `doctor` is the report the README brags about — which containment is
 * actually active on this machine — and it rendered as an undifferentiated
 * block of terminal text. It already marks each line with a glyph, so this
 * gives those lines the hue that glyph already means. Nothing is *invented*:
 * a line with no verdict marker gets no colour, which is the same rule the
 * rest of the panel follows.
 */
export function Report({ text }: { text: string }) {
  return (
    <pre className="report figure">
      {stripAnsi(text)
        .split('\n')
        .map((line, i) => {
          const cls = verdict(line)
          return (
            <span key={i} className={cls ?? undefined}>
              {line}
              {'\n'}
            </span>
          )
        })}
    </pre>
  )
}

/** The three states, from the glyphs the CLI already prints. */
export function verdict(line: string): string | null {
  const t = line.trimStart()
  if (/^[✓✔]/.test(t)) return 'is-proven'
  if (/^[✗✕✘×]/.test(t)) return 'is-failed'
  if (/^[⚠!]/.test(t)) return 'is-asserted'
  return null
}

/**
 * Drop ANSI colour from captured output.
 *
 * A pilot log is a real process's stdout and `tracing` writes it with colour
 * when the pilot had a terminal, so the file holds escape sequences a browser
 * renders as literal `[2m` garbage in front of every line. Stripped at render
 * rather than on the server: the route's job is to hand over the file as it
 * is, and this is a presentation problem.
 */
export function stripAnsi(text: string): string {
  // CSI: ESC [ , parameter bytes, intermediates, then a final byte in @-~.
  // eslint-disable-next-line no-control-regex
  return text.replace(/\u001b\[[0-9;?]*[ -/]*[@-~]/g, '')
}
