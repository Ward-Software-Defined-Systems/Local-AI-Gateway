export class ApiError extends Error {
  constructor(
    public readonly status: number,
    message: string,
  ) {
    super(message)
    this.name = 'ApiError'
  }
}

export async function apiGet<T>(path: string): Promise<T> {
  const res = await fetch(path, {
    credentials: 'same-origin',
    headers: { accept: 'application/json' },
  })
  if (!res.ok) {
    const message =
      res.status === 401
        ? 'Not authorised: open the dashboard URL printed at startup; it carries the access token.'
        : `HTTP ${res.status} from ${path}`
    throw new ApiError(res.status, message)
  }
  return (await res.json()) as T
}

/** POST or PUT with a JSON body; the error message carries the server's `error.message` when it has one. */
export async function apiSend<T>(method: 'POST' | 'PUT' | 'DELETE', path: string, body?: unknown): Promise<T> {
  const res = await fetch(path, {
    method,
    credentials: 'same-origin',
    headers: { accept: 'application/json', ...(body === undefined ? {} : { 'content-type': 'application/json' }) },
    body: body === undefined ? undefined : JSON.stringify(body),
  })
  if (!res.ok) {
    let message = `HTTP ${res.status} from ${path}`
    try {
      const payload = (await res.json()) as { error?: { message?: string } }
      if (payload.error?.message) message = payload.error.message
    } catch {
      // the body was not JSON; keep the status line
    }
    throw new ApiError(res.status, message)
  }
  return (await res.json()) as T
}
