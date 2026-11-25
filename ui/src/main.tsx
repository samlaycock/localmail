import { StrictMode } from "react"
import { createRoot } from "react-dom/client"
import { createBrowserRouter } from "react-router"
import { RouterProvider } from "react-router/dom"

import "./index.css"
import "./theme.css"
import { ThemeProvider } from "@/components/theme-provider"
import { infoLoader } from "@/lib/info"
import { Attempts } from "@/routes/attempts"
import { Inbox, inboxAction, inboxLoader, NoMessage } from "@/routes/inbox"
import { MessageView, messageAction, messageLoader } from "@/routes/message"
import { NotFound } from "@/routes/not-found"
import { Loading, Root, RouteError } from "@/routes/root"
import { Setup, setupLoader } from "@/routes/setup"

// Links from before the app moved to path routes: /#/m/42 is now /messages/42.
const legacy = location.hash.match(/^#\/m\/(\d+)/)
if (legacy)
  history.replaceState(null, "", `/messages/${legacy[1]}${location.search}`)

const router = createBrowserRouter([
  {
    id: "root",
    loader: infoLoader,
    // What it loads (the version, addresses and providers) can't change while the page is open.
    shouldRevalidate: () => false,
    Component: Root,
    ErrorBoundary: RouteError,
    HydrateFallback: Loading,
    children: [
      {
        id: "inbox",
        path: "/",
        loader: inboxLoader,
        action: inboxAction,
        Component: Inbox,
        ErrorBoundary: RouteError,
        children: [
          { index: true, Component: NoMessage },
          {
            path: "messages/:id",
            loader: messageLoader,
            action: messageAction,
            Component: MessageView,
            ErrorBoundary: RouteError,
          },
        ],
      },
      { path: "/attempts", Component: Attempts },
      {
        path: "/setup",
        loader: setupLoader,
        Component: Setup,
        ErrorBoundary: RouteError,
      },
      { path: "*", Component: NotFound },
    ],
  },
])

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <ThemeProvider storageKey="localmail.theme">
      <RouterProvider router={router} />
    </ThemeProvider>
  </StrictMode>
)
