import {
  ChevronRightIcon,
  PauseIcon,
  PlayIcon,
  TriangleAlertIcon,
} from "lucide-react"
import * as React from "react"
import { Link } from "react-router"

import { CodeBlock } from "@/components/code-block"
import { ProviderBadge } from "@/components/provider-badge"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card"
import { Checkbox } from "@/components/ui/checkbox"
import { Label } from "@/components/ui/label"
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select"
import { Spinner } from "@/components/ui/spinner"
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table"
import { api, type Attempt, type Attempts as AttemptsResponse } from "@/lib/api"
import { fullTime } from "@/lib/format"
import { useInfo } from "@/lib/info"
import { cn } from "@/lib/utils"

/** How many attempts the page keeps, newest first. */
const KEEP = 1000

/** Follows the attempts log, polling for the attempts after the newest one it has. */
function useAttempts(paused: boolean) {
  const [entries, setEntries] = React.useState<Attempt[] | null>(null)
  const latest = React.useRef(0)

  React.useEffect(() => {
    if (paused) return
    let stopped = false
    let busy = false
    async function poll() {
      // A slow answer mustn't overlap the next tick, which would add its attempts twice.
      if (busy) return
      busy = true
      try {
        let res = await api<AttemptsResponse>(
          `/dev/api/attempts?after=${latest.current}`
        )
        if (res.latest_id < latest.current) {
          // Restarted: ids start again from 1.
          res = await api<AttemptsResponse>("/dev/api/attempts")
          if (!stopped) setEntries([])
        }
        if (stopped) return
        latest.current = res.latest_id
        setEntries((current) =>
          [...res.attempts, ...(current ?? [])].slice(0, KEEP)
        )
      } catch {
        // Try again on the next tick.
      } finally {
        busy = false
      }
    }
    void poll()
    const timer = setInterval(() => void poll(), 1500)
    return () => {
      stopped = true
      clearInterval(timer)
    }
  }, [paused])

  return [entries, setEntries] as const
}

export function Attempts() {
  const info = useInfo()
  const [paused, setPaused] = React.useState(false)
  const [entries, setEntries] = useAttempts(paused)
  const [provider, setProvider] = React.useState<string | null>(null)
  const [failuresOnly, setFailuresOnly] = React.useState(false)
  const [open, setOpen] = React.useState<ReadonlySet<number>>(new Set())

  const providers = [
    { label: "All providers", value: null },
    ...info.providers.map((p) => ({ label: p.name, value: p.id })),
  ]
  const shown = (entries ?? []).filter(
    (a) =>
      (!provider || a.provider === provider) &&
      (!failuresOnly || a.outcome !== "accepted")
  )
  const toggle = (id: number) =>
    setOpen((current) => {
      const next = new Set(current)
      if (!next.delete(id)) next.add(id)
      return next
    })

  async function clear() {
    await api("/dev/api/attempts", { method: "DELETE" })
    setEntries([])
  }

  return (
    <div className="mx-auto flex max-w-6xl flex-col gap-6 p-6">
      <title>Attempts · localmail</title>
      <Card>
        <CardHeader>
          <CardDescription>Attempts</CardDescription>
          <CardTitle className="text-xl">Every send, accepted or not</CardTitle>
          <CardDescription>
            The last {info.attempts_capacity} sends over SMTP and the provider
            APIs, newest first, including the ones that failed validation or hit
            a scenario address. Click one to see the request and the response.
            Tests can read the same list from <code>GET /dev/api/attempts</code>
            .
          </CardDescription>
        </CardHeader>
        <CardContent className="flex flex-col gap-4">
          <div className="flex flex-wrap items-center gap-3">
            <Select
              items={providers}
              value={provider}
              onValueChange={setProvider}
            >
              <SelectTrigger className="w-44" aria-label="Filter by provider">
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
            <Label>
              <Checkbox
                checked={failuresOnly}
                onCheckedChange={(checked) => setFailuresOnly(checked)}
              />
              Failures only
            </Label>
            <span className="flex-1" />
            <Button
              variant="outline"
              size="sm"
              onClick={() => setPaused(!paused)}
            >
              {paused ? (
                <PlayIcon data-icon="inline-start" />
              ) : (
                <PauseIcon data-icon="inline-start" />
              )}
              {paused ? "Resume" : "Pause"}
            </Button>
            <Button variant="ghost" size="sm" onClick={clear}>
              Clear
            </Button>
          </div>
          {entries == null ? (
            <Spinner className="mx-auto my-6 text-muted-foreground" />
          ) : shown.length ? (
            <div className="rounded-lg border">
              <Table>
                <TableHeader>
                  <TableRow>
                    <TableHead className="w-0" />
                    <TableHead>Time</TableHead>
                    <TableHead>Provider</TableHead>
                    <TableHead>Endpoint</TableHead>
                    <TableHead>Status</TableHead>
                    <TableHead>Recipients</TableHead>
                    <TableHead>Outcome</TableHead>
                  </TableRow>
                </TableHeader>
                <TableBody>
                  {shown.map((a) => (
                    <AttemptRows
                      key={a.id}
                      attempt={a}
                      open={open.has(a.id)}
                      onToggle={() => toggle(a.id)}
                    />
                  ))}
                </TableBody>
              </Table>
            </div>
          ) : (
            <p className="py-6 text-center text-sm text-muted-foreground">
              {entries.length
                ? "No attempts match these filters."
                : "No sends yet."}
            </p>
          )}
        </CardContent>
      </Card>
    </div>
  )
}

function AttemptRows({
  attempt: a,
  open,
  onToggle,
}: {
  attempt: Attempt
  open: boolean
  onToggle: () => void
}) {
  const details = (
    [
      ["Subject", a.subject],
      ["From", a.from],
      ["Error", a.error_description],
      ["Warnings", a.warnings?.join("\n")],
      ["Duration", `${a.duration_ms} ms`],
    ] as const
  ).filter(([, v]) => v)
  return (
    <>
      <TableRow
        tabIndex={0}
        aria-expanded={open}
        className="cursor-pointer"
        onClick={onToggle}
        onKeyDown={(e) => {
          if (e.key === "Enter" || e.key === " ") {
            e.preventDefault()
            onToggle()
          }
        }}
      >
        <TableCell>
          <ChevronRightIcon
            className={cn(
              "size-4 text-muted-foreground transition-transform",
              open && "rotate-90"
            )}
          />
        </TableCell>
        <TableCell className="font-mono text-xs" title={fullTime(a.at)}>
          {new Date(a.at).toLocaleTimeString()}
        </TableCell>
        <TableCell>
          <ProviderBadge id={a.provider} />
        </TableCell>
        <TableCell className="font-mono text-xs">{a.endpoint}</TableCell>
        <TableCell>
          <div className="flex items-center gap-1">
            <Badge
              variant={a.status >= 400 ? "destructive" : "secondary"}
              className="font-mono"
            >
              {a.status}
            </Badge>
            {a.outcome === "partial" && (
              <Badge variant="outline">partial</Badge>
            )}
          </div>
        </TableCell>
        <TableCell className="max-w-64 truncate">
          {a.recipients.join(", ")}
        </TableCell>
        <TableCell className="max-w-80">
          <div className="flex items-center gap-1">
            {a.error ? (
              <span className="truncate text-destructive">{a.error}</span>
            ) : a.messages.length ? (
              <span className="text-muted-foreground">
                {a.messages.length} stored
              </span>
            ) : null}
            {a.scenario && (
              <Badge variant="outline" title={`Scenario address ${a.scenario}`}>
                <TriangleAlertIcon />
                {a.scenario.split("@")[0]}
              </Badge>
            )}
          </div>
        </TableCell>
      </TableRow>
      {open && (
        <TableRow className="hover:bg-transparent">
          <TableCell colSpan={7} className="bg-muted/30 p-4 whitespace-normal">
            <div className="flex flex-col gap-4">
              <dl className="grid grid-cols-[max-content_1fr] gap-x-4 gap-y-1 text-sm">
                {details.map(([k, v]) => (
                  <React.Fragment key={k}>
                    <dt className="text-muted-foreground">{k}</dt>
                    <dd className="min-w-0 break-words whitespace-pre-wrap">
                      {v}
                    </dd>
                  </React.Fragment>
                ))}
                {a.messages.length > 0 && (
                  <>
                    <dt className="text-muted-foreground">Messages</dt>
                    <dd className="flex flex-wrap gap-x-2">
                      {a.messages.map((id) => (
                        <Link
                          key={id}
                          to={`/messages/${id}`}
                          className="text-primary hover:underline"
                        >
                          #{id}
                        </Link>
                      ))}
                    </dd>
                  </>
                )}
              </dl>
              {a.request && (
                <CodeBlock
                  title={a.provider === "smtp" ? "Transcript" : "Request"}
                  code={a.request}
                />
              )}
              {a.response && <CodeBlock title="Response" code={a.response} />}
            </div>
          </TableCell>
        </TableRow>
      )}
    </>
  )
}
