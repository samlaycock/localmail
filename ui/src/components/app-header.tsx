import { MailIcon } from "lucide-react"
import { Link, matchPath, useLocation } from "react-router"

import { ModeToggle } from "@/components/mode-toggle"
import { Badge } from "@/components/ui/badge"
import { buttonVariants } from "@/components/ui/button"
import { useInfo } from "@/lib/info"

/** Each nav entry, and the paths that show it as current. */
const NAV = [
  { to: "/", label: "Inbox", paths: ["/", "/messages/:id"] },
  { to: "/attempts", label: "Attempts", paths: ["/attempts"] },
  { to: "/setup", label: "Setup", paths: ["/setup"] },
]

export function AppHeader() {
  const info = useInfo()
  const { pathname } = useLocation()
  return (
    <header className="flex h-12 shrink-0 items-center gap-4 border-b px-4">
      <Link to="/" className="flex items-center gap-2 text-sm font-semibold">
        <span className="flex size-6 items-center justify-center rounded-md bg-primary text-primary-foreground">
          <MailIcon className="size-3.5" />
        </span>
        localmail
        <Badge variant="secondary" className="font-mono font-normal">
          {info.version}
        </Badge>
      </Link>
      <nav className="flex items-center gap-1">
        {NAV.map(({ to, label, paths }) => {
          const current = paths.some((p) => matchPath(p, pathname))
          return (
            <Link
              key={to}
              to={to}
              aria-current={current ? "page" : undefined}
              className={buttonVariants({
                variant: current ? "secondary" : "ghost",
                size: "sm",
              })}
            >
              {label}
            </Link>
          )
        })}
      </nav>
      <div className="ml-auto flex items-center gap-2">
        <ModeToggle />
      </div>
    </header>
  )
}
