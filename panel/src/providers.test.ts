import { describe, expect, it } from 'vitest'
import type { ProviderInfo } from './api'
import { category, credentialLine } from './Providers'

const p = (over: Partial<ProviderInfo>): ProviderInfo => ({
  id: 'x',
  label: 'X',
  kind: 'api-key',
  env_var: null,
  default_model: 'm',
  default_base_url: null,
  connected: true,
  ready: true,
  credential: { kind: 'keychain' },
  model: null,
  base_url: null,
  is_default: false,
  ...over,
})

describe('category', () => {
  it('files subscriptions, enterprise clouds and local servers apart from plain APIs', () => {
    expect(category(p({ id: 'chatgpt', kind: 'oauth' }))).toBe('subscription')
    expect(category(p({ id: 'claude-code', kind: 'cli' }))).toBe('subscription')
    expect(category(p({ id: 'bedrock' }))).toBe('enterprise')
    expect(category(p({ id: 'ollama', kind: 'local' }))).toBe('local')
    expect(category(p({ id: 'groq' }))).toBe('cloud')
  })
})

describe('credentialLine', () => {
  it('says where a key comes from, and warns about plain text and missing entries', () => {
    expect(credentialLine(p({ credential: { kind: 'env', var: 'OPENAI_API_KEY' } }))).toEqual({
      text: 'Key read from $OPENAI_API_KEY',
      tone: 'is-proven',
    })
    expect(credentialLine(p({ credential: { kind: 'config' } })).tone).toBe('is-asserted')
    expect(credentialLine(p({ credential: { kind: 'keychain-missing' } })).tone).toBe('is-failed')
    expect(credentialLine(p({ kind: 'oauth', credential: { kind: 'none' } })).text).toBe('Not signed in')
  })
})
