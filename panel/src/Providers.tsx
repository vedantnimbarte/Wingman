import { useCallback, useEffect, useMemo, useState } from 'react'
import { copyText, useDialog } from './a11y'
import { api, type ProviderInfo, type ProviderList } from './api'
import { message } from './state'
import { Failed, Icon, Loading, Note } from './ui'
import { Picker } from './Picker'

/**
 * Models & providers — connect the services Wingman can call, and pick the
 * one it uses by default.
 *
 * The browser twin of `wingman login`: the server tests a key with a live
 * request, stores it in the OS keyring and records the provider in the global
 * config, exactly as the CLI does, so either can undo the other. Keys only
 * travel one way — into the server. Nothing on this page can show a stored key.
 */

/* ── Catalogue ─────────────────────────────────────────────────────────────
 *
 * Presentation only. The list of providers, and everything true about them,
 * comes from the server; this adds the words and grouping a newcomer needs.
 */

const POPULAR = [
  'anthropic',
  'openai',
  'claude-code',
  'gemini',
  'openrouter',
  'chatgpt',
  'ollama',
  'groq',
  'mistral',
  'deepseek',
  'xai',
  'lmstudio',
]

const ENTERPRISE = new Set(['azure', 'bedrock', 'vertex', 'watsonx', 'databricks', 'snowflake', 'cloudflare', 'nvidia'])

const ABOUT: Record<string, string> = {
  anthropic: 'Claude models, billed to your Anthropic API key.',
  openai: 'GPT models, billed to your OpenAI API key.',
  'claude-code': 'Uses your Claude subscription through the Claude Code app. No key needed.',
  gemini: 'Gemini models with a key from Google AI Studio.',
  openrouter: 'One key for hundreds of models from many labs.',
  chatgpt: 'Uses your ChatGPT subscription. Signs in through a browser.',
  ollama: 'Open models running on this computer with Ollama.',
  groq: 'Open models served very fast.',
  mistral: 'Mistral and Codestral models.',
  deepseek: 'DeepSeek chat and reasoning models.',
  xai: 'Grok models from xAI.',
  lmstudio: 'Models you run in LM Studio on this computer.',
}

/** Where to get a key, for the providers people connect most. */
const KEY_URL: Record<string, string> = {
  anthropic: 'https://console.anthropic.com/settings/keys',
  openai: 'https://platform.openai.com/api-keys',
  gemini: 'https://aistudio.google.com/apikey',
  openrouter: 'https://openrouter.ai/keys',
  groq: 'https://console.groq.com/keys',
  mistral: 'https://console.mistral.ai/api-keys',
  deepseek: 'https://platform.deepseek.com/api_keys',
  xai: 'https://console.x.ai',
}

type Category = 'cloud' | 'local' | 'enterprise' | 'subscription'

const CATEGORIES: { id: Category | 'all'; label: string }[] = [
  { id: 'all', label: 'All' },
  { id: 'cloud', label: 'Cloud' },
  { id: 'local', label: 'On this computer' },
  { id: 'enterprise', label: 'Enterprise' },
  { id: 'subscription', label: 'Subscriptions' },
]

export function category(p: ProviderInfo): Category {
  if (p.kind === 'oauth' || p.kind === 'cli') return 'subscription'
  if (ENTERPRISE.has(p.id)) return 'enterprise'
  if (p.kind === 'local') return 'local'
  return 'cloud'
}

/** "Anthropic (Claude)" → "Anthropic". The parenthetical goes in the description. */
function name(p: ProviderInfo): string {
  return p.label.replace(/\s*\(.*\)\s*$/, '')
}

function about(p: ProviderInfo): string {
  if (ABOUT[p.id]) return ABOUT[p.id]
  switch (category(p)) {
    case 'local':
      return `Models running on your own machine${p.default_base_url ? ` at ${p.default_base_url.replace(/^https?:\/\//, '')}` : ''}.`
    case 'enterprise':
      return `Models through your organisation's ${name(p)} account.`
    case 'subscription':
      return `Uses your ${name(p)} subscription.`
    default:
      return `Models billed to your ${name(p)} API key.`
  }
}

/** A credential's source, said plainly, with the tone it deserves. */
export function credentialLine(p: ProviderInfo): { text: string; tone: '' | 'is-proven' | 'is-asserted' | 'is-failed' } {
  const c = p.credential
  switch (c.kind) {
    case 'keychain':
      return { text: 'Key stored in your system keychain', tone: 'is-proven' }
    case 'env':
      return { text: `Key read from $${c.var}`, tone: 'is-proven' }
    case 'config':
      return { text: 'Key saved as plain text in config.toml', tone: 'is-asserted' }
    case 'keychain-missing':
      return { text: 'The saved key is missing from the keychain', tone: 'is-failed' }
    case 'not-needed':
      return { text: p.kind === 'cli' ? 'Uses the claude command' : 'No key needed', tone: 'is-proven' }
    default:
      return { text: p.kind === 'oauth' ? 'Not signed in' : 'No key yet', tone: 'is-failed' }
  }
}

/** Initials on a tinted tile. The hue is derived from the id, so it is stable. */
function Mono({ p, size = 'md' }: { p: ProviderInfo; size?: 'md' | 'lg' }) {
  const words = name(p).split(/[\s.-]+/).filter(Boolean)
  const text = (words.length > 1 ? words[0][0] + words[1][0] : name(p).slice(0, 2)).toUpperCase()
  let h = 0
  for (const ch of p.id) h = (h * 31 + ch.charCodeAt(0)) % 360
  return (
    <span className={`prov-mono is-${size}`} style={{ '--h': h } as React.CSSProperties} aria-hidden="true">
      {text}
    </span>
  )
}

/* ── Page ──────────────────────────────────────────────────────────────── */

export function Providers() {
  const [data, setData] = useState<ProviderList | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [open, setOpen] = useState<string | null>(null)
  const [query, setQuery] = useState('')
  const [cat, setCat] = useState<Category | 'all'>('all')
  const [notice, setNotice] = useState<string | null>(null)

  const load = useCallback(async () => {
    try {
      setData(await api.providers())
      setError(null)
    } catch (e) {
      setError(message(e))
    }
  }, [])

  useEffect(() => {
    void load()
  }, [load])

  const all = data?.providers ?? []
  const connected = all.filter((p) => p.connected).sort((a, b) => Number(b.is_default) - Number(a.is_default))
  const popular = POPULAR.map((id) => all.find((p) => p.id === id)).filter(
    (p): p is ProviderInfo => Boolean(p && !p.connected),
  )
  const browse = useMemo(() => {
    const q = query.trim().toLowerCase()
    return all.filter(
      (p) =>
        (cat === 'all' || category(p) === cat) &&
        (!q || `${p.label} ${p.id}`.toLowerCase().includes(q)),
    )
  }, [all, cat, query])

  if (error)
    return (
      <Failed
        title="Couldn't load providers"
        detail={error}
        action={{ label: 'Try again', onClick: () => void load() }}
      />
    )
  if (!data) return <Loading what="providers" />

  const current = all.find((p) => p.is_default) ?? null
  const active = open ? (all.find((p) => p.id === open) ?? null) : null

  async function done(text: string) {
    setOpen(null)
    setNotice(text)
    await load()
  }

  return (
    <div className="providers">
      <header className="settings-head">
        <h1>Models &amp; providers</h1>
        <p>
          Connect the AI services Wingman can use. Keys are stored in your system keychain, the same
          place <code>wingman login</code> puts them, and can't be read back from this page.
        </p>
      </header>

      {notice && (
        <Note tone="is-proven">
          <span className="prov-notice">
            {notice}
            <button type="button" className="button button-quiet button-sm" onClick={() => setNotice(null)}>
              Dismiss
            </button>
          </span>
        </Note>
      )}

      <DefaultCard
        // Remounted when the default changes, so its fields start from the
        // new default rather than the one the page loaded with.
        key={`${current?.id ?? ''}/${data.default_model ?? ''}`}
        current={current}
        defaultModel={data.default_model}
        connected={connected}
        onChanged={(t) => void done(t)}
        onConnect={() => document.getElementById('prov-popular')?.scrollIntoView({ behavior: 'smooth' })}
      />

      {connected.length > 0 && (
        <section className="prov-section" aria-labelledby="prov-connected">
          <h2 id="prov-connected">
            Connected <span className="count">{connected.length}</span>
          </h2>
          <div className="prov-grid">
            {connected.map((p) => (
              <ConnectedCard key={p.id} p={p} onManage={() => setOpen(p.id)} />
            ))}
          </div>
        </section>
      )}

      {popular.length > 0 && (
        <section className="prov-section" aria-labelledby="prov-popular" id="prov-popular">
          <h2 id="prov-popular">Popular</h2>
          <div className="prov-grid">
            {popular.map((p) => (
              <article key={p.id} className="prov-card">
                <div className="prov-card-top">
                  <Mono p={p} />
                  {p.kind !== 'api-key' && <span className="prov-tag">{TAG[category(p)]}</span>}
                </div>
                <h3>{name(p)}</h3>
                <p className="prov-about">{about(p)}</p>
                <button type="button" className="button prov-cta" onClick={() => setOpen(p.id)}>
                  {p.kind === 'oauth' ? 'How to sign in' : 'Connect'}
                </button>
              </article>
            ))}
          </div>
        </section>
      )}

      <section className="prov-section" aria-labelledby="prov-all">
        <div className="prov-section-head">
          <h2 id="prov-all">
            All providers <span className="count">{all.length}</span>
          </h2>
          <label className="filter-search prov-search">
            <Icon name="search" size={14} />
            <input
              className="input"
              type="search"
              placeholder="Search providers"
              aria-label="Search providers"
              value={query}
              onChange={(e) => setQuery(e.target.value)}
            />
          </label>
        </div>
        <div className="filters" role="toolbar" aria-label="Filter providers">
          {CATEGORIES.map((c) => (
            <button
              key={c.id}
              type="button"
              className="chip"
              aria-pressed={cat === c.id}
              onClick={() => setCat(c.id)}
            >
              {c.label}
              <span className="chip-count">
                {c.id === 'all' ? all.length : all.filter((p) => category(p) === c.id).length}
              </span>
            </button>
          ))}
        </div>
        {browse.length === 0 ? (
          <p className="panel-empty">No provider matches “{query.trim()}”.</p>
        ) : (
          <ul className="prov-list">
            {browse.map((p) => (
              <li key={p.id}>
                <button type="button" onClick={() => setOpen(p.id)}>
                  <Mono p={p} />
                  <span className="prov-list-text">
                    <span className="prov-list-name">{p.label}</span>
                    <span className="prov-list-about">{about(p)}</span>
                  </span>
                  {p.connected ? (
                    <span className={`pill ${p.ready ? 'is-proven' : 'is-failed'}`}>
                      {p.is_default ? 'Default' : p.ready ? 'Connected' : 'Needs a key'}
                    </span>
                  ) : (
                    <span className="prov-list-action">{p.kind === 'oauth' ? 'Sign in' : 'Connect'}</span>
                  )}
                </button>
              </li>
            ))}
          </ul>
        )}
      </section>

      {active && (
        <ConnectSheet
          key={active.id}
          p={active}
          hasDefault={Boolean(current)}
          onClose={() => setOpen(null)}
          onDone={(t) => void done(t)}
        />
      )}
    </div>
  )
}

const TAG: Record<Category, string> = {
  cloud: 'Cloud',
  local: 'On this computer',
  enterprise: 'Enterprise',
  subscription: 'Subscription',
}

/* ── The default ───────────────────────────────────────────────────────── */

/**
 * The one decision on this page that changes every chat and run: which
 * provider and model Wingman reaches for when nothing names one.
 */
function DefaultCard({
  current,
  defaultModel,
  connected,
  onChanged,
  onConnect,
}: {
  current: ProviderInfo | null
  defaultModel: string | null
  connected: ProviderInfo[]
  onChanged: (text: string) => void
  onConnect: () => void
}) {
  const shownModel = (defaultModel ?? '').replace(/^[^/]+\//, '') || current?.model || current?.default_model || ''
  const [provider, setProvider] = useState(current?.id ?? '')
  const [model, setModel] = useState(shownModel)
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const choices = connected.filter((p) => p.ready)
  const picked = choices.find((p) => p.id === provider)
  const dirty = provider !== (current?.id ?? '') || model.trim() !== shownModel

  async function save() {
    if (!picked) return
    setBusy(true)
    setError(null)
    try {
      const r = await api.makeDefaultProvider(picked.id, model.trim() || undefined)
      onChanged(`Wingman now uses ${r.model} from ${name(picked)} by default.`)
    } catch (e) {
      setError(message(e))
    } finally {
      setBusy(false)
    }
  }

  return (
    <section className="prov-default" aria-label="Default model">
      <div className="prov-default-main">
        {current ? <Mono p={current} size="lg" /> : <span className="prov-mono is-lg is-empty" aria-hidden="true">?</span>}
        <div>
          <span className="prov-default-label">Default model</span>
          <p className="prov-default-value">
            {current ? (
              <>
                {shownModel || current.default_model}
                <span> from {name(current)}</span>
              </>
            ) : (
              'No default yet'
            )}
          </p>
          <p className="prov-default-help">
            {current
              ? 'Used for chats, runs and the terminal whenever a command does not name a model.'
              : 'Connect a provider below and Wingman will use it for every chat and run.'}
          </p>
        </div>
      </div>

      {choices.length > 0 ? (
        <div className="prov-default-form">
          <label>
            <span className="field-label">Provider</span>
            <Picker
              label="Default provider"
              value={provider}
              placeholder="Choose a provider"
              onChange={(v) => {
                setProvider(v)
                const p = choices.find((c) => c.id === v)
                setModel(p?.model ?? p?.default_model ?? '')
              }}
              options={choices.map((p) => ({ value: p.id, label: name(p), description: p.model ?? p.default_model }))}
            />
          </label>
          <label>
            <span className="field-label">Model</span>
            <input
              className="input mono"
              value={model}
              placeholder={picked?.default_model}
              spellCheck={false}
              onChange={(e) => setModel(e.target.value)}
            />
          </label>
          <button type="button" className="button button-primary" disabled={!dirty || !picked || busy} onClick={() => void save()}>
            {busy ? 'Saving…' : 'Save default'}
          </button>
          {error && (
            <p className="is-failed dot figure prov-default-error" role="alert">
              {error}
            </p>
          )}
        </div>
      ) : (
        <button type="button" className="button button-primary" onClick={onConnect}>
          <Icon name="plus" size={15} />
          Connect a provider
        </button>
      )}
    </section>
  )
}

/* ── A connected provider ──────────────────────────────────────────────── */

function ConnectedCard({ p, onManage }: { p: ProviderInfo; onManage: () => void }) {
  const line = credentialLine(p)
  return (
    <article className={`prov-card is-connected${p.is_default ? ' is-default' : ''}`}>
      <div className="prov-card-top">
        <Mono p={p} />
        {p.is_default && <span className="prov-tag is-default">Default</span>}
      </div>
      <h3>{name(p)}</h3>
      <p className="prov-model mono">{p.model ?? p.default_model}</p>
      <p className={`prov-cred ${line.tone}`}>
        <span className={`lamp ${line.tone}`} aria-hidden="true" />
        {line.text}
      </p>
      {p.base_url && p.kind !== 'api-key' && <p className="prov-url mono">{p.base_url}</p>}
      <button type="button" className="button prov-cta" onClick={onManage}>
        {p.ready ? 'Manage' : p.kind === 'oauth' ? 'How to sign in' : 'Add key'}
      </button>
    </article>
  )
}

/* ── Connect / manage ──────────────────────────────────────────────────── */

type Outcome = { tone: 'is-proven' | 'is-failed' | 'is-asserted'; text: string } | null

function ConnectSheet({
  p,
  hasDefault,
  onClose,
  onDone,
}: {
  p: ProviderInfo
  hasDefault: boolean
  onClose: () => void
  onDone: (text: string) => void
}) {
  const ref = useDialog<HTMLDivElement>(onClose)
  const [key, setKey] = useState('')
  const [reveal, setReveal] = useState(false)
  const [model, setModel] = useState(p.model ?? '')
  const [baseUrl, setBaseUrl] = useState(p.base_url ?? '')
  const [custom, setCustom] = useState(Boolean(p.base_url) && p.kind === 'api-key')
  const [makeDefault, setMakeDefault] = useState(!hasDefault && !p.is_default)
  const [busy, setBusy] = useState<'test' | 'save' | 'remove' | null>(null)
  const [outcome, setOutcome] = useState<Outcome>(null)
  const [failedTest, setFailedTest] = useState(false)

  // A result describes the inputs it was made with; once they change it is stale.
  function changed<T>(set: (v: T) => void) {
    return (v: T) => {
      set(v)
      setOutcome(null)
      setFailedTest(false)
    }
  }
  const onKey = changed(setKey)
  const onModel = changed(setModel)
  const onUrl = changed(setBaseUrl)

  const hasKey = p.credential.kind === 'keychain' || p.credential.kind === 'env' || p.credential.kind === 'config'
  const needsKey = p.kind === 'api-key'
  const showUrl = p.kind === 'local' || Boolean(p.default_base_url) || custom
  const line = credentialLine(p)
  const cli = [
    `wingman login ${p.id}`,
    model.trim() && `--model ${model.trim()}`,
    showUrl && baseUrl.trim() && baseUrl.trim() !== p.default_base_url && `--base-url ${baseUrl.trim()}`,
    !makeDefault && !p.is_default && '--no-default',
  ]
    .filter(Boolean)
    .join(' ')

  function body(skip = false) {
    return {
      api_key: key.trim() || undefined,
      model: model.trim() || undefined,
      base_url: showUrl ? baseUrl.trim() || undefined : undefined,
      make_default: makeDefault,
      skip_test: skip,
    }
  }

  async function test() {
    setBusy('test')
    setOutcome(null)
    try {
      const r = await api.testProvider(p.id, body())
      setFailedTest(false)
      setOutcome({ tone: 'is-proven', text: `It works — ${r.model} answered.` })
    } catch (e) {
      setFailedTest(true)
      setOutcome({ tone: 'is-failed', text: message(e) })
    } finally {
      setBusy(null)
    }
  }

  async function save(skip = false) {
    setBusy('save')
    setOutcome(null)
    try {
      const r = await api.connectProvider(p.id, body(skip))
      const where =
        r.stored_in === 'keychain'
          ? ' Its key is in your system keychain.'
          : r.stored_in === 'config'
            ? ' No keychain was available, so the key was saved in config.toml.'
            : ''
      onDone(`${p.connected ? 'Saved' : 'Connected'} ${name(p)}${r.is_default ? ' and made it your default' : ''}.${where}`)
    } catch (e) {
      setFailedTest(/test failed/i.test(message(e)))
      setOutcome({ tone: 'is-failed', text: message(e) })
      setBusy(null)
    }
  }

  async function remove() {
    if (!window.confirm(`Disconnect ${name(p)}? Its key is removed from the keychain and its settings from config.toml.`))
      return
    setBusy('remove')
    setOutcome(null)
    try {
      const r = await api.disconnectProvider(p.id)
      onDone(
        r.keyring_error
          ? `Disconnected ${name(p)}, but the keychain entry could not be removed: ${r.keyring_error}`
          : `Disconnected ${name(p)}.`,
      )
    } catch (e) {
      setOutcome({ tone: 'is-failed', text: message(e) })
      setBusy(null)
    }
  }

  return (
    <div className="palette-backdrop prov-backdrop" role="presentation" onMouseDown={(e) => e.target === e.currentTarget && onClose()}>
      <div className="prov-sheet" ref={ref} role="dialog" aria-modal="true" aria-labelledby="prov-sheet-title" tabIndex={-1}>
        <header className="prov-sheet-head">
          <Mono p={p} size="lg" />
          <div>
            <h2 id="prov-sheet-title">{p.label}</h2>
            <p>{about(p)}</p>
          </div>
          <button type="button" className="button button-quiet button-icon" aria-label="Close" onClick={onClose}>
            <Icon name="close" />
          </button>
        </header>

        {p.connected && (
          <p className={`prov-cred ${line.tone} prov-sheet-status`}>
            <span className={`lamp ${line.tone}`} aria-hidden="true" />
            {line.text}
            {p.is_default && <span className="prov-tag is-default">Default</span>}
          </p>
        )}

        {p.kind === 'oauth' ? (
          <div className="prov-sheet-body">
            <p>
              {name(p)} signs in through your browser, on the computer where Wingman is running. Open a
              terminal there and run:
            </p>
            <Command text={`wingman login ${p.id}`} />
            <p className="faint">Once you're signed in, refresh this page and it shows as connected.</p>
          </div>
        ) : (
          <form
            className="prov-sheet-body"
            onSubmit={(e) => {
              e.preventDefault()
              void save()
            }}
          >
            {p.kind === 'cli' && (
              <Note>
                Needs the <code>claude</code> command installed and signed in on the computer running
                Wingman. Testing checks that it is there.
              </Note>
            )}

            {needsKey && (
              <label className="prov-field">
                <span className="field-label">API key</span>
                <span className="prov-key">
                  <input
                    className="input mono"
                    type={reveal ? 'text' : 'password'}
                    autoComplete="off"
                    spellCheck={false}
                    autoFocus
                    value={key}
                    placeholder={hasKey ? 'Leave empty to keep the current key' : p.env_var ? `Paste your key (or set $${p.env_var})` : 'Paste your key'}
                    onChange={(e) => onKey(e.target.value)}
                  />
                  <button
                    type="button"
                    className="button button-quiet button-sm"
                    aria-pressed={reveal}
                    onClick={() => setReveal((r) => !r)}
                  >
                    {reveal ? 'Hide' : 'Show'}
                  </button>
                </span>
                <span className="prov-hint">
                  Stored in your system keychain. It is never shown again.
                  {KEY_URL[p.id] && (
                    <>
                      {' '}
                      <a href={KEY_URL[p.id]} target="_blank" rel="noreferrer">
                        Get a key
                      </a>
                    </>
                  )}
                </span>
              </label>
            )}

            <label className="prov-field">
              <span className="field-label">Model</span>
              <input
                className="input mono"
                value={model}
                spellCheck={false}
                placeholder={p.default_model}
                onChange={(e) => onModel(e.target.value)}
              />
              <span className="prov-hint">Leave empty to use {p.default_model}.</span>
            </label>

            {showUrl ? (
              <label className="prov-field">
                <span className="field-label">Server address</span>
                <input
                  className="input mono"
                  value={baseUrl}
                  spellCheck={false}
                  placeholder={p.default_base_url ?? 'https://…'}
                  onChange={(e) => onUrl(e.target.value)}
                />
                <span className="prov-hint">
                  {p.kind === 'local'
                    ? 'Where the model server is listening on your machine or network.'
                    : p.default_base_url?.includes('YOUR-')
                      ? 'Replace the parts in capitals with your own resource and deployment.'
                      : 'Leave empty to use the provider’s standard address.'}
                </span>
              </label>
            ) : (
              needsKey && (
                <button type="button" className="button button-quiet button-sm prov-more" onClick={() => setCustom(true)}>
                  Use a custom server address
                </button>
              )
            )}

            {!p.is_default && (
              <label className="check prov-check">
                <input type="checkbox" checked={makeDefault} onChange={(e) => setMakeDefault(e.target.checked)} />
                Make this my default provider
              </label>
            )}

            {outcome && (
              <Note tone={outcome.tone} role={outcome.tone === 'is-failed' ? 'alert' : 'status'}>
                {outcome.text}
              </Note>
            )}

            <div className="prov-sheet-actions">
              <button type="button" className="button" disabled={busy !== null || (needsKey && !hasKey && !key.trim())} onClick={() => void test()}>
                {busy === 'test' ? 'Testing…' : 'Test connection'}
              </button>
              <button type="submit" className="button button-primary" disabled={busy !== null || (needsKey && !hasKey && !key.trim())}>
                {busy === 'save' ? 'Testing and saving…' : p.connected ? 'Save changes' : 'Connect'}
              </button>
            </div>
            {failedTest && (
              <button type="button" className="button button-quiet button-sm prov-skip" disabled={busy !== null} onClick={() => void save(true)}>
                Save anyway, without testing
              </button>
            )}

            <details className="prov-cli">
              <summary>Prefer the terminal?</summary>
              <p>The same thing from a shell. You'll be asked for the key, so it never lands in your history:</p>
              <Command text={cli} />
              {p.env_var && (
                <p>
                  Or set <code>{p.env_var}</code> in your environment. Wingman picks it up without
                  saving anything.
                </p>
              )}
            </details>
          </form>
        )}

        {p.connected && (
          <footer className="prov-sheet-foot">
            <button type="button" className="button button-quiet prov-remove" disabled={busy !== null} onClick={() => void remove()}>
              {busy === 'remove' ? 'Disconnecting…' : `Disconnect ${name(p)}`}
            </button>
          </footer>
        )}
      </div>
    </div>
  )
}

function Command({ text }: { text: string }) {
  const [state, setState] = useState<'idle' | 'done' | 'failed'>('idle')
  return (
    <div className="prov-command">
      <code>{text}</code>
      <button
        type="button"
        className="button button-quiet button-sm"
        onClick={() =>
          void copyText(text).then((ok) => {
            setState(ok ? 'done' : 'failed')
            window.setTimeout(() => setState('idle'), 1600)
          })
        }
      >
        <Icon name={state === 'done' ? 'check' : 'copy'} size={14} />
        {state === 'done' ? 'Copied' : state === 'failed' ? 'Blocked' : 'Copy'}
      </button>
    </div>
  )
}
