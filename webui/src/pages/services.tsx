import * as React from "react"
import {
  LockIcon,
  PlayIcon,
  RotateCwIcon,
  ScrollTextIcon,
  SquareIcon,
} from "lucide-react"
import { Link } from "react-router"
import { toast } from "sonner"

import { ConfirmDialog } from "@/components/common/confirm-dialog"
import { ServiceStateBadge } from "@/components/services/service-state-badge"
import { Button } from "@/components/ui/button"
import { Card, CardContent } from "@/components/ui/card"
import { Skeleton } from "@/components/ui/skeleton"
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table"
import {
  Tooltip,
  TooltipContent,
  TooltipTrigger,
} from "@/components/ui/tooltip"
import { usePolling } from "@/hooks/use-polling"
import {
  ApiError,
  api,
  errorMessage,
  type ServiceAction,
  type ServiceStatus,
} from "@/lib/api"
import { useAuth } from "@/lib/auth"
import { formatDuration } from "@/lib/format"

const ACTIVE = new Set(["running", "starting", "backoff"])

function actionError(name: string, action: ServiceAction, e: unknown) {
  if (e instanceof ApiError && e.status === 409) {
    return `${name} is critical (watchdog feeder) and cannot be ${action === "stop" ? "stopped" : "restarted"}`
  }
  return `${action} ${name} failed: ${errorMessage(e)}`
}

function Actions({
  service,
  pending,
  onAction,
  onStop,
}: {
  service: ServiceStatus
  pending: boolean
  onAction: (action: ServiceAction) => void
  onStop: () => void
}) {
  const active = ACTIVE.has(service.state)
  return (
    <div className="flex justify-end gap-1">
      {!active && (
        <Button
          variant="ghost"
          size="icon-sm"
          disabled={pending}
          onClick={() => onAction("start")}
          aria-label={`Start ${service.name}`}
          title="Start"
        >
          <PlayIcon />
        </Button>
      )}
      {service.critical ? (
        <Tooltip>
          <TooltipTrigger asChild>
            <span
              tabIndex={0}
              className="inline-flex size-7 items-center justify-center text-muted-foreground"
              aria-label="watchdog feeder — cannot be stopped"
            >
              <LockIcon className="size-3.5" />
            </span>
          </TooltipTrigger>
          <TooltipContent>watchdog feeder — cannot be stopped</TooltipContent>
        </Tooltip>
      ) : (
        <>
          {active && (
            <Button
              variant="ghost"
              size="icon-sm"
              disabled={pending}
              onClick={() => onAction("restart")}
              aria-label={`Restart ${service.name}`}
              title="Restart"
            >
              <RotateCwIcon className={pending ? "animate-spin" : ""} />
            </Button>
          )}
          {active && (
            <Button
              variant="ghost"
              size="icon-sm"
              disabled={pending}
              onClick={onStop}
              aria-label={`Stop ${service.name}`}
              title="Stop"
              className="text-destructive"
            >
              <SquareIcon />
            </Button>
          )}
        </>
      )}
    </div>
  )
}

export default function ServicesPage() {
  const { isAdmin } = useAuth()
  const { data, error, loading, refresh } = usePolling(api.services, 3000)
  const [pending, setPending] = React.useState<Set<string>>(new Set())
  const [confirmStop, setConfirmStop] = React.useState<ServiceStatus | null>(
    null
  )

  // Toast once per failure streak, not every 3 s.
  const failure = error && !loading ? errorMessage(error) : null
  React.useEffect(() => {
    if (failure) {
      toast.error(`Service status unavailable: ${failure}`, {
        id: "services-poll",
      })
    }
  }, [failure])

  async function run(name: string, action: ServiceAction) {
    setPending((p) => new Set(p).add(name))
    try {
      await api.serviceAction(name, action)
      toast.success(`${name}: ${action} requested`)
    } catch (e) {
      toast.error(actionError(name, action, e))
      throw e
    } finally {
      setPending((p) => {
        const n = new Set(p)
        n.delete(name)
        return n
      })
      refresh()
    }
  }

  return (
    <Card className="py-0">
      <CardContent className="px-0">
        <Table>
          <TableHeader>
            <TableRow>
              <TableHead className="pl-4">Service</TableHead>
              <TableHead>State</TableHead>
              <TableHead className="hidden sm:table-cell">PID</TableHead>
              <TableHead>Uptime</TableHead>
              <TableHead className="text-right">Restarts</TableHead>
              <TableHead className="hidden md:table-cell">Last exit</TableHead>
              <TableHead className="pr-4 text-right">
                <span className="sr-only">Actions</span>
              </TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {!data &&
              loading &&
              Array.from({ length: 5 }, (_, i) => (
                <TableRow key={i}>
                  <TableCell colSpan={7} className="px-4">
                    <Skeleton className="h-5 w-full" />
                  </TableCell>
                </TableRow>
              ))}
            {data?.length === 0 && (
              <TableRow>
                <TableCell
                  colSpan={7}
                  className="py-8 text-center text-muted-foreground"
                >
                  No services configured in supervisor.toml.
                </TableCell>
              </TableRow>
            )}
            {!data && !loading && (
              <TableRow>
                <TableCell
                  colSpan={7}
                  className="py-8 text-center text-destructive"
                >
                  {errorMessage(error)}
                </TableCell>
              </TableRow>
            )}
            {data?.map((s) => (
              <TableRow key={s.name}>
                <TableCell className="pl-4 font-medium">
                  <span className="flex items-center gap-1.5">
                    {s.name}
                    {s.critical && (
                      <LockIcon
                        className="size-3 text-muted-foreground"
                        aria-label="critical"
                      />
                    )}
                    {s.log && (
                      <Link
                        to={`/logs?file=${encodeURIComponent(s.log)}`}
                        className="text-muted-foreground hover:text-foreground"
                        aria-label={`${s.name} log`}
                        title="View log"
                      >
                        <ScrollTextIcon className="size-3.5" />
                      </Link>
                    )}
                  </span>
                </TableCell>
                <TableCell>
                  <ServiceStateBadge state={s.state} />
                </TableCell>
                <TableCell className="hidden tabular-nums sm:table-cell">
                  {s.pid ?? "—"}
                </TableCell>
                <TableCell className="tabular-nums">
                  {s.state === "running" ? formatDuration(s.uptime_s) : "—"}
                </TableCell>
                <TableCell className="text-right tabular-nums">
                  {s.restarts}
                </TableCell>
                <TableCell
                  className="hidden max-w-48 truncate text-muted-foreground md:table-cell"
                  title={s.last_exit ?? undefined}
                >
                  {s.last_exit ?? "—"}
                </TableCell>
                <TableCell className="pr-4">
                  {isAdmin && (
                    <Actions
                      service={s}
                      pending={pending.has(s.name)}
                      onAction={(a) => void run(s.name, a).catch(() => {})}
                      onStop={() => setConfirmStop(s)}
                    />
                  )}
                </TableCell>
              </TableRow>
            ))}
          </TableBody>
        </Table>
      </CardContent>
      <ConfirmDialog
        open={confirmStop !== null}
        onOpenChange={(o) => !o && setConfirmStop(null)}
        title={`Stop ${confirmStop?.name ?? ""}?`}
        description="The supervisor will not restart it until you start it again (or the camera reboots)."
        confirmLabel="Stop"
        destructive
        onConfirm={() =>
          confirmStop ? run(confirmStop.name, "stop") : undefined
        }
      />
    </Card>
  )
}
