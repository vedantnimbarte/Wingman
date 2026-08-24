import { useCallback, useEffect, useState } from 'react'
import {
  api,
  isTextOutput,
  type ApiSchema,
  type ContextReport,
  type CostReport,
  type RouteInfo,
} from './api'
import { money } from './Board'
import { navigate } from './router'
import { message } from './state'
import { Failed, Loading } from './ui'

/**
 * Insights — cost, the per-turn context tax, and the long tail of reports.
 *
 * Zero new server code. Cost and context return real structure and get real
 * charts; everything else is a CLI command behind the route table and returns
 * whatever it printed, so it is rendered as what it is rather than dressed up
 * as data it isn't.
 *
 * Charts are bars drawn in CSS. A charting library would be a dependency and a
 * bundle for two horizontal bar lists, and the design rule here — colour means
 * epistemic status, cost gets typography — is easier to keep without one.
 */
export function Insights({ project }: { project: string | null }) {
  if (!project) {
    return (
      <div className="view">
        <Failed
          title="No project selected"
          detail="These reports are per-repo. Pick one in the header."
          action={{ label: 'Go to Overview', onClick: () => navigate('/') }}
        />
      </div>
    )
  }
  return (
    <div className="view">
      <span className="eyebrow">Insights</span>
      <h1>What this repo has cost</h1>
      <Cost project={project} />
      <Context project={project} />
      <Reports project={project} />
    </div>
  )
}

/* ── Cost ──────────────────────────────────────────────────────────────── */

function Cost({ project }: { project: string }) {
  const [report, setReport] = useState<CostReport | null>(null)
  const [error, setError] = useState<string | null>(null)

  const load = useCallback(async () => {
    try {
      // Always with `compare`: repricing the same token volume against other
      // models is the point of the report, not an extra.
      setReport(await api.cost(project, true))
      setError(null)
    } catch (e) {
      setError(message(e))
    }
  }, [project])

  useEffect(() => {
    void load()
  }, [load])

  if (error)
    return <Failed title="Could not read cost" detail={error} action={{ label: 'Try again', onClick: () => void load() }} />
  if (!report) return <Loading what="cost" />

  if (report.rows.length === 0) {
    return (
      <div className="state">
        <h2>Nothing spent yet</h2>
        <p>Cost accrues as you run turns and pilot runs in this repo.</p>
      </div>
    )
  }

  // The comparison list includes the model you are already on, and its figure
  // is your actual spend to the last decimal — the server derives both from
  // the same token volume. Charting it beside the "you" bar is the same number
  // twice, so it is dropped rather than labelled.
  const used = new Set(report.rows.map((r) => r.key))
  const alternatives = report.comparison.filter((c) => !used.has(c.model))

  // The scale has to cover the actual spend as well as every alternative, or
  // the "you are here" bar runs off the end whenever the model you chose is
  // the expensive one.
  const ceiling = Math.max(report.total_usd, ...alternatives.map((c) => c.would_cost_usd))
  const cheapest = alternatives.reduce<number | null>(
    (min, c) => (min === null ? c.would_cost_usd : Math.min(min, c.would_cost_usd)),
    null,
  )

  return (
    <section>
      <p className="headline figure">{money(report.total_usd)}</p>
      <p className="view-intro">
        {report.total_input_tokens.toLocaleString()} tokens in ·{' '}
        {report.total_output_tokens.toLocaleString()} out, across{' '}
        {report.rows.length === 1 ? '1 model' : `${report.rows.length} models`}.
      </p>

      <h2 className="section-head">By model</h2>
      <div className="rows">
        {report.rows.map((r) => (
          <div key={r.key} className="row">
            <span className="figure">{r.key}</span>
            <span className="figure">
              {money(r.usd)}
              <br />
              <span className="muted">
                {r.input_tokens.toLocaleString()} in · {r.output_tokens.toLocaleString()} out
              </span>
            </span>
          </div>
        ))}
      </div>

      {alternatives.length > 0 && (
        <>
          <h2 className="section-head">What the same work would have cost</h2>
          <p className="view-intro">
            Your real token volume, repriced. Only a provider-agnostic agent can show you this
            number — and it is a price comparison, not a recommendation: a cheaper model that needs
            three attempts is not cheaper.
          </p>
          <div className="bars">
            {[
              { model: report.rows[0]?.key ?? 'this repo', usd: report.total_usd, actual: true },
              ...alternatives.map((c) => ({
                model: c.model,
                usd: c.would_cost_usd,
                actual: false,
              })),
            ]
              .sort((a, b) => a.usd - b.usd)
              .map((b) => (
                <div key={b.model} className={`bar-row${b.actual ? ' bar-actual' : ''}`}>
                  <span className="figure bar-label">
                    {b.model}
                    {b.actual && <span className="muted"> · you</span>}
                  </span>
                  <span className="bar-track">
                    <span
                      className="bar-fill"
                      style={{ width: `${ceiling > 0 ? (b.usd / ceiling) * 100 : 0}%` }}
                    />
                  </span>
                  <span className="figure bar-value">{money(b.usd)}</span>
                </div>
              ))}
          </div>
          {cheapest !== null && cheapest < report.total_usd && (
            <p className="view-intro">
              The cheapest option priced here would have been {money(report.total_usd - cheapest)}{' '}
              less for the identical token volume.
            </p>
          )}
        </>
      )}
    </section>
  )
}

/* ── Context ───────────────────────────────────────────────────────────── */

function Context({ project }: { project: string }) {
  const [report, setReport] = useState<ContextReport | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [all, setAll] = useState(false)

  useEffect(() => {
    let live = true
    api
      .context(project)
      .then((r) => live && setReport(r))
      .catch((e: unknown) => live && setError(message(e)))
    return () => {
      live = false
    }
  }, [project])

  if (error) return <p className="is-failed dot figure">{error}</p>
  if (!report) return null

  const shown = all ? report.tools : report.tools.slice(0, 8)
  const widest = report.tools[0]?.tokens ?? 1

  return (
    <section>
      <h2 className="section-head">The per-turn tax</h2>
      <p className="view-intro">
        What every turn pays before you type anything. Most agents never tell you this number.
      </p>

      <div className="rows">
        <div className="row">
          <span className="muted">System prompt</span>
          <span className="figure">{report.system_prompt_tokens.toLocaleString()} tokens</span>
        </div>
        <div className="row">
          <span className="muted">Tool schemas</span>
          <span className="figure">
            {report.tool_schema_tokens.toLocaleString()} tokens
            <span className="muted"> · {report.tool_count} tools</span>
          </span>
        </div>
        <div className="row">
          <span>First turn</span>
          <span className="figure">{report.first_turn_tokens.toLocaleString()} tokens</span>
        </div>
      </div>

      <h3 className="section-head">Cost per tool</h3>
      <div className="bars">
        {shown.map((t) => (
          <div key={t.name} className="bar-row">
            <span className="figure bar-label">{t.name}</span>
            <span className="bar-track">
              <span
                className="bar-fill"
                style={{ width: `${widest > 0 ? (t.tokens / widest) * 100 : 0}%` }}
              />
            </span>
            <span className="figure bar-value">{t.tokens}</span>
          </div>
        ))}
      </div>
      {report.tools.length > shown.length && (
        <button type="button" className="button button-quiet" onClick={() => setAll(true)}>
          Show all {report.tools.length}
        </button>
      )}
    </section>
  )
}

/* ── The long tail ─────────────────────────────────────────────────────── */

/**
 * Every other read route, listed from `GET /v1/schema`.
 *
 * The route table is generated from the table the server dispatches on and
 * "cannot drift from the implementation — they are the same array", so this
 * list gains a report the day one is added, with the description the table
 * already carries.
 */
function Reports({ project }: { project: string }) {
  const [schema, setSchema] = useState<ApiSchema | null>(null)
  const [open, setOpen] = useState<string | null>(null)
  const [result, setResult] = useState<unknown>(null)
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)

  useEffect(() => {
    let live = true
    api
      .apiSchema()
      .then((s) => live && setSchema(s))
      .catch(() => {
        /* The reports list is optional furniture; cost above is the report. */
      })
    return () => {
      live = false
    }
  }, [])

  if (!schema) return null

  // Read-only, project-scoped, no path parameter left to fill, and not one of
  // the surfaces that already has a screen of its own.
  const own = ['/cost', '/context', '/pilot', '/sessions']
  const routes = schema.routes.filter(
    (r) =>
      r.method === 'GET' &&
      r.path.includes('{project}') &&
      !r.path.includes('{run}') &&
      !r.path.includes('{id}') &&
      !own.some((o) => r.path.includes(o)),
  )

  async function run(r: RouteInfo) {
    const tail = r.path.split('{project}/')[1]
    setOpen(tail)
    setResult(null)
    setError(null)
    setBusy(true)
    try {
      setResult(await api.report(project, tail))
    } catch (e) {
      setError(message(e))
    } finally {
      setBusy(false)
    }
  }

  return (
    <section>
      <h2 className="section-head">Reports</h2>
      <p className="view-intro">
        Listed from <code>GET /v1/schema</code>, which is generated from the server's own route
        table — so a report added to the CLI appears here without this page changing.
      </p>

      <div className="rows">
        {routes.map((r) => {
          const tail = r.path.split('{project}/')[1]
          return (
            <div key={r.path} className="row">
              <button type="button" className="task-toggle" onClick={() => void run(r)}>
                {tail}
                <span className="task-meta muted">
                  {r.about ?? r.returns ?? ''}
                  {r.runs && <span className="figure"> · {r.runs}</span>}
                </span>
              </button>
              {open === tail && busy && <span className="figure muted">running…</span>}
            </div>
          )
        })}
      </div>

      {error && (
        <p className="is-failed dot figure" role="alert">
          {error}
        </p>
      )}

      {open && result != null && (
        <>
          <h3 className="section-head figure">{open}</h3>
          <Output value={result} />
        </>
      )}
    </section>
  )
}

/**
 * Render whatever a table route returned.
 *
 * The routes promise exactly this: output that parses as JSON comes back as
 * JSON, anything else as `{stdout, stderr, exit}` which "is honest about being
 * text". So this checks which it got instead of assuming.
 */
function Output({ value }: { value: unknown }) {
  if (isTextOutput(value)) {
    return (
      <>
        {value.exit !== 0 && (
          <p className="is-failed dot figure">exited {value.exit}</p>
        )}
        <pre className="report figure">{value.stdout || '(no output)'}</pre>
        {value.stderr.trim() && <pre className="report figure is-failed">{value.stderr}</pre>}
      </>
    )
  }
  return <pre className="report figure">{JSON.stringify(value, null, 2)}</pre>
}
