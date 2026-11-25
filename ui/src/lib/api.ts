// The dev API, which the app reads everything through (the same one tests use), and its types.

export class ApiError extends Error {
  status: number

  constructor(message: string, status: number) {
    super(message)
    this.status = status
  }
}

/** Fetch JSON from the dev API. Errors carry the API's `error_description`. */
export async function api<T>(path: string, init?: RequestInit): Promise<T> {
  const headers = new Headers(init?.headers)
  headers.set("accept", "application/json")
  const res = await fetch(path, { ...init, headers })
  if (res.status === 204) return null as T
  const body = await res.json().catch(() => null)
  if (!res.ok) {
    throw new ApiError(
      body?.error_description ?? `HTTP ${res.status}`,
      res.status
    )
  }
  return body as T
}

export type Provider = { id: string; name: string; color: string }

export type Info = {
  name: string
  version: string
  base_url: string
  smtp: string
  scenario_domain: string
  attempts_capacity: number
  providers: Provider[]
}

export type Address = { name?: string; email: string }

export type Template = {
  field: string
  id: string
  data: unknown
  rendered: boolean
}

export type Summary = {
  id: number
  received_at: number
  provider: string
  endpoint: string
  provider_message_id: string
  from: Address | null
  to: Address[]
  cc: Address[]
  bcc: Address[]
  subject: string
  preview: string
  size: number
  has_attachments: boolean
  read: boolean
  scenario?: string
  template?: Template
  tags: string[]
}

export type Attachment = {
  index: number
  filename: string
  content_type: string
  size: number
  content_id?: string
  inline: boolean
}

export type Message = Summary & {
  reply_to: Address[]
  message_id: string | null
  headers: { name: string; value: string }[]
  text: string | null
  html: string | null
  attachments: Attachment[]
  details: Record<string, unknown>
  request?: unknown
  envelope?: {
    mail_from: string
    rcpt_to: string[]
    helo?: string
    auth_user?: string
  }
}

export type MessageList = {
  messages: Summary[]
  total: number
  unread: number
  cursor: number
}

export type Mailboxes = {
  mailboxes: { address: string; total: number; unread: number }[]
  /** Addresses matching the filter; the list may be cut short. */
  total: number
  /** Across the whole inbox. */
  messages: number
  unread: number
}

export type Extracted = {
  otp?: string | null
  links?: { href: string; text: string }[]
}

export type Attempt = {
  id: number
  at: number
  duration_ms: number
  provider: string
  endpoint: string
  status: number
  outcome: "accepted" | "partial" | "failed"
  from?: string
  recipients: string[]
  subject?: string
  messages: number[]
  scenario?: string
  error?: string
  error_description?: string
  warnings?: string[]
  request?: string
  response?: string
}

export type Attempts = { attempts: Attempt[]; latest_id: number }

export type Snippet = { title: string; code: string }

export type Setup = {
  base_url: string
  smtp: string
  scenario_domain: string
  endpoints: { label: string; url: string }[]
  snippets: Snippet[]
  test: Snippet
}

export type Scenarios = {
  domain: string
  scenarios: { address: string; effect: string }[]
}
