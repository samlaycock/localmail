import type { Address } from "@/lib/api"

const pad = (n: number) => String(n).padStart(2, "0")

/** `14:05` today, `2026-11-24 14:05` before that. */
export function when(ms: number) {
  const d = new Date(ms)
  const time = `${pad(d.getHours())}:${pad(d.getMinutes())}`
  if (d.toDateString() === new Date().toDateString()) return time
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${time}`
}

export const fullTime = (ms: number) => new Date(ms).toLocaleString()

export const address = (a: Address | null | undefined) =>
  a ? (a.name ? `${a.name} <${a.email}>` : a.email) : ""

export function size(n: number) {
  if (n < 1024) return `${n} B`
  if (n < 1048576) return `${(n / 1024).toFixed(1)} KB`
  return `${(n / 1048576).toFixed(1)} MB`
}

export const plural = (n: number, one: string, many = `${one}s`) =>
  `${n} ${n === 1 ? one : many}`
