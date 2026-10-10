import * as React from "react"
import {
  CheckIcon,
  CopyIcon,
  TriangleAlertIcon,
  WifiIcon,
  WifiOffIcon,
} from "lucide-react"
import { Link } from "react-router"
import { toast } from "sonner"

import { LightsCard } from "@/components/lights/lights-card"
import { ServiceStateBadge } from "@/components/services/service-state-badge"
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import {
  Card,
  CardAction,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card"
import { Input } from "@/components/ui/input"
import { Skeleton } from "@/components/ui/skeleton"
import { SnapshotView } from "@/components/video/snapshot-view"
import { usePolling } from "@/hooks/use-polling"
import {
  api,
  errorMessage,
  type ServiceState,
  type ServiceStatus,
  type WifiStatus,
} from "@/lib/api"
import { useAuth } from "@/lib/auth"
import { copyText, isTruthy } from "@/lib/format"
import { cn } from "@/lib/utils"

const SUMMARY: { state: ServiceState; label: string; className: string }[] = [
  {
    state: "running",
    label: "Running",
    className: "text-emerald-700 dark:text-emerald-400",
  },
  { state: "starting", label: "Starting", className: "" },
  {
    state: "backoff",
    label: "Backing off",
    className: "text-amber-700 dark:text-amber-400",
  },
  { state: "failed", label: "Failed", className: "text-destructive" },
  { state: "stopped", label: "Stopped", className: "" },
  { state: "disabled", label: "Disabled", className: "text-muted-foreground" },
]

function ServicesCard({
  services,
  error,
}: {
  services?: ServiceStatus[]
  error: unknown
}) {
  const counts = React.useMemo(() => {
    const c = new Map<ServiceState, number>()
    for (const s of services ?? []) c.set(s.state, (c.get(s.state) ?? 0) + 1)
    return c
  }, [services])
  const notRunning = (services ?? []).filter((s) => s.state !== "running")

  return (
    <Card>
      <CardHeader>
        <CardTitle>Services</CardTitle>
        <CardDescription>
          {services
            ? `${counts.get("running") ?? 0} of ${services.length} running`
            : "Supervisor status"}
        </CardDescription>
        <CardAction>
          <Button asChild variant="outline" size="sm">
            <Link to="/services">Manage</Link>
          </Button>
        </CardAction>
      </CardHeader>
      <CardContent className="flex flex-col gap-4">
        {!services && !error && <Skeleton className="h-16 w-full" />}
        {!services && !!error && (
          <p className="text-sm text-destructive">{errorMessage(error)}</p>
        )}
        {services && (
          <>
            <dl className="grid grid-cols-3 gap-2 text-center">
              {SUMMARY.filter(
                (s) => s.state === "running" || counts.get(s.state)
              ).map((s) => (
                <div
                  key={s.state}
                  className="flex flex-col-reverse rounded-lg border p-2"
                >
                  <dt className="text-xs text-muted-foreground">{s.label}</dt>
                  <dd
                    className={cn(
                      "text-xl font-semibold tabular-nums",
                      s.className
                    )}
                  >
                    {counts.get(s.state) ?? 0}
                  </dd>
                </div>
              ))}
            </dl>
            {notRunning.length === 0 ? (
              <p className="text-sm text-muted-foreground">
                All services are running.
              </p>
            ) : (
              <ul className="flex flex-col divide-y text-sm">
                {notRunning.map((s) => (
                  <li
                    key={s.name}
                    className="flex items-center justify-between gap-2 py-1.5"
                  >
                    <span className="truncate font-medium">{s.name}</span>
                    <span className="flex items-center gap-2">
                      <ServiceStateBadge state={s.state} />
                      {s.log && (
                        <Link
                          to={`/logs?file=${encodeURIComponent(s.log)}`}
                          className="text-xs text-muted-foreground underline underline-offset-3 hover:text-foreground"
                        >
                          log
                        </Link>
                      )}
                    </span>
                  </li>
                ))}
              </ul>
            )}
          </>
        )}
      </CardContent>
    </Card>
  )
}

function Row({
  label,
  children,
}: {
  label: string
  children: React.ReactNode
}) {
  return (
    <div className="flex items-center justify-between gap-4 py-1.5">
      <dt className="text-muted-foreground">{label}</dt>
      <dd className="truncate text-right font-medium">{children}</dd>
    </div>
  )
}

function WifiCard({ wifi, error }: { wifi?: WifiStatus; error: unknown }) {
  return (
    <Card>
      <CardHeader>
        <CardTitle className="flex items-center gap-2">
          {wifi?.connected === false ? (
            <WifiOffIcon className="size-4" />
          ) : (
            <WifiIcon className="size-4" />
          )}
          Wi-Fi
        </CardTitle>
        <CardDescription>{wifi?.iface ?? "wlan0"}</CardDescription>
        {wifi?.healthy != null && (
          <CardAction>
            <Badge variant={wifi.healthy ? "secondary" : "destructive"}>
              {wifi.healthy ? "healthy" : "unhealthy"}
            </Badge>
          </CardAction>
        )}
      </CardHeader>
      <CardContent>
        {!wifi && !error && <Skeleton className="h-24 w-full" />}
        {!wifi && !!error && (
          <p className="text-sm text-destructive">{errorMessage(error)}</p>
        )}
        {wifi && (
          <dl className="divide-y text-sm">
            <Row label="SSID">{wifi.ssid || "—"}</Row>
            <Row label="State">
              {wifi.state ?? (wifi.connected ? "connected" : "disconnected")}
            </Row>
            <Row label="Address">{wifi.ip || "—"}</Row>
            {wifi.signal_dbm != null && (
              <Row label="Signal">{wifi.signal_dbm} dBm</Row>
            )}
          </dl>
        )}
      </CardContent>
    </Card>
  )
}

function RtspCard() {
  const url = `rtsp://${window.location.hostname}:554/vs0`
  const [copied, setCopied] = React.useState(false)

  async function copy() {
    if (await copyText(url)) {
      setCopied(true)
      window.setTimeout(() => setCopied(false), 1500)
    } else {
      toast.error("Could not copy; select the URL and copy it manually")
    }
  }

  return (
    <Card>
      <CardHeader>
        <CardTitle>RTSP stream</CardTitle>
        <CardDescription>For VLC, Frigate, Home Assistant…</CardDescription>
      </CardHeader>
      <CardContent className="flex gap-2">
        <Input
          readOnly
          value={url}
          aria-label="RTSP URL"
          className="font-mono text-xs"
          onFocus={(e) => e.currentTarget.select()}
        />
        <Button
          variant="outline"
          size="icon"
          onClick={copy}
          aria-label="Copy RTSP URL"
        >
          {copied ? <CheckIcon /> : <CopyIcon />}
        </Button>
      </CardContent>
    </Card>
  )
}

export default function DashboardPage() {
  const { isAdmin } = useAuth()
  const services = usePolling(api.services, 5000)
  const wifi = usePolling(api.wifi, 10000)
  const config = usePolling(api.config, null)

  const telnet = isTruthy(config.data?.values.start_telnetd)
  const byName = (n: string) => services.data?.find((s) => s.name === n)
  const rtspRunning = byName("rtsp")?.state === "running"
  const snapshotLog = byName("snapshot")?.log ?? "snapshot.log"

  return (
    <div className="flex flex-col gap-4">
      {telnet && (
        <Alert className="border-amber-600/40 text-amber-800 dark:text-amber-300">
          <TriangleAlertIcon />
          <AlertTitle>Unauthenticated telnet on port 23</AlertTitle>
          <AlertDescription>
            Anyone on this network can get a root shell. Once SSH works, turn
            off <code>start_telnetd</code>
            {isAdmin ? (
              <>
                {" "}
                in <Link to="/settings">Settings</Link>
              </>
            ) : null}
            .
          </AlertDescription>
        </Alert>
      )}
      <div className="grid gap-4 lg:grid-cols-2">
        <Card className="lg:row-span-2">
          <CardHeader>
            <CardTitle>Snapshot</CardTitle>
            <CardDescription>Refreshes every 5 s</CardDescription>
          </CardHeader>
          <CardContent>
            <SnapshotView logFile={snapshotLog} />
          </CardContent>
        </Card>
        <ServicesCard services={services.data} error={services.error} />
        <WifiCard wifi={wifi.data} error={wifi.error} />
        <LightsCard />
        {rtspRunning && <RtspCard />}
      </div>
    </div>
  )
}
