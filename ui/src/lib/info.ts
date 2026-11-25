import { useRouteLoaderData } from "react-router"

import { api, type Info } from "@/lib/api"

/** The root route's data: what every page knows about this server. */
export const infoLoader = () => api<Info>("/dev/api/info")

export function useInfo() {
  const info = useRouteLoaderData<typeof infoLoader>("root")
  if (!info) throw new Error("useInfo needs the root route's data")
  return info
}
