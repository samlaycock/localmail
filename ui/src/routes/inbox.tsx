import {
  InboxIcon,
  MailIcon,
  PaperclipIcon,
  SearchIcon,
  TriangleAlertIcon,
} from "lucide-react"
import * as React from "react"
import {
  Link,
  Outlet,
  redirect,
  useFetcher,
  useLoaderData,
  useLocation,
  useNavigate,
  useNavigationType,
  useParams,
  useRevalidator,
  useSearchParams,
  type ActionFunctionArgs,
  type LoaderFunctionArgs,
} from "react-router"

import { CopyButton } from "@/components/copy-button"
import { ProviderBadge } from "@/components/provider-badge"
import {
  AlertDialog,
  AlertDialogAction,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
  AlertDialogTrigger,
} from "@/components/ui/alert-dialog"
import { Badge } from "@/components/ui/badge"
import { Button, buttonVariants } from "@/components/ui/button"
import {
  Empty,
  EmptyDescription,
  EmptyHeader,
  EmptyMedia,
  EmptyTitle,
} from "@/components/ui/empty"
import {
  InputGroup,
  InputGroupAddon,
  InputGroupInput,
} from "@/components/ui/input-group"
import { Item, ItemContent, ItemTitle } from "@/components/ui/item"
import { Kbd } from "@/components/ui/kbd"
import {
  ResizableHandle,
  ResizablePanel,
  ResizablePanelGroup,
} from "@/components/ui/resizable"
import { ScrollArea } from "@/components/ui/scroll-area"
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select"
import { api, type Mailboxes, type MessageList, type Summary } from "@/lib/api"
import { formText } from "@/lib/form"
import { fullTime, plural, when } from "@/lib/format"
import {
  listFilters,
  mailboxLink,
  mailboxUrl,
  readFilters,
  type Filters,
} from "@/lib/inbox"
import { useInfo } from "@/lib/info"
import { cn } from "@/lib/utils"

export async function inboxLoader({ request }: LoaderFunctionArgs) {
  const filters = readFilters(new URL(request.url).searchParams)
  const list = listFilters(filters)
  list.set("limit", "300")
  const boxes = new URLSearchParams({ limit: "200" })
  if (filters.mbox) boxes.set("q", filters.mbox)
  const [messages, mailboxes] = await Promise.all([
    api<MessageList>(`/dev/api/messages?${list}`, { signal: request.signal }),
    api<Mailboxes>(`/dev/api/mailboxes?${boxes}`, { signal: request.signal }),
  ])
  return { messages, mailboxes }
}

/** The bulk actions, on the messages listed (`filters`, from `listFilters`). */
export async function inboxAction({ request }: ActionFunctionArgs) {
  const form = await request.formData()
  const filters = formText(form, "filters")
  switch (formText(form, "intent")) {
    case "read-all":
      await api(`/dev/api/messages/read?${filters}`, { method: "POST" })
      return null
    case "delete-all":
      await api(`/dev/api/messages?${filters}`, { method: "DELETE" })
      // Close the open message, keeping the mailbox and filters.
      return redirect(`/${formText(form, "search")}`)
  }
  throw new Response("Unknown intent", { status: 400 })
}

export function Inbox() {
  const { messages, mailboxes } = useLoaderData<typeof inboxLoader>()
  const [searchParams] = useSearchParams()
  const filters = readFilters(searchParams)
  const { search } = useLocation()
  const navigate = useNavigate()
  const params = useParams()
  const selected = params.id ? Number(params.id) : null

  useLiveUpdates(messages.cursor)

  // j and k move through the list.
  React.useEffect(() => {
    function onKeyDown(e: KeyboardEvent) {
      if (
        e.metaKey ||
        e.ctrlKey ||
        e.altKey ||
        (e.key !== "j" && e.key !== "k")
      )
        return
      const target = e.target as HTMLElement
      if (target.closest("input, textarea, select, [contenteditable]")) return
      const list = messages.messages
      const i = list.findIndex((m) => m.id === selected)
      const next = list[i === -1 ? 0 : i + (e.key === "j" ? 1 : -1)]
      if (next) void navigate({ pathname: `/messages/${next.id}`, search })
    }
    window.addEventListener("keydown", onKeyDown)
    return () => window.removeEventListener("keydown", onKeyDown)
  }, [messages, selected, navigate, search])

  // In a mailbox, the title counts its own unread mail (the list's, unless a search narrows it).
  const narrowed = Boolean(filters.q || filters.provider)
  const open = mailboxes.mailboxes.find((b) => b.address === filters.to)
  const unread = !filters.to
    ? mailboxes.unread
    : open
      ? open.unread
      : narrowed
        ? null
        : messages.unread

  return (
    <>
      <title>{`${unread ? `(${unread}) ` : ""}${filters.to ?? "Inbox"} · localmail`}</title>
      <ResizablePanelGroup orientation="horizontal">
        <ResizablePanel defaultSize="18%" minSize="12%">
          <MailboxPane mailboxes={mailboxes} filters={filters} />
        </ResizablePanel>
        <ResizableHandle />
        <ResizablePanel defaultSize="30%" minSize="20%">
          <MessagePane list={messages} filters={filters} selected={selected} />
        </ResizablePanel>
        <ResizableHandle />
        <ResizablePanel defaultSize="52%" minSize="25%">
          <Outlet />
        </ResizablePanel>
      </ResizablePanelGroup>
    </>
  )
}

/** The right-hand pane with no message open. */
export function NoMessage() {
  return (
    <Empty className="h-full">
      <EmptyHeader>
        <EmptyMedia variant="icon">
          <MailIcon />
        </EmptyMedia>
        <EmptyTitle>Pick a message to read it</EmptyTitle>
        <EmptyDescription>
          <Kbd>j</Kbd> and <Kbd>k</Kbd> move through the list.
        </EmptyDescription>
      </EmptyHeader>
    </Empty>
  )
}

/** Revalidate whenever mail arrives, with the same long-poll tests use. */
function useLiveUpdates(cursor: number) {
  const { revalidate } = useRevalidator()
  const cursorRef = React.useRef(cursor)
  React.useEffect(() => {
    cursorRef.current = cursor
  }, [cursor])

  React.useEffect(() => {
    const controller = new AbortController()
    const { signal } = controller
    async function watch() {
      let reconnected = false
      while (!signal.aborted) {
        try {
          const res = await fetch(
            `/dev/api/messages/wait?after=${cursorRef.current}&timeout=25s`,
            { signal, headers: { accept: "application/json" } }
          )
          const body = await res.json()
          if (res.ok) {
            cursorRef.current = Math.max(cursorRef.current, body.message.id)
            await revalidate()
          } else if (reconnected || body.cursor < cursorRef.current) {
            // Back after a restart, which may have emptied the inbox (ids then start again).
            cursorRef.current = body.cursor
            await revalidate()
          }
          reconnected = false
        } catch {
          if (signal.aborted) return
          // Usually a restart. Keep the list as it is and try again until the server is back.
          reconnected = true
          await new Promise((resolve) => setTimeout(resolve, 1000))
        }
      }
    }
    void watch()
    return () => controller.abort()
  }, [revalidate])
}

/**
 * A text input bound to a URL parameter. It updates the URL once typing pauses, and follows
 * the URL on back and forward.
 */
function useUrlInput(name: string) {
  const [params, setParams] = useSearchParams()
  const location = useLocation()
  const navigationType = useNavigationType()
  const fromUrl = params.get(name) ?? ""
  const [value, setValue] = React.useState(fromUrl)
  const [syncedKey, setSyncedKey] = React.useState(location.key)
  if (navigationType === "POP" && location.key !== syncedKey) {
    setSyncedKey(location.key)
    setValue(fromUrl)
  }

  React.useEffect(() => {
    if (value.trim() === fromUrl.trim()) return
    const timer = setTimeout(() => {
      setParams(
        (current) => {
          const next = new URLSearchParams(current)
          if (value.trim()) next.set(name, value.trim())
          else next.delete(name)
          return next
        },
        { replace: true }
      )
    }, 200)
    return () => clearTimeout(timer)
  }, [value, fromUrl, name, setParams])

  return [value, setValue] as const
}

function MailboxPane({
  mailboxes,
  filters,
}: {
  mailboxes: Mailboxes
  filters: Filters
}) {
  const [params] = useSearchParams()
  const [filter, setFilter] = useUrlInput("mbox")
  const listed = mailboxes.mailboxes.some((b) => b.address === filters.to)

  // `total` is null when unknown: the open mailbox, hidden by the filter or the limit.
  const entry = (
    to: string | null,
    label: string,
    total: number | null,
    unread: number
  ) => {
    const current = to === filters.to
    return (
      <Link
        key={to ?? ""}
        to={mailboxLink(params, to)}
        title={to ? `Mail to ${to}` : undefined}
        aria-current={current ? "page" : undefined}
        className={cn(
          buttonVariants({
            variant: current ? "secondary" : "ghost",
            size: "sm",
          }),
          "w-full justify-between font-normal"
        )}
      >
        <span className="truncate">{label}</span>
        {unread ? (
          <Badge title={`${unread} unread`}>{unread}</Badge>
        ) : total != null ? (
          <span className="text-xs text-muted-foreground tabular-nums">
            {total}
          </span>
        ) : null}
      </Link>
    )
  }

  return (
    <nav aria-label="Mailboxes" className="flex h-full flex-col gap-2 py-3">
      <div className="px-4 text-xs font-medium text-muted-foreground">
        Mailboxes
      </div>
      <div className="px-3">
        <InputGroup>
          <InputGroupAddon>
            <SearchIcon />
          </InputGroupAddon>
          <InputGroupInput
            type="search"
            placeholder="Filter addresses"
            aria-label="Filter mailboxes"
            value={filter}
            onChange={(e) => setFilter(e.target.value)}
          />
        </InputGroup>
      </div>
      <ScrollArea className="min-h-0 flex-1">
        <div className="flex flex-col gap-0.5 px-3">
          {entry(null, "All mail", mailboxes.messages, mailboxes.unread)}
          {/* The open mailbox stays listed even if the filter (or the limit) would hide it. */}
          {filters.to && !listed && entry(filters.to, filters.to, null, 0)}
          {mailboxes.mailboxes.map((b) =>
            entry(b.address, b.address, b.total, b.unread)
          )}
          {mailboxes.total > mailboxes.mailboxes.length ? (
            <p className="px-2.5 py-2 text-xs text-muted-foreground">
              Showing {mailboxes.mailboxes.length} of {mailboxes.total}. Filter
              to find others.
            </p>
          ) : (
            !mailboxes.mailboxes.length &&
            filters.mbox && (
              <p className="px-2.5 py-2 text-xs text-muted-foreground">
                No addresses match.
              </p>
            )
          )}
        </div>
      </ScrollArea>
    </nav>
  )
}

function MessagePane({
  list,
  filters,
  selected,
}: {
  list: MessageList
  filters: Filters
  selected: number | null
}) {
  const info = useInfo()
  const [params, setParams] = useSearchParams()
  const { search } = useLocation()
  const [query, setQuery] = useUrlInput("q")
  const bulk = useFetcher()
  const listRef = React.useRef<HTMLDivElement>(null)

  React.useEffect(() => {
    listRef.current
      ?.querySelector(`[data-message-id="${selected}"]`)
      ?.scrollIntoView({ block: "nearest" })
  }, [selected])

  const providers = [
    { label: "All providers", value: null },
    ...info.providers.map((p) => ({ label: p.name, value: p.id })),
  ]
  const setProvider = (provider: string | null) =>
    setParams(
      (current) => {
        const next = new URLSearchParams(current)
        if (provider) next.set("provider", provider)
        else next.delete("provider")
        return next
      },
      { replace: true }
    )

  const narrowed = Boolean(filters.q || filters.provider)
  const submitBulk = (intent: "read-all" | "delete-all") =>
    bulk.submit(
      { intent, filters: listFilters(filters).toString(), search },
      { method: "post", action: "/" }
    )

  return (
    <section aria-label="Messages" className="flex h-full flex-col">
      {filters.to && (
        <div className="flex items-center gap-2 border-b px-3 py-2">
          <div className="min-w-0 flex-1">
            <div className="text-xs text-muted-foreground">Mail to</div>
            <div className="truncate text-sm font-medium">{filters.to}</div>
          </div>
          <CopyButton text={mailboxUrl(filters.to)} label="Copy link" />
          <Link
            to={mailboxLink(params, null)}
            className={buttonVariants({ variant: "ghost", size: "xs" })}
          >
            All mail
          </Link>
        </div>
      )}
      <div className="flex gap-2 px-3 pt-3 pb-2">
        <InputGroup>
          <InputGroupAddon>
            <SearchIcon />
          </InputGroupAddon>
          <InputGroupInput
            type="search"
            placeholder="Search"
            aria-label="Search messages"
            value={query}
            onChange={(e) => setQuery(e.target.value)}
          />
        </InputGroup>
        <Select
          items={providers}
          value={filters.provider}
          onValueChange={setProvider}
        >
          <SelectTrigger
            className="w-36 shrink-0"
            aria-label="Filter by provider"
          >
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            {providers.map((p) => (
              <SelectItem key={p.value ?? ""} value={p.value}>
                {p.label}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
      </div>
      <div className="flex items-center gap-1 border-b px-3 pb-2">
        <span className="text-xs text-muted-foreground">
          {narrowed
            ? `${list.total} matching`
            : `${plural(list.total, "message")}, ${list.unread} unread`}
        </span>
        <span className="flex-1" />
        <Button
          variant="ghost"
          size="xs"
          disabled={!list.unread}
          onClick={() => submitBulk("read-all")}
        >
          Mark all read
        </Button>
        <DeleteAll
          question={deleteQuestion(filters, list.total, info.providers)}
          disabled={!list.total}
          onConfirm={() => submitBulk("delete-all")}
        />
      </div>
      <ScrollArea className="min-h-0 flex-1">
        {list.messages.length ? (
          <div ref={listRef} role="list" className="flex flex-col gap-1 p-2">
            {list.messages.map((m) => (
              <MessageRow
                key={m.id}
                message={m}
                selected={m.id === selected}
                search={search}
              />
            ))}
          </div>
        ) : (
          <EmptyList filters={filters} narrowed={narrowed} />
        )}
      </ScrollArea>
    </section>
  )
}

/** "Delete every message?", or exactly what will go: "Delete the 2 messages to a@b.test matching “code”?" */
function deleteQuestion(
  filters: Filters,
  total: number,
  providers: { id: string; name: string }[]
) {
  const provider = providers.find((p) => p.id === filters.provider)?.name
  if (!filters.to && !filters.q && !provider) return "Delete every message?"
  return (
    `Delete ${total === 1 ? "the 1 message" : `the ${total} messages`}` +
    (filters.to ? ` to ${filters.to}` : "") +
    (filters.q ? ` matching “${filters.q}”` : "") +
    (provider ? ` sent through ${provider}` : "") +
    "?"
  )
}

function DeleteAll({
  question,
  disabled,
  onConfirm,
}: {
  question: string
  disabled: boolean
  onConfirm: () => void
}) {
  const [open, setOpen] = React.useState(false)
  return (
    <AlertDialog open={open} onOpenChange={setOpen}>
      <AlertDialogTrigger
        render={
          <Button
            variant="ghost"
            size="xs"
            className="text-destructive"
            disabled={disabled}
          />
        }
      >
        Delete all
      </AlertDialogTrigger>
      <AlertDialogContent>
        <AlertDialogHeader>
          <AlertDialogTitle>{question}</AlertDialogTitle>
          <AlertDialogDescription>
            Deleting a message deletes it for every recipient. This can't be
            undone.
          </AlertDialogDescription>
        </AlertDialogHeader>
        <AlertDialogFooter>
          <AlertDialogCancel>Cancel</AlertDialogCancel>
          <AlertDialogAction
            variant="destructive"
            onClick={() => {
              onConfirm()
              setOpen(false)
            }}
          >
            Delete
          </AlertDialogAction>
        </AlertDialogFooter>
      </AlertDialogContent>
    </AlertDialog>
  )
}

function MessageRow({
  message: m,
  selected,
  search,
}: {
  message: Summary
  selected: boolean
  search: string
}) {
  const to =
    [...m.to, ...m.cc].map((a) => a.email).join(", ") ||
    m.bcc.map((a) => a.email).join(", ")
  const subject =
    m.subject || (m.template ? `(template ${m.template.id})` : "(no subject)")
  return (
    <div role="listitem" data-message-id={m.id}>
      <Item
        size="sm"
        variant={selected ? "muted" : "default"}
        aria-current={selected ? "true" : undefined}
        render={<Link to={{ pathname: `/messages/${m.id}`, search }} />}
        className={cn(selected && "border-border")}
      >
        <ItemContent className="min-w-0 gap-0.5">
          <div className="flex items-center gap-2">
            {!m.read && (
              <span
                className="size-2 shrink-0 rounded-full bg-primary"
                aria-label="Unread"
              />
            )}
            <span className={cn("truncate", !m.read && "font-semibold")}>
              {m.from ? m.from.name || m.from.email : "(no sender)"}
            </span>
            <span
              className="ml-auto shrink-0 text-xs text-muted-foreground tabular-nums"
              title={fullTime(m.received_at)}
            >
              {when(m.received_at)}
            </span>
          </div>
          <ItemTitle className={cn("w-full", m.read && "font-normal")}>
            <span className="truncate">{subject}</span>
          </ItemTitle>
          <div className="truncate text-xs text-muted-foreground">To {to}</div>
          {m.preview && (
            <div className="truncate text-xs text-muted-foreground">
              {m.preview}
            </div>
          )}
          <div className="flex flex-wrap items-center gap-1 pt-1">
            <ProviderBadge id={m.provider} />
            {m.has_attachments && (
              <Badge variant="outline" title="Has attachments">
                <PaperclipIcon />
              </Badge>
            )}
            {m.scenario && (
              <Badge variant="outline" title={`Scenario address ${m.scenario}`}>
                <TriangleAlertIcon />
                {m.scenario.split("@")[0]}
              </Badge>
            )}
            {m.template && <Badge variant="secondary">template</Badge>}
          </div>
        </ItemContent>
      </Item>
    </div>
  )
}

function EmptyList({
  filters,
  narrowed,
}: {
  filters: Filters
  narrowed: boolean
}) {
  const info = useInfo()
  return (
    <Empty>
      <EmptyHeader>
        <EmptyMedia variant="icon">
          <InboxIcon />
        </EmptyMedia>
        <EmptyTitle>
          {narrowed
            ? "No messages match"
            : filters.to
              ? `No mail to ${filters.to} yet`
              : "No messages yet"}
        </EmptyTitle>
        {!narrowed && (
          <EmptyDescription>
            {filters.to ? (
              "It will appear here as soon as it arrives."
            ) : (
              <>
                Send over SMTP to <code>{info.smtp}</code> or call a provider
                API at <code>{info.base_url}</code>.{" "}
                <Link to="/setup">Setup</Link> shows how.
              </>
            )}
          </EmptyDescription>
        )}
      </EmptyHeader>
    </Empty>
  )
}
