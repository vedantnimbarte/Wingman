/**
 * The one place that talks to `/v1`.
 *
 * Authentication is a cookie the browser holds and page script cannot read
 * (`HttpOnly`), so nothing here handles a token except `signIn`. Every other
 * call just needs the cookie to ride along, which `credentials: 'same-origin'`
 * arranges.
 */

export class ApiError extends Error {
  constructor(
    readonly status: number,
    message: string,
  ) {
    super(message)
    this.name = 'ApiError'
  }

  /** The credential is missing or no longer accepted — sign in again. */
  get isUnauthorized() {
    return this.status === 401
  }
}

/** `GET /v1/health` — the only route that never needs a credential. */
export type Health = {
  ok: boolean
  version: string
  uptime_secs: number
  /** False on a loopback server with no token: skip the sign-in screen. */
  auth_required: boolean
}

/** One entry of `GET /v1/projects`. Field names mirror `serve::projects::describe`. */
export type Project = {
  id: string
  root: string
  branch: string | null
  indexd_running: boolean
  /** Age of the semantic index, or null when the repo has never been indexed. */
  index_age_secs: number | null
}

async function request<T>(path: string, init?: RequestInit): Promise<T> {
  let res: Response
  try {
    res = await fetch(path, { credentials: 'same-origin', ...init })
  } catch {
    // A network-level failure means the daemon is not answering at all, which
    // is a different problem from a route that said no — name it as such
    // rather than letting a generic "Failed to fetch" reach the screen.
    throw new ApiError(0, 'No answer from the daemon. Is `wingman serve` running?')
  }

  if (!res.ok) {
    throw new ApiError(res.status, await errorText(res))
  }
  if (res.status === 204) return undefined as T
  return (await res.json()) as T
}

/** Prefer the server's own `{"error": ...}` message over a status code. */
async function errorText(res: Response): Promise<string> {
  try {
    const body = (await res.json()) as { error?: string }
    if (body?.error) return body.error
  } catch {
    // Not JSON. The status line is all we have.
  }
  return `${res.status} ${res.statusText}`
}

export const api = {
  health: () => request<Health>('/v1/health'),
  projects: () => request<{ projects: Project[] }>('/v1/projects').then((r) => r.projects),

  /**
   * Exchange a token for the panel cookie. The token is sent once and never
   * stored by the page — the server puts it in an `HttpOnly` cookie, so it
   * is not readable by script afterwards, including by this module.
   */
  signIn: (token: string) =>
    request<{ auth_required: boolean }>('/v1/ui/session', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ token }),
    }),

  signOut: () => request<void>('/v1/ui/session', { method: 'DELETE' }),
}
