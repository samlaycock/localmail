// The inbox's state lives in its URL: `/?to=<mailbox>&q=<search>&provider=<id>&mbox=<filter>`,
// with `/messages/<id>` for an open message. A mailbox link works before any mail arrives.

export type Filters = {
  /** The mailbox shown: a recipient address, or null for all mail. */
  to: string | null
  q: string
  provider: string | null
  /** Narrows the list of mailboxes. */
  mbox: string
}

export function readFilters(params: URLSearchParams): Filters {
  return {
    to: params.get("to")?.trim().toLowerCase() || null,
    q: params.get("q")?.trim() ?? "",
    provider: params.get("provider") || null,
    mbox: params.get("mbox")?.trim() ?? "",
  }
}

/**
 * The dev API filters for the messages listed. The bulk actions use the same filters, so they
 * act on exactly the messages listed.
 */
export function listFilters(filters: Filters) {
  const params = new URLSearchParams()
  if (filters.to) params.set("to", filters.to)
  if (filters.q) params.set("q", filters.q)
  if (filters.provider) params.set("provider", filters.provider)
  return params
}

/** The inbox at a mailbox (or all mail), keeping the search and provider filters. */
export function mailboxLink(params: URLSearchParams, to: string | null) {
  const next = new URLSearchParams(params)
  if (to) next.set("to", to)
  else next.delete("to")
  const search = next.toString()
  return { pathname: "/", search: search ? `?${search}` : "" }
}

/** A mailbox's own link, to share or print from a failing test. */
export const mailboxUrl = (to: string) =>
  `${location.origin}/?to=${encodeURIComponent(to)}`
