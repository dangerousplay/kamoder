import * as React from "react"
import { CheckIcon, CopyIcon } from "lucide-react"
import { toast } from "sonner"

import { CameraListenButton } from "@/components/audio/listen-button"
import { SnapshotView } from "@/components/video/snapshot-view"
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
import { usePolling } from "@/hooks/use-polling"
import { api, cameraRtspUrl, errorMessage, type CameraDetails } from "@/lib/api"
import { copyText } from "@/lib/format"

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
      <dd className="truncate text-right font-medium tabular-nums">
        {children}
      </dd>
    </div>
  )
}

function stateBadge(state: string) {
  // Expected by the board profile but not found: muted, not an error colour.
  if (state === "missing")
    return (
      <Badge variant="outline" className="text-muted-foreground">
        missing
      </Badge>
    )
  const variant =
    state === "running"
      ? "secondary"
      : state === "starting"
        ? "outline"
        : "destructive"
  return <Badge variant={variant}>{state}</Badge>
}

function RtspUrl({ cam }: { cam: CameraDetails }) {
  const url = cameraRtspUrl(cam)
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
    <div className="flex gap-2">
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
    </div>
  )
}

function CameraCard({ cam }: { cam: CameraDetails }) {
  const l = cam.live
  return (
    <Card>
      <CardHeader>
        <CardTitle>Camera {cam.index + 1}</CardTitle>
        <CardDescription>
          <span className="font-mono">{cam.sensor}</span>
          {cam.bus ? ` · i2c ${cam.bus}` : ""}
        </CardDescription>
        <CardAction>{stateBadge(l.state)}</CardAction>
      </CardHeader>
      <CardContent className="flex flex-col gap-4">
        <SnapshotView
          camera={cam.index}
          intervalMs={3000}
          logFile="snapshot.log"
        />
        <dl className="divide-y text-sm">
          <Row label="Frame rate">
            {l.state === "running" ? `${l.fps.toFixed(1)} fps` : "—"}
          </Row>
          <Row label="Sensor resolution">
            {l.sensor_width ? `${l.sensor_width}×${l.sensor_height}` : "—"}
          </Row>
          <Row label="Capture">
            {l.width ? `${l.width}×${l.height} ${l.format}` : "—"}
          </Row>
          <Row label="Video encoding">
            {l.stream
              ? `${l.stream.codec} ${l.stream.profile} ${l.stream.width}×${l.stream.height} @ ${l.stream.fps} fps`
              : "—"}
          </Row>
          <Row label="Bitrate">
            {l.stream
              ? `${Math.round(l.stream.kbps)} kbit/s (target ${l.stream.target_kbps}, GOP ${l.stream.gop})`
              : "—"}
          </Row>
          <Row label="Encoder">{l.stream?.encoder ?? "—"}</Row>
          <Row label="Snapshots">{l.encoding || "—"}</Row>
          <Row label="Frames captured">{l.frames.toLocaleString()}</Row>
          <Row label="Sensor id">{l.sensor_id ?? "—"}</Row>
          <Row label="ISP tuning">
            <span className="font-mono text-xs">{l.tuning ?? "—"}</span>
          </Row>
          <Row label="Capture channels">{cam.channels}</Row>
        </dl>
        {l.error && <p className="text-sm text-destructive">{l.error}</p>}
        <div className="flex flex-col gap-1.5">
          <span className="text-sm font-medium">RTSP stream</span>
          <RtspUrl cam={cam} />
        </div>
      </CardContent>
    </Card>
  )
}

/** Each image sensor: live preview, measured frame rate, formats, RTSP URL. */
export default function CamerasPage() {
  const cameras = usePolling(api.cameras, 1000)

  if (!cameras.data) {
    return cameras.error ? (
      <p className="text-sm text-destructive">{errorMessage(cameras.error)}</p>
    ) : (
      <div className="grid gap-4 lg:grid-cols-2">
        <Skeleton className="h-96 w-full" />
        <Skeleton className="h-96 w-full" />
      </div>
    )
  }
  return (
    <div className="flex flex-col gap-4">
      {/* One microphone serves every sensor, so one Listen for the page. */}
      <div className="flex justify-end">
        <CameraListenButton size="default" />
      </div>
      <div className="grid items-start gap-4 lg:grid-cols-2">
        {cameras.data.map((cam) => (
          <CameraCard key={cam.index} cam={cam} />
        ))}
        {cameras.data.length === 0 && (
          <p className="text-sm text-muted-foreground">
            No image sensor found.
          </p>
        )}
      </div>
    </div>
  )
}
