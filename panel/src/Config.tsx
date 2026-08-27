import { useCallback, useEffect, useMemo, useState } from 'react'
import { useUnsavedWarning } from './a11y'
import { api, type ConfigSchema, type SchemaNode } from './api'
import { at, fieldsOf, resolve, toPatch, type Field } from './schema'
import { message } from './state'
import { Failed, Loading, Note, PageHead } from './ui'

const REDACTED = '<redacted>'

/** Synthetic section holding the bare top-level keys. Never a real config table. */
const GENERAL = 'general'

/**
 * Settings.
 *
 * Every form on this screen is generated from `GET /v1/config/schema`, which
 * is derived from the `wingman-config` structs themselves. A field added to a
 * Rust struct shows up here with its `///` comment as help text and nobody
 * edits this file — which is the whole reason the schema route exists rather
 * than 28 hand-written forms that drift.
 *
 * The panel does **no validation of its own**. `PATCH /v1/config` round-trips
 * the result through the real config parser and returns its error, so there is
 * one validator and it is the one that actually has to load the file.
 */
export function Config() {
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

  // Twenty-one sections and a floating save bar meant twelve pending edits
  // could leave with the tab and take no notice. The panel routes in-page, so
  // this covers the half the browser gives a hook for and nothing more.
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
  // (`default_model`, `default_provider`). Listing those as sections of their
  // own would give each a page with one control, and — because they have no
  // `properties` — drop them into the JSON escape hatch. They are collected
  // into a synthetic first section instead.
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

  const all = general.length ? [GENERAL, ...sections] : sections
  const active = section ?? all[0] ?? null

  if (error)
    return (
      <div className="view">
        <Failed
          title="Could not load settings"
          detail={error}
          action={{ label: 'Try again', onClick: () => void load() }}
        />
      </div>
    )
  if (!meta || !current || !active) return <Loading what="settings" />

  const defs = meta.schema.definitions ?? {}
  const props = meta.schema.properties ?? {}
  const isGeneral = active === GENERAL
  const readOnly = meta.readonly_sections.includes(active)

  // The synthetic section is assembled from the bare top-level keys; a real
  // one is read straight off the schema.
  const resolved = isGeneral ? {} : resolve(props[active], defs)
  const fields = isGeneral
    ? fieldsOf({ type: 'object', properties: Object.fromEntries(general.map((k) => [k, props[k]])) }, defs)
    : fieldsOf(props[active], defs)

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

  /**
   * A field's path in the config document.
   *
   * The synthetic `general` section is not a table, so its fields are already
   * at the top level and must not be prefixed — prefixing would patch a
   * `[general]` table that no config has.
   */
  function keyOf(f: Field): string[] {
    return isGeneral ? f.path : [active, ...f.path]
  }

  function edit(path: string[], value: unknown) {
    setSaved(false)
    setEdits((prev) => {
      const next = new Map(prev)
      next.set((isGeneral ? path : [active, ...path]).join('.'), value)
      return next
    })
  }

  return (
    <div className="view">
      <PageHead
        eyebrow="Config"
        title="Settings"
        intro={
          <>
            Generated from the schema the daemon derives from its own config types, so every field
            here carries the documentation the source does. Saving writes to{' '}
            <code className="figure">{meta.writes_to}</code> — the global file, never a repo's{' '}
            <code>.wingman/config.toml</code>.
          </>
        }
      />

      <div className="config-body">
        <nav className="config-nav" aria-label="Config sections">
          {/* Twenty-one sections in a list with no way to narrow it. ⌘K
              deliberately does not reach config fields — it never acts on
              something you cannot see — so the filter belongs here, next to
              what it filters. */}
          <label className="filter-search config-find">
            <input
              className="input"
              type="search"
              placeholder="Find a section"
              aria-label="Find a config section"
              value={query}
              onChange={(e) => setQuery(e.target.value)}
            />
          </label>
          {all
            .filter((s) => s.toLowerCase().includes(query.trim().toLowerCase()))
            .map((s) => (
              <button
                key={s}
                type="button"
                className="nav-item"
                aria-current={s === active ? 'page' : undefined}
                onClick={() => setSection(s)}
              >
                {s}
                {meta.readonly_sections.includes(s) && (
                  <span className="muted"> · read-only</span>
                )}
              </button>
            ))}
        </nav>

        <section className="config-fields">
          <h2 className="section-head">{active}</h2>
          {resolved.description && <p className="section-intro">{resolved.description}</p>}

          {readOnly && (
            <Note tone="is-asserted">
              <code>[{active}]</code> cannot be changed through the API it configures — a server
              that can rewrite its own token, ceiling or project allowlist has no ceiling. Edit the
              file directly.
            </Note>
          )}

          {fields.length === 0 ? (
            <FreeForm
              // Keyed on the section. Without it React reuses the instance
              // between two form-less sections, and `useState` keeps the text
              // it was seeded with — so switching from `[hooks]` to `[mcp]`
              // left you editing one section's JSON under the other's
              // heading, and saving wrote it there.
              key={active}
              value={at(current, [active])}
              node={resolved}
              readOnly={readOnly}
              onChange={(v) => edit([], v)}
            />
          ) : (
            <div className="rows">
              {fields.map((f) => (
                <FieldRow
                  key={f.path.join('.')}
                  field={f}
                  value={
                    edits.has(keyOf(f).join('.'))
                      ? edits.get(keyOf(f).join('.'))
                      : at(current, keyOf(f))
                  }
                  fallback={at(meta.defaults, keyOf(f))}
                  dirty={edits.has(keyOf(f).join('.'))}
                  readOnly={readOnly}
                  onChange={(v) => edit(f.path, v)}
                />
              ))}
            </div>
          )}
        </section>
      </div>

      <div className="config-save">
        <span className="config-save-spacer">
          {saved && <span className="is-proven dot figure">Saved</span>}
          {saveError && (
            <span className="is-failed dot figure" role="alert">
              {saveError}
            </span>
          )}
          {!saved && !saveError && edits.size > 0 && (
            <span className="faint figure">
              {edits.size} field{edits.size === 1 ? '' : 's'} edited
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
            {preview ? 'Hide' : 'Preview'}
          </button>
        )}
        {edits.size > 0 && (
          <button type="button" className="button button-quiet" onClick={() => setEdits(new Map())}>
            Discard
          </button>
        )}
        <button
          type="button"
          className="button button-primary"
          disabled={saving || edits.size === 0}
          onClick={() => void save()}
        >
          {saving
            ? 'Saving…'
            : edits.size === 0
              ? 'No changes'
              : `Save ${edits.size} change${edits.size === 1 ? '' : 's'}`}
        </button>
      </div>

      {/* The patch itself, before it is sent. `PATCH` deep-merges and the file
          is edited as a TOML document, so a save is a one-line diff — but
          which line is not obvious from a form with a hundred inputs, and the
          only way to check used to be to save and read the file. */}
      {preview && edits.size > 0 && (
        <>
          <h3 className="section-head">This save sends</h3>
          <p className="section-intro">
            Merged into <code className="figure">{meta.writes_to}</code>. Keys not listed here are
            not touched.
          </p>
          <pre className="report figure">{JSON.stringify(toPatch(edits), null, 2)}</pre>
        </>
      )}
    </div>
  )
}

/* ── One field ─────────────────────────────────────────────────────────── */

function FieldRow({
  field,
  value,
  fallback,
  dirty,
  readOnly,
  onChange,
}: {
  field: Field
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
        <span className={dirty ? 'config-dirty' : undefined}>{field.path.join('.')}</span>
        {field.description && <span className="config-help">{field.description}</span>}
        {fallback !== undefined && !isRedacted && (
          <span className="config-default figure muted">default {stringify(fallback)}</span>
        )}
      </label>

      <span className="config-input">
        {isRedacted ? (
          <Redacted id={id} readOnly={readOnly} onChange={onChange} />
        ) : (
          <Input id={id} field={field} value={value} readOnly={readOnly} onChange={onChange} />
        )}
      </span>
    </div>
  )
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

    case 'enum':
      return (
        <select
          id={id}
          className="select"
          disabled={readOnly}
          value={typeof value === 'string' ? value : ''}
          onChange={(e) => onChange(e.target.value)}
        >
          {field.nullable && <option value="">— unset —</option>}
          {field.choices?.map((c) => (
            <option key={c.value} value={c.value} title={c.description}>
              {c.value}
            </option>
          ))}
        </select>
      )

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
