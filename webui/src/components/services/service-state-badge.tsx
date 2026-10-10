import { Badge } from "@/components/ui/badge"
import type { ServiceState } from "@/lib/api"

const STYLES: Record<ServiceState, string> = {
  running:
    "border-emerald-600/30 bg-emerald-500/10 text-emerald-700 dark:text-emerald-400",
  starting: "border-sky-600/30 bg-sky-500/10 text-sky-700 dark:text-sky-400",
  backoff:
    "border-amber-600/30 bg-amber-500/10 text-amber-700 dark:text-amber-400",
  failed: "border-destructive/30 bg-destructive/10 text-destructive",
  stopped: "bg-secondary text-secondary-foreground",
  disabled: "border-border text-muted-foreground",
}

const LABELS: Record<ServiceState, string> = {
  running: "running",
  starting: "starting",
  backoff: "backing off",
  failed: "failed",
  stopped: "stopped",
  disabled: "disabled",
}

export function ServiceStateBadge({ state }: { state: ServiceState }) {
  return (
    <Badge variant="outline" className={STYLES[state] ?? ""}>
      {LABELS[state] ?? state}
    </Badge>
  )
}
