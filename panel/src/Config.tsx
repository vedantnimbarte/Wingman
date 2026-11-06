import { useCallback, useEffect, useMemo, useState } from 'react'
import { useUnsavedWarning } from './a11y'
import { api, type ConfigSchema, type SchemaNode } from './api'
import { Providers } from './Providers'
import { navigate } from './router'
import { at, fieldsOf, optionsFor, resolve, toPatch, type Field } from './schema'
import { message } from './state'
import { Failed, Icon, Loading, Note, type IconName } from './ui'
import { Picker } from './Picker'
import './settings.css'

const REDACTED = '<redacted>'

/** Synthetic section holding the bare top-level keys. Never a real config table. */
const GENERAL = 'general'

/**
 * Settings.
 *
 * A sidebar of pages. Models & providers is its own screen; the rest are
 * curated groups of the settings people actually change, each still generated
 * from `GET /v1/config/schema` — a page names the *paths* it shows, and the
 * label, help text, control and default all come from the `wingman-config`
 * structs. A path the schema does not have is skipped, so a page cannot drift
 * into showing a field that no longer exists.
 *
 * Advanced is the complete generated form, section by section, so nothing in
 * the config is unreachable from here.
 *
 * The panel does **no validation of its own**. `PATCH /v1/config` round-trips
 * the result through the real config parser and returns its error, so there is
 * one validator and it is the one that actually has to load the file.
 */

type Group = {
  title: string
  about: string
  /** Full config paths. A trailing `*` takes every field under that table. */
  paths: Spec[]
}

type Page = {
  id: string
  label: string
  icon: IconName
  intro?: string
  groups?: Group[]
}

/** `path`, or a path with the plain label and one-line help a newcomer needs.
 *  The schema's own description stays reachable under "Details". */
type Spec = string | { path: string; label: string; help: string }

const f = (path: string, label: string, help: string): Spec => ({ path, label, help })

const PAGES: Page[] = [
  { id: 'providers', label: 'Models & providers', icon: 'spark' },
  {
    id: 'general',
    label: 'General',
    icon: 'config',
    intro: 'How Wingman works for you by default, and how much it may spend.',
    groups: [
      {
        title: 'How Wingman works',
        about: 'What it may do without asking, and how much it thinks before answering.',
        paths: [
          f('permission_mode', 'What it may do without asking', 'Read only never changes anything. Plan proposes changes first. Auto-edit edits files on its own. Yolo also runs any command — use it only in a sandbox.'),
          f('reasoning', 'Thinking effort', 'How much the model thinks before answering. Higher is slower and costs more; off is fastest.'),
        ],
      },
      {
        title: 'Spending limits',
        about: 'Guard rails on what a chat or a run may cost.',
        paths: [
          f('tokens.max_usd_per_session', 'Warn when a chat costs more than (USD)', 'You get one warning when a chat passes this. Leave empty for no warning.'),
          f('pilot.max_usd', 'Stop a run that costs more than (USD)', 'A run halts before it spends more than this.'),
        ],
      },
      {
        title: 'Privacy',
        about: 'What may leave this machine.',
        paths: [
          f('privacy.local_only', 'Keep everything on this machine', 'Only providers that run locally are allowed; anything that would call a cloud service is refused.'),
          f('tools.redact_output_secrets', 'Hide secrets from the model', 'API keys, tokens and private keys found in command output are blanked out before the model sees them.'),
        ],
      },
      {
        title: 'Terminal app',
        about: 'How `wingman` looks in your terminal.',
        paths: [
          f('tui.theme', 'Colour theme', 'Default, light, or mono for terminals without colour.'),
          f('tui.show_token_usage', 'Show token usage', 'Show how many tokens each reply used.'),
        ],
      },
    ],
  },
  {
    id: 'tools',
    label: 'Tools & safety',
    icon: 'tasks',
    intro: 'What Wingman can run on your machine, and the guard rails around it.',
    groups: [
      {
        title: 'Safety',
        about: 'Where commands run, and whether they can reach the internet.',
        paths: [
          f('tools.shell_sandbox', 'Run commands in a sandbox', 'Auto uses the OS sandbox when one is available. Required refuses to run commands without it. Off never uses it.'),
          f('tools.allow_network', 'Allow web look-ups while planning', 'Lets Wingman fetch and search the web in read-only and plan modes too. Off by default so nothing can be sent out unnoticed.'),
          f('tools.shell_denylist', 'Never run these commands', 'Patterns that are always refused, even in yolo mode. One per line, e.g. sudo.'),
        ],
      },
      {
        title: 'Available tools',
        about: 'Every tool adds a little to the cost of each message, so turn off the ones you never use.',
        paths: [
          f('tools.preset', 'Tool set', 'Limit Wingman to a named set of tools, such as review or minimal. Empty means every tool.'),
          f('tools.disabled_tools', 'Turned-off tools', 'Tools Wingman will not use. One per line.'),
        ],
      },
      {
        title: 'Limits',
        about: 'How long a tool may run and how much of its output Wingman reads.',
        paths: [
          f('tools.tool_timeout_secs', 'Tool time limit (seconds)', 'A single tool call that runs longer than this is stopped.'),
          f('tokens.tool_output_max_lines', 'Lines of output to read', 'Longer output is cut to its start and end before the model reads it.'),
          f('tokens.compact_at_tokens', 'Summarise a long chat after (tokens)', 'Older messages are summarised once a chat grows past this, so it fits in the model’s memory.'),
          f('tokens.prompt_cache', 'Use prompt caching', 'Reuse repeated context at a lower price on providers that support it.'),
        ],
      },
    ],
  },
  {
    id: 'autopilot',
    label: 'Autopilot',
    icon: 'runs',
    intro: 'How runs plan and carry out work on their own.',
    groups: [
      {
        title: 'Models',
        about: 'Which models plan, do and review the work. Empty means your default model. Write them as provider/model.',
        paths: [
          f('pilot.default_model', 'Planning model', 'Plans the work and reviews it.'),
          f('pilot.worker_model', 'Worker model', 'Does each task. Often a cheaper model.'),
          f('pilot.reviewer_model', 'Reviewer model', 'Checks each finished task. Point it at a stronger model for tougher review.'),
          f('pilot.tier', 'How hands-off', 'Assist asks you at every step. Copilot is the everyday default. Autopilot also discovers work and reviews itself.'),
        ],
      },
      {
        title: 'Limits',
        about: 'A run stops when it reaches any of these.',
        paths: [
          f('pilot.max_usd', 'Spending limit (USD)', 'A run halts before it spends more than this.'),
          f('pilot.max_concurrent_agents', 'Agents at once', 'How many tasks may be worked on at the same time.'),
          f('pilot.max_retries_per_task', 'Retries per task', 'How many times a failing task is retried, with a stronger model or split up, before it is marked blocked.'),
          f('pilot.task_timeout_secs', 'Time limit per task (seconds)', 'A task that runs longer than this is stopped.'),
        ],
      },
      {
        title: 'Approval',
        about: 'A plan starts without waiting for you only when it is small, cheap and touches only allowed files. Everything else waits.',
        paths: [
          f('pilot.approval.auto_approve_max_tasks', 'Start on its own up to (tasks)', 'Plans with more tasks than this wait for you.'),
          f('pilot.approval.auto_approve_usd', 'Start on its own up to (USD)', 'Plans estimated to cost more than this wait for you.'),
          f('pilot.approval.auto_approve_globs', 'Files it may change without asking', 'File patterns, one per line. A plan that writes anywhere else waits for you.'),
          f('pilot.approval.dangerous_paths', 'Always ask before touching', 'File patterns, one per line. Plans that touch these always wait for your approval.'),
          f('pilot.approval.notify_only_window_secs', 'Time to stop a medium-risk plan (seconds)', 'Medium-risk plans tell you and start after this long unless you stop them.'),
        ],
      },
    ],
  },
  {
    id: 'advanced',
    label: 'Advanced',
    icon: 'filter',
    intro: 'Every setting Wingman has, section by section.',
  },
]

export function Config({ page }: { page?: string | null }) {
  const current = PAGES.find((p) => p.id === page) ?? PAGES[0]

  return (
    <div className="view settings">
      <div className="settings-layout">
        <nav className="settings-nav" aria-label="Settings pages">
          <span className="settings-nav-title">Settings</span>
          {PAGES.map((p) => (
            <button
              key={p.id}
              type="button"
              className="nav-item"
              aria-current={p.id === current.id ? 'page' : undefined}
              onClick={() => navigate(`/config/${p.id}`)}
            >
              <Icon name={p.icon} />
              <span className="nav-label">{p.label}</span>
            </button>
          ))}
        </nav>

        <div className="settings-page">
          {current.id === 'providers' ? <Providers /> : <FieldSettings key={current.id} page={current} />}
        </div>
      </div>
    </div>
  )
}

type Resolved = { field: Field; full: string[]; label?: string; help?: string }

/** The fields a page's group names, resolved against the schema. Missing paths are skipped. */
function groupFields(
  group: Group,
  props: Record<string, SchemaNode>,
  defs: Record<string, SchemaNode>,
): Resolved[] {
  const out: Resolved[] = []
  for (const spec of group.paths) {
    const path = typeof spec === 'string' ? spec : spec.path
    const plain = typeof spec === 'string' ? {} : { label: spec.label, help: spec.help }
    const parts = path.split('.')
    const wild = parts[parts.length - 1] === '*'
    const want = wild ? parts.slice(0, -1) : parts
    const [head, ...rest] = want
    if (!props[head]) continue
    const fields =
      rest.length === 0 && !wild
        ? fieldsOf({ type: 'object', properties: { [head]: props[head] } }, defs)
        : fieldsOf(props[head], defs)
    for (const f of fields) {
      const full = rest.length === 0 && !wild ? f.path : [head, ...f.path]
      const match = wild
        ? full.slice(0, want.length).join('.') === want.join('.')
        : full.join('.') === want.join('.')
      if (match) out.push({ field: f, full, ...plain })
    }
  }
  return out
}

function FieldSettings({ page }: { page: Page }) {
  const [meta, setMeta] = useState<ConfigSchema | null>(null)
  const [current, setCurrent] = useState<Record<string, unknown> | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [section, setSection] = useState<string | null>(null)
  const [edits, setEdits] = useState<Map<string, unknown>>(new Map())
  const [saving, setSaving] = useState(false)
  const [saveError, setSaveError] = useState<string | null>(null)
  const [saved, setSaved] = useState(false)
  const [query, setQuery] = useState('')
  const [preview, setPreview] = useState(false)

  // A floating save bar meant pending edits could leave with the tab and take
  // no notice. The panel routes in-page, so this covers the half the browser
  // gives a hook for and nothing more.
  useUnsavedWarning(edits.size > 0)

  const load = useCallback(async () => {
    try {
      const [m, c] = await Promise.all([api.configSchema(), api.config()])
      setMeta(m)
      setCurrent(c)
      setError(null)
    } catch (e) {
      setError(message(e))
    }
  }, [])

  useEffect(() => {
    void load()
  }, [load])

  // `Config` mixes tables with a handful of bare top-level keys
  // (`default_model`, `default_provider`). They are collected into a
  // synthetic first section rather than each getting a page of one control.
  const { general, sections } = useMemo(() => {
    const props = meta?.schema.properties ?? {}
    const defs = meta?.schema.definitions ?? {}
    const general: string[] = []
    const sections: string[] = []
    for (const key of Object.keys(props).sort()) {
      const node = resolve(props[key], defs)
      const isTable = Boolean(node.properties) || Boolean(node.additionalProperties)
      ;(isTable ? sections : general).push(key)
    }
    return { general, sections }
  }, [meta])

  if (error)
    return (
      <Failed
        title="Couldn't load settings"
        detail={error}
        action={{ label: 'Try again', onClick: () => void load() }}
      />
    )
  if (!meta || !current) return <Loading what="settings" />

  const defs = meta.schema.definitions ?? {}
  const props = meta.schema.properties ?? {}

  function value(full: string[]) {
    const k = full.join('.')
    return edits.has(k) ? edits.get(k) : at(current, full)
  }

  function edit(full: string[], v: unknown) {
    setSaved(false)
    setEdits((prev) => new Map(prev).set(full.join('.'), v))
  }

  async function save() {
    setSaving(true)
    setSaveError(null)
    setSaved(false)
    try {
      await api.patchConfig(toPatch(edits))
      setEdits(new Map())
      setSaved(true)
      await load()
    } catch (e) {
      setSaveError(message(e))
    } finally {
      setSaving(false)
    }
  }

  const rows = (list: Resolved[], readOnly = false) => (
    <div className="rows">
      {list.map(({ field, full, label, help }) => (
        <FieldRow
          key={full.join('.')}
          field={{ ...field, path: full }}
          label={label}
          help={help}
          value={value(full)}
          fallback={at(meta.defaults, full)}
          dirty={edits.has(full.join('.'))}
          readOnly={readOnly}
          onChange={(v) => edit(full, v)}
        />
      ))}
    </div>
  )

  let body: React.ReactNode
  if (page.groups) {
    body = page.groups.map((g) => {
      const list = groupFields(g, props, defs)
      if (list.length === 0) return null
      return (
        <section key={g.title} className="settings-card" aria-label={g.title}>
          <header>
            <h2>{g.title}</h2>
            <p>{g.about}</p>
          </header>
          {rows(list)}
        </section>
      )
    })
  } else {
    const all = general.length ? [GENERAL, ...sections] : sections
    const active = section && all.includes(section) ? section : all[0]
    const isGeneral = active === GENERAL
    const readOnly = meta.readonly_sections.includes(active)
    const resolved = isGeneral ? {} : resolve(props[active], defs)
    const fields = isGeneral
      ? fieldsOf({ type: 'object', properties: Object.fromEntries(general.map((k) => [k, props[k]])) }, defs)
      : fieldsOf(props[active], defs)
    const shown = all.filter((s) => humanize(s).toLowerCase().includes(query.trim().toLowerCase()))

    body = (
      <div className="config-body">
        <nav className="config-nav" aria-label="Config sections">
          <label className="filter-search config-find">
            <Icon name="search" size={14} />
            <input
              className="input"
              type="search"
              placeholder="Find a section"
              aria-label="Find a config section"
              value={query}
              onChange={(e) => setQuery(e.target.value)}
            />
          </label>
          {shown.map((s) => (
            <button
              key={s}
              type="button"
              className="nav-item"
              aria-current={s === active ? 'page' : undefined}
              onClick={() => setSection(s)}
            >
              <span className="nav-label">{humanize(s)}</span>
              {meta.readonly_sections.includes(s) && (
                <span className="count" title="Edit this section in the file directly">
                  locked
                </span>
              )}
            </button>
          ))}
        </nav>

        <section className="settings-card config-fields" aria-label={humanize(active)}>
          <header>
            <h2>{humanize(active)}</h2>
            {resolved.description && <p>{resolved.description}</p>}
          </header>

          {readOnly && (
            <Note tone="is-asserted">
              These settings can't be changed from the panel, because they control who can reach
              it. Edit <code>[{active}]</code> in the config file directly.
            </Note>
          )}

          {fields.length === 0 ? (
            <FreeForm
              // Keyed on the section, or React reuses the instance and its
              // seeded text between two form-less sections.
              key={active}
              value={at(current, [active])}
              node={resolved}
              readOnly={readOnly}
              onChange={(v) => edit([active], v)}
            />
          ) : (
            rows(
              fields.map((f) => ({ field: f, full: isGeneral ? f.path : [active, ...f.path] })),
              readOnly,
            )
          )}
        </section>
      </div>
    )
  }

  return (
    <>
      <header className="settings-head">
        <h1>{page.label}</h1>
        <p>
          {page.intro} Saved to <code>{meta.writes_to}</code> and applied to every repository.
        </p>
      </header>

      {body}

      {/* Shown only while there is something to save, so the page is not
          permanently topped by a disabled button. */}
      {(edits.size > 0 || saved || saveError) && (
        <div className="config-save">
          <span className="config-save-spacer">
            {saved && <span className="is-proven dot figure">Saved. New chats and runs use the new settings.</span>}
            {saveError && (
              <span className="is-failed dot figure" role="alert">
                {saveError}
              </span>
            )}
            {!saved && !saveError && edits.size > 0 && (
              <span className="faint figure">
                {edits.size} unsaved change{edits.size === 1 ? '' : 's'}
              </span>
            )}
          </span>
          {edits.size > 0 && (
            <button
              type="button"
              className="button button-quiet"
              aria-expanded={preview}
              onClick={() => setPreview((p) => !p)}
              title="Exactly what a save would send"
            >
              {preview ? 'Hide changes' : 'Review changes'}
            </button>
          )}
          {edits.size > 0 && (
            <button type="button" className="button button-quiet" onClick={() => setEdits(new Map())}>
              Discard
            </button>
          )}
          {edits.size > 0 && (
            <button type="button" className="button button-primary" disabled={saving} onClick={() => void save()}>
              {saving ? 'Saving…' : `Save ${edits.size} change${edits.size === 1 ? '' : 's'}`}
            </button>
          )}
        </div>
      )}

      {preview && edits.size > 0 && (
        <section className="settings-card">
          <header>
            <h2>This save sends</h2>
            <p>
              Merged into <code>{meta.writes_to}</code>. Settings not listed here are not touched.
            </p>
          </header>
          <pre className="report figure">{JSON.stringify(toPatch(edits), null, 2)}</pre>
        </section>
      )}
    </>
  )
}

/* ── One field ─────────────────────────────────────────────────────────── */

function FieldRow({
  field,
  label,
  help,
  value,
  fallback,
  dirty,
  readOnly,
  onChange,
}: {
  field: Field
  label?: string
  help?: string
  value: unknown
  fallback: unknown
  dirty: boolean
  readOnly: boolean
  onChange: (v: unknown) => void
}) {
  const id = field.path.join('-')
  // A credential comes back as `<redacted>`. Rendering that into an input
  // would offer to PATCH the literal string over the real key; rendering an
  // empty box would look like the key is unset. Neither, so: show that it is
  // set, and only send a value the user actually typed.
  const isRedacted = value === REDACTED

  return (
    <div className="row config-row">
      <label htmlFor={id} className="config-label">
        <span className={dirty ? 'config-dirty' : undefined}>
          {label ?? humanize(field.path[field.path.length - 1] ?? '')}
        </span>
        {help ? (
          <span className="config-help">{help}</span>
        ) : (
          field.description && <span className="config-help">{field.description}</span>
        )}
        {shownDefault(fallback) && !isRedacted && (
          <span className="config-default">Default: {shownDefault(fallback)}</span>
        )}
      </label>

      <span className="config-input">
        {isRedacted ? (
          <Redacted id={id} readOnly={readOnly} onChange={onChange} />
        ) : (
          <Input id={id} field={field} value={value} readOnly={readOnly} onChange={onChange} />
        )}
      </span>

      {/* The raw key and the developer documentation, for anyone editing the
          file by hand — out of the way of everyone else. */}
      <details className="config-more">
        <summary>Details</summary>
        <span className="config-key">{field.path.join('.')}</span>
        {help && field.description && <span className="config-help">{field.description}</span>}
      </details>
    </div>
  )
}

/** A default worth showing, as words — nothing for an empty one. */
function shownDefault(v: unknown): string {
  if (v === undefined || v === null || v === '') return ''
  if (Array.isArray(v)) return v.length ? v.join(', ') : ''
  if (typeof v === 'boolean') return v ? 'On' : 'Off'
  return stringify(v)
}

function Redacted({
  id,
  readOnly,
  onChange,
}: {
  id: string
  readOnly: boolean
  onChange: (v: unknown) => void
}) {
  const [replacing, setReplacing] = useState(false)
  if (!replacing) {
    return (
      <span className="config-redacted">
        <span className="figure muted">set · hidden</span>
        {!readOnly && (
          <button
            type="button"
            className="button button-sm"
            onClick={() => setReplacing(true)}
          >
            Replace
          </button>
        )}
      </span>
    )
  }
  return (
    <input
      id={id}
      className="input"
      type="password"
      autoFocus
      placeholder="New value"
      onChange={(e) => onChange(e.target.value)}
    />
  )
}

function Input({
  id,
  field,
  value,
  readOnly,
  onChange,
}: {
  id: string
  field: Field
  value: unknown
  readOnly: boolean
  onChange: (v: unknown) => void
}) {
  switch (field.kind) {
    case 'boolean':
      return (
        <input
          id={id}
          type="checkbox"
          disabled={readOnly}
          checked={value === true}
          onChange={(e) => onChange(e.target.checked)}
        />
      )

    case 'enum': {
      // The value on disk may legally be outside the schema's list — several
      // of these fields are `String` in Rust precisely because their parsers
      // accept aliases. `optionsFor` carries an unlisted value in rather than
      // letting the select fall back to its first option and misreport the
      // config it is editing.
      const options = optionsFor(field.choices, value)
      return (
        <Picker
          id={id}
          label={humanize(field.path[field.path.length - 1] ?? 'Value')}
          disabled={readOnly}
          value={typeof value === 'string' ? value : ''}
          onChange={onChange}
          placeholder="Not set"
          options={[
            ...(field.nullable
              ? [{ value: '', label: 'Not set', description: 'Use the built-in default.' }]
              : []),
            ...options.map((c) => ({
              value: c.value,
              label: c.value,
              muted: !c.listed,
              description: c.listed
                ? c.description
                : 'Set in your config and accepted, but not one of the documented values.',
              detail: c.listed ? undefined : 'not listed',
            })),
          ]}
        />
      )
    }

    case 'integer':
    case 'number':
      return (
        <input
          id={id}
          className="input"
          type="number"
          step={field.kind === 'integer' ? 1 : 'any'}
          disabled={readOnly}
          value={typeof value === 'number' ? value : ''}
          onChange={(e) => {
            const raw = e.target.value
            // An empty box means "unset", which is not the same as zero.
            if (raw === '') return onChange(null)
            const n = Number(raw)
            onChange(Number.isNaN(n) ? raw : n)
          }}
        />
      )

    case 'string-list':
      return (
        <textarea
          id={id}
          className="input config-area"
          rows={3}
          disabled={readOnly}
          value={Array.isArray(value) ? value.join('\n') : ''}
          placeholder="One per line"
          onChange={(e) =>
            onChange(
              e.target.value
                .split('\n')
                .map((s) => s.trim())
                .filter(Boolean),
            )
          }
        />
      )

    case 'json':
      return <FreeForm value={value} node={field.node} readOnly={readOnly} onChange={onChange} />

    default:
      return (
        <input
          id={id}
          className="input"
          type="text"
          disabled={readOnly}
          value={typeof value === 'string' ? value : ''}
          onChange={(e) => onChange(e.target.value === '' && field.nullable ? null : e.target.value)}
        />
      )
  }
}

/**
 * The escape hatch, for shapes a generated form cannot express: arrays of
 * objects like `[[hooks.pre_tool_use]]`, and maps keyed by names the user
 * chooses like `[mcp.<name>]`.
 *
 * Editing them as JSON is honest about what they are. Local parse errors are
 * reported here because malformed JSON never reaches the server to be judged;
 * anything that parses is still the server's to accept or refuse.
 */
function FreeForm({
  value,
  node,
  readOnly,
  onChange,
}: {
  value: unknown
  node: SchemaNode
  readOnly: boolean
  onChange: (v: unknown) => void
}) {
  const [text, setText] = useState(() => stringify(value, 2))
  const [parseError, setParseError] = useState<string | null>(null)

  return (
    <>
      <textarea
        className="input config-area"
        rows={8}
        spellCheck={false}
        disabled={readOnly}
        value={text}
        onChange={(e) => {
          setText(e.target.value)
          try {
            onChange(JSON.parse(e.target.value))
            setParseError(null)
          } catch (err) {
            setParseError(err instanceof Error ? err.message : String(err))
          }
        }}
      />
      {node.description && <span className="config-help">{node.description}</span>}
      {parseError && (
        <span className="is-failed dot figure" role="alert">
          {parseError}
        </span>
      )}
    </>
  )
}

function stringify(v: unknown, indent = 0): string {
  if (v === undefined) return ''
  if (typeof v === 'string') return v
  try {
    return JSON.stringify(v, null, indent) ?? ''
  } catch {
    return String(v)
  }
}

/** `default_model` → "Default model". The raw key is shown beside it. */
export function humanize(key: string): string {
  const words = key.replace(/[_-]+/g, ' ').trim()
  return words ? words[0].toUpperCase() + words.slice(1) : key
}
