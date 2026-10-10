import * as React from "react"

import { Badge } from "@/components/ui/badge"
import {
  Card,
  CardAction,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card"
import { Skeleton } from "@/components/ui/skeleton"
import { usePolling } from "@/hooks/use-polling"
import { api, errorMessage } from "@/lib/api"
import { formatDuration } from "@/lib/format"

function Row({ label, value }: { label: string; value: React.ReactNode }) {
  return (
    <div className="flex items-center justify-between gap-4 py-1.5">
      <dt className="text-muted-foreground">{label}</dt>
      <dd className="truncate text-right font-medium">{value}</dd>
    </div>
  )
}

/** Live `wifi status` (CFG-03). */
export function WifiPanel() {
  const { data: w, error } = usePolling(api.wifi, 5000)

  return (
    <Card>
      <CardHeader>
        <CardTitle>Wi-Fi status</CardTitle>
        <CardDescription>Reported by wifi, every 5 s</CardDescription>
        {w?.healthy != null && (
          <CardAction>
            <Badge variant={w.healthy ? "secondary" : "destructive"}>
              {w.healthy ? "healthy" : "unhealthy"}
            </Badge>
          </CardAction>
        )}
      </CardHeader>
      <CardContent>
        {!w && !error && <Skeleton className="h-40 w-full" />}
        {!w && !!error && (
          <p className="text-sm text-destructive">{errorMessage(error)}</p>
        )}
        {w && (
          <dl className="divide-y text-sm">
            <Row label="Interface" value={w.iface} />
            <Row label="Connected" value={w.connected ? "yes" : "no"} />
            <Row label="Current SSID" value={w.ssid || "—"} />
            {w.target_ssid !== undefined && (
              <Row
                label="Target SSID"
                value={w.target_ssid || "(keep current)"}
              />
            )}
            <Row label="State" value={w.state ?? "—"} />
            <Row label="Address" value={w.ip || "—"} />
            <Row label="Gateway" value={w.gateway || "—"} />
            {w.signal_dbm != null && (
              <Row label="Signal" value={`${w.signal_dbm} dBm`} />
            )}
            {w.last_good_ssid !== undefined && (
              <Row label="Last good network" value={w.last_good_ssid || "—"} />
            )}
            {w.rollback_in_s != null && (
              <Row
                label="Rollback / retry in"
                value={formatDuration(w.rollback_in_s)}
              />
            )}
          </dl>
        )}
      </CardContent>
    </Card>
  )
}
