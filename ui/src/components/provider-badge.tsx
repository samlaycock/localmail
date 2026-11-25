import { Badge } from "@/components/ui/badge"
import { useInfo } from "@/lib/info"

/** A provider's name, with its brand colour as a dot. */
export function ProviderBadge({ id }: { id: string }) {
  const { providers } = useInfo()
  const provider = providers.find((p) => p.id === id) ?? {
    name: id,
    color: "var(--muted-foreground)",
  }
  return (
    <Badge variant="outline">
      <span
        className="size-1.5 rounded-full"
        style={{ backgroundColor: provider.color }}
      />
      {provider.name}
    </Badge>
  )
}
