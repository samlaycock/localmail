import { Link, useLoaderData, type LoaderFunctionArgs } from "react-router"

import { CodeBlock } from "@/components/code-block"
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card"
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table"
import { api, type Scenarios, type Setup as SetupData } from "@/lib/api"

export async function setupLoader({ request }: LoaderFunctionArgs) {
  const { signal } = request
  const [setup, scenarios] = await Promise.all([
    api<SetupData>("/dev/api/setup", { signal }),
    api<Scenarios>("/dev/api/scenarios", { signal }),
  ])
  return { setup, scenarios }
}

export function Setup() {
  const { setup, scenarios } = useLoaderData<typeof setupLoader>()
  return (
    <div className="mx-auto flex max-w-6xl flex-col gap-6 p-6">
      <title>Setup · localmail</title>
      <Card>
        <CardHeader>
          <CardDescription>Local email for development</CardDescription>
          <CardTitle className="text-xl leading-snug">
            Send to <span className="text-primary">{setup.smtp}</span> or the
            provider APIs at{" "}
            <span className="text-primary">{setup.base_url}</span>
          </CardTitle>
          <CardDescription>
            localmail keeps every message instead of delivering it. Point an
            SMTP client or your email provider's SDK at it, and read what was
            sent in the{" "}
            <Link to="/" className="text-primary hover:underline">
              inbox
            </Link>{" "}
            or through the dev API. Any credentials are accepted, but the APIs
            still expect the kind their provider uses (a bearer token, a server
            token, a SigV4 signature).
          </CardDescription>
        </CardHeader>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle>Endpoints</CardTitle>
        </CardHeader>
        <CardContent>
          <Table>
            <TableBody>
              {setup.endpoints.map((e) => (
                <TableRow key={e.label}>
                  <TableCell className="whitespace-normal">{e.label}</TableCell>
                  <TableCell className="font-mono text-xs break-all whitespace-normal">
                    {e.url}
                  </TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle>Pointing an SDK at localmail</CardTitle>
        </CardHeader>
        <CardContent className="grid gap-4 lg:grid-cols-2">
          {setup.snippets.map((s) => (
            <CodeBlock key={s.title} title={s.title} code={s.code} wrap />
          ))}
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle>Scenario addresses</CardTitle>
          <CardDescription>
            Send to one of these to make the send itself fail (or slow down) the
            way the provider would. A <code>+suffix</code> is ignored, e.g.{" "}
            <code>ratelimit+signup@{scenarios.domain}</code>. Flaky counters can
            be reset with <code>POST /dev/api/scenarios/reset</code>.
          </CardDescription>
        </CardHeader>
        <CardContent>
          <Table>
            <TableHeader>
              <TableRow>
                <TableHead>Address</TableHead>
                <TableHead>Effect</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {scenarios.scenarios.map((s) => (
                <TableRow key={s.address}>
                  <TableCell className="font-mono text-xs">
                    {s.address}
                  </TableCell>
                  <TableCell className="whitespace-normal">
                    {s.effect}
                  </TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle>Test automation</CardTitle>
        </CardHeader>
        <CardContent>
          <CodeBlock title={setup.test.title} code={setup.test.code} wrap />
        </CardContent>
      </Card>
    </div>
  )
}
