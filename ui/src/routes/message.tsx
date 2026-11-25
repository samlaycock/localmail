import {
  DownloadIcon,
  InfoIcon,
  KeyRoundIcon,
  LinkIcon,
  MailXIcon,
  PaperclipIcon,
  Trash2Icon,
  TriangleAlertIcon,
} from "lucide-react"
import * as React from "react"
import {
  Link,
  redirect,
  useFetcher,
  useLoaderData,
  useLocation,
  useRouteLoaderData,
  useSearchParams,
  type ActionFunctionArgs,
  type LoaderFunctionArgs,
} from "react-router"

import { CodeBlock } from "@/components/code-block"
import { CopyButton } from "@/components/copy-button"
import { ProviderBadge } from "@/components/provider-badge"
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import {
  Empty,
  EmptyHeader,
  EmptyMedia,
  EmptyTitle,
} from "@/components/ui/empty"
import {
  Item,
  ItemActions,
  ItemContent,
  ItemDescription,
  ItemMedia,
  ItemTitle,
} from "@/components/ui/item"
import { ScrollArea } from "@/components/ui/scroll-area"
import { Spinner } from "@/components/ui/spinner"
import { Table, TableBody, TableCell, TableRow } from "@/components/ui/table"
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs"
import {
  api,
  ApiError,
  type Address,
  type Extracted,
  type Message,
} from "@/lib/api"
import { formText } from "@/lib/form"
import { address, fullTime, size } from "@/lib/format"
import { mailboxLink } from "@/lib/inbox"
import type { inboxLoader } from "@/routes/inbox"

export async function messageLoader({ params, request }: LoaderFunctionArgs) {
  const id = params.id ?? ""
  const { signal } = request
  try {
    const [message, extracted] = await Promise.all([
      api<Message>(`/dev/api/messages/${id}`, { signal }),
      api<Extracted>(`/dev/api/messages/${id}/extract?extract=otp,links`, {
        signal,
      }).catch(() => null),
    ])
    return { id, message, extracted }
  } catch (e) {
    // Deleted (here, or by a test), or not an id at all.
    if (e instanceof ApiError && (e.status === 404 || e.status === 400)) {
      return { id, message: null, extracted: null }
    }
    throw e
  }
}

export async function messageAction({ params, request }: ActionFunctionArgs) {
  const form = await request.formData()
  switch (formText(form, "intent")) {
    case "read":
      await api(`/dev/api/messages/${params.id}/read`, { method: "POST" })
      return null
    case "delete":
      await api(`/dev/api/messages/${params.id}`, { method: "DELETE" })
      return redirect(formText(form, "next") || "/")
  }
  throw new Response("Unknown intent", { status: 400 })
}

type Tab = "html" | "text" | "headers" | "source" | "request" | "details"

export function MessageView() {
  const { id, message, extracted } = useLoaderData<typeof messageLoader>()
  const { submit } = useFetcher()

  // Opening a message marks it read.
  React.useEffect(() => {
    if (message && !message.read) {
      void submit({ intent: "read" }, { method: "post" })
    }
  }, [message, submit])

  // The tab stays the same from one message to the next, where the message has it.
  const [preferred, setPreferred] = React.useState<Tab>("html")

  if (!message) {
    return (
      <Empty className="h-full">
        <EmptyHeader>
          <EmptyMedia variant="icon">
            <MailXIcon />
          </EmptyMedia>
          <EmptyTitle>Message {id} isn't here any more</EmptyTitle>
        </EmptyHeader>
      </Empty>
    )
  }

  const tabs: [Tab, string][] = []
  if (message.html != null) tabs.push(["html", "HTML"])
  if (message.text != null) tabs.push(["text", "Text"])
  tabs.push(["headers", "Headers"], ["source", "Source"])
  if (message.request != null) tabs.push(["request", "Request"])
  tabs.push(["details", "Details"])
  const tab = tabs.some(([t]) => t === preferred) ? preferred : tabs[0][0]

  return (
    <ScrollArea className="h-full">
      <article className="flex flex-col gap-4 p-6">
        <MessageHeader message={message} />
        {message.template && !message.template.rendered && (
          <Alert>
            <InfoIcon />
            <AlertTitle>Sent with a provider template</AlertTitle>
            <AlertDescription>
              <p>
                The template{" "}
                <code>{`${message.template.field} = ${message.template.id}`}</code>{" "}
                lives in the provider account, so localmail shows its data
                instead of rendering it.
              </p>
            </AlertDescription>
          </Alert>
        )}
        <Found message={message} extracted={extracted} />
        <Tabs value={tab} onValueChange={(value) => setPreferred(value as Tab)}>
          <TabsList>
            {tabs.map(([value, label]) => (
              <TabsTrigger key={value} value={value}>
                {label}
              </TabsTrigger>
            ))}
          </TabsList>
          <TabsContent value="html">
            <iframe
              key={message.id}
              src={`/dev/api/messages/${message.id}/html`}
              title="HTML body"
              sandbox="allow-popups allow-popups-to-escape-sandbox"
              referrerPolicy="no-referrer"
              // Email HTML is written for a white page, whatever the app's theme.
              className="h-[70vh] min-h-96 w-full rounded-lg border bg-white"
            />
          </TabsContent>
          <TabsContent value="text">
            <CodeBlock code={message.text ?? ""} wrap />
          </TabsContent>
          <TabsContent value="headers">
            <KeyValues
              rows={message.headers.map((h) => [h.name, h.value])}
              monoKeys
            />
          </TabsContent>
          <TabsContent value="source">
            <Source key={message.id} id={message.id} />
          </TabsContent>
          <TabsContent value="request" className="flex flex-col gap-2">
            <p className="text-sm text-muted-foreground">
              What your app sent to <code>{message.endpoint}</code>. Long values
              such as attachment contents are shortened.
            </p>
            <CodeBlock
              title="Request"
              code={
                typeof message.request === "string"
                  ? message.request
                  : JSON.stringify(message.request, null, 2)
              }
            />
          </TabsContent>
          <TabsContent value="details">
            <KeyValues rows={detailRows(message)} />
          </TabsContent>
        </Tabs>
      </article>
    </ScrollArea>
  )
}

function MessageHeader({ message: m }: { message: Message }) {
  const [params] = useSearchParams()
  const { search } = useLocation()
  const inbox = useRouteLoaderData<typeof inboxLoader>("inbox")
  const { submit, state } = useFetcher()

  function remove() {
    // Open the next message down the list (or up, at the end).
    const list = inbox?.messages.messages ?? []
    const i = list.findIndex((x) => x.id === m.id)
    const next = list[i + 1] ?? list[i - 1]
    void submit(
      {
        intent: "delete",
        next: next ? `/messages/${next.id}${search}` : `/${search}`,
      },
      { method: "post" }
    )
  }

  // Recipients link to their mailboxes.
  const recipients = (list: Address[]) =>
    list.length
      ? list.map((a, i) => (
          <React.Fragment key={`${a.email}-${i}`}>
            {i > 0 && ", "}
            <Link
              to={mailboxLink(params, a.email)}
              className="hover:underline"
              title={`Mail to ${a.email}`}
            >
              {address(a)}
            </Link>
          </React.Fragment>
        ))
      : null
  const rows: [string, React.ReactNode][] = [
    ["From", address(m.from) || null],
    ["To", recipients(m.to)],
    ["Cc", recipients(m.cc)],
    ["Bcc", recipients(m.bcc)],
    ["Reply-To", m.reply_to.map(address).join(", ") || null],
    ["Date", fullTime(m.received_at)],
  ]

  return (
    <header className="flex flex-col gap-3">
      <div className="flex items-start gap-3">
        <h1 className="min-w-0 flex-1 text-xl font-semibold tracking-tight break-words">
          {m.subject ||
            (m.template ? `(template ${m.template.id})` : "(no subject)")}
        </h1>
        <div className="flex shrink-0 gap-2">
          <Button
            variant="outline"
            size="sm"
            nativeButton={false}
            render={<a href={`/dev/api/messages/${m.id}/raw?download`} />}
          >
            <DownloadIcon data-icon="inline-start" />
            Download .eml
          </Button>
          <Button
            variant="destructive"
            size="sm"
            onClick={remove}
            disabled={state !== "idle"}
          >
            <Trash2Icon data-icon="inline-start" />
            Delete
          </Button>
        </div>
      </div>
      <dl className="grid grid-cols-[max-content_1fr] gap-x-3 gap-y-1 text-sm">
        {rows
          .filter(([, value]) => value != null)
          .map(([label, value]) => (
            <React.Fragment key={label}>
              <dt className="text-muted-foreground">{label}</dt>
              <dd className="min-w-0 break-words">{value}</dd>
            </React.Fragment>
          ))}
      </dl>
      <div className="flex flex-wrap items-center gap-1.5">
        <ProviderBadge id={m.provider} />
        <Badge variant="outline" className="font-mono" title="Endpoint">
          {m.endpoint}
        </Badge>
        <Badge
          variant="secondary"
          className="font-mono"
          title="The message ID the provider API returned"
        >
          {m.provider_message_id}
        </Badge>
        {m.tags.map((t) => (
          <Badge key={t} variant="secondary">
            {t}
          </Badge>
        ))}
        {m.scenario && (
          <Badge variant="outline" title="Scenario address">
            <TriangleAlertIcon />
            {m.scenario}
          </Badge>
        )}
      </div>
    </header>
  )
}

/** The one-time code and links found in the message, and its attachments. */
function Found({
  message,
  extracted,
}: {
  message: Message
  extracted: Extracted | null
}) {
  const links = extracted?.links ?? []
  const items: React.ReactNode[] = []
  if (extracted?.otp) {
    items.push(
      <Item key="otp" variant="outline" size="sm">
        <ItemMedia variant="icon">
          <KeyRoundIcon />
        </ItemMedia>
        <ItemContent>
          <ItemDescription>One-time code</ItemDescription>
          <ItemTitle className="font-mono text-base tracking-widest">
            {extracted.otp}
          </ItemTitle>
        </ItemContent>
        <ItemActions>
          <CopyButton text={extracted.otp} />
        </ItemActions>
      </Item>
    )
  }
  for (const [i, link] of links.slice(0, 3).entries()) {
    items.push(
      <Item key={`link-${i}`} variant="outline" size="sm">
        <ItemMedia variant="icon">
          <LinkIcon />
        </ItemMedia>
        <ItemContent className="min-w-0">
          <ItemTitle className="w-full">
            <a
              href={link.href}
              target="_blank"
              rel="noopener"
              className="truncate hover:underline"
            >
              {link.text || link.href}
            </a>
          </ItemTitle>
          <ItemDescription className="line-clamp-1 break-all">
            {link.href}
          </ItemDescription>
        </ItemContent>
        <ItemActions>
          <CopyButton text={link.href} />
        </ItemActions>
      </Item>
    )
  }
  for (const a of message.attachments) {
    items.push(
      <Item
        key={`attachment-${a.index}`}
        variant="outline"
        size="sm"
        render={
          <a
            href={`/dev/api/messages/${message.id}/attachments/${a.index}?download`}
          />
        }
      >
        <ItemMedia variant="icon">
          <PaperclipIcon />
        </ItemMedia>
        <ItemContent className="min-w-0">
          <ItemTitle className="w-full">
            <span className="truncate">{a.filename}</span>
          </ItemTitle>
          <ItemDescription>
            {a.content_type} · {size(a.size)}
            {a.content_id ? " · inline" : ""}
          </ItemDescription>
        </ItemContent>
        <ItemActions>
          <DownloadIcon className="size-4 text-muted-foreground" />
        </ItemActions>
      </Item>
    )
  }
  if (!items.length) return null
  return (
    <div className="flex flex-col gap-2">
      <div className="grid gap-2 xl:grid-cols-2">{items}</div>
      {links.length > 3 && (
        <p className="text-xs text-muted-foreground">
          +{links.length - 3} more links. The Text and HTML tabs have them all.
        </p>
      )}
    </div>
  )
}

/** The MIME source, fetched when its tab opens. */
function Source({ id }: { id: number }) {
  const [raw, setRaw] = React.useState<string | null>(null)
  React.useEffect(() => {
    const controller = new AbortController()
    fetch(`/dev/api/messages/${id}/raw`, { signal: controller.signal })
      .then((res) => res.text())
      .then(setRaw)
      .catch(() => {})
    return () => controller.abort()
  }, [id])
  if (raw == null) return <Spinner className="m-4" />
  return <CodeBlock title="MIME source" code={raw} />
}

function detailRows(m: Message): [string, string][] {
  const json = (v: unknown) =>
    typeof v === "string" ? v : JSON.stringify(v, null, 2)
  const rows: [string, string][] = [
    ["localmail id", String(m.id)],
    ["Provider message ID", m.provider_message_id],
    ["Endpoint", m.endpoint],
    ["Message-ID", m.message_id ?? ""],
    ["Size", size(m.size)],
  ]
  if (m.template) {
    rows.push(
      ["Template", `${m.template.field} = ${m.template.id}`],
      ["Template data", json(m.template.data)]
    )
  }
  for (const [k, v] of Object.entries(m.details)) rows.push([k, json(v)])
  if (m.envelope) {
    rows.push(
      ["MAIL FROM", m.envelope.mail_from || "<>"],
      ["RCPT TO", m.envelope.rcpt_to.join("\n")]
    )
    if (m.envelope.helo) rows.push(["EHLO", m.envelope.helo])
    if (m.envelope.auth_user) rows.push(["AUTH user", m.envelope.auth_user])
  }
  return rows.filter(([, v]) => v !== "")
}

function KeyValues({
  rows,
  monoKeys = false,
}: {
  rows: [string, string][]
  monoKeys?: boolean
}) {
  return (
    <div className="rounded-lg border">
      <Table>
        <TableBody>
          {rows.map(([k, v], i) => (
            <TableRow key={`${k}-${i}`}>
              <TableCell
                className={
                  monoKeys
                    ? "w-0 align-top font-mono text-xs text-muted-foreground"
                    : "w-0 align-top text-muted-foreground"
                }
              >
                {k}
              </TableCell>
              <TableCell className="font-mono text-xs break-all whitespace-pre-wrap">
                {v}
              </TableCell>
            </TableRow>
          ))}
        </TableBody>
      </Table>
    </div>
  )
}
