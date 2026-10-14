import { CameraIcon, CpuIcon } from "lucide-react"

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
import { errorMessage, type Hardware } from "@/lib/api"
import { formatBytes, formatDuration } from "@/lib/format"

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

/** Board profile, SoC, memory, flash and the image sensors (`/api/hardware`). */
export function HardwareCard({ hw, error }: { hw?: Hardware; error: unknown }) {
  const flash = hw?.flash.reduce((sum, p) => sum + p.size_bytes, 0) ?? 0
  return (
    <Card>
      <CardHeader>
        <CardTitle className="flex items-center gap-2">
          <CpuIcon className="size-4" />
          Hardware
        </CardTitle>
        <CardDescription>{hw?.model ?? "Camera board"}</CardDescription>
        {hw && (
          <CardAction>
            <Badge variant="secondary" className="gap-1">
              <CameraIcon className="size-3" />
              {hw.camera_count} {hw.camera_count === 1 ? "camera" : "cameras"}
            </Badge>
          </CardAction>
        )}
      </CardHeader>
      <CardContent>
        {!hw && !error && <Skeleton className="h-32 w-full" />}
        {!hw && !!error && (
          <p className="text-sm text-destructive">{errorMessage(error)}</p>
        )}
        {hw && (
          <dl className="divide-y text-sm">
            <Row label="Board">
              {hw.board ? (
                <span title={hw.board.reason}>
                  <span className="font-mono">{hw.board.name}</span>
                  {hw.board.description && (
                    <span className="text-muted-foreground">
                      {" "}
                      · {hw.board.description}
                    </span>
                  )}
                </span>
              ) : (
                <span className="text-muted-foreground">not resolved</span>
              )}
            </Row>
            {hw.board?.reason && (
              <Row label="Chosen by">
                <span
                  title={hw.board.reason}
                  className="text-xs font-normal text-muted-foreground"
                >
                  {hw.board.reason}
                </span>
              </Row>
            )}
            <Row label="SoC">{hw.soc ?? "—"}</Row>
            <Row label="CPU">
              {hw.cpu ?? "—"}
              {hw.bogomips != null && (
                <span className="text-muted-foreground">
                  {" "}
                  · {hw.bogomips.toFixed(0)} BogoMIPS
                </span>
              )}
            </Row>
            <Row label="Memory">
              {hw.memory_total_kb != null
                ? `${formatBytes((hw.memory_available_kb ?? 0) * 1024)} free of ${formatBytes(hw.memory_total_kb * 1024)}`
                : "—"}
            </Row>
            {flash > 0 && <Row label="Flash">{formatBytes(flash)}</Row>}
            <Row label="Kernel">{hw.kernel ?? "—"}</Row>
            <Row label="Uptime">{formatDuration(hw.uptime_s)}</Row>
            {hw.cameras.map((c) => (
              <Row key={c.index} label={`Camera ${c.index}`}>
                <span className="font-mono">{c.sensor}</span>
                <span className="text-muted-foreground">
                  {c.bus ? ` · i2c ${c.bus}` : ""} · {c.channels}{" "}
                  {c.channels === 1 ? "channel" : "channels"}
                </span>
              </Row>
            ))}
          </dl>
        )}
      </CardContent>
    </Card>
  )
}
