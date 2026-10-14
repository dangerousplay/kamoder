import * as React from "react"
import { RadioIcon } from "lucide-react"

import { Label } from "@/components/ui/label"
import { Switch } from "@/components/ui/switch"
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs"
import { SnapshotView } from "@/components/video/snapshot-view"
import { api, liveMjpegUrl, type Camera } from "@/lib/api"

interface CameraViewProps {
  /** Sensors from `/api/hardware`; one tab each when there are several. */
  cameras?: Camera[]
  logFile?: string
}

/**
 * Preview of one image sensor: a snapshot refreshed every few seconds, or,
 * with "Live" on, the MJPEG stream (`/api/live.mjpeg`, one viewer at a time).
 * `<img>` cannot send the bearer header, so live view uses a short-lived
 * stream token in the URL.
 */
export function CameraView({ cameras, logFile }: CameraViewProps) {
  const [camera, setCamera] = React.useState(0)
  const [live, setLive] = React.useState(false)
  const multi = (cameras?.length ?? 0) > 1
  const selected = multi ? camera : undefined
  // The stream for one (live, camera) choice; results for an older choice
  // are ignored rather than cleared.
  const streamKey = live ? String(selected ?? -1) : null
  const [stream, setStream] = React.useState<{
    key: string
    url?: string
    failed?: boolean
  } | null>(null)
  const current = stream && stream.key === streamKey ? stream : null
  const liveUrl = current?.url ?? null
  const liveFailed = current?.failed ?? false

  React.useEffect(() => {
    if (streamKey == null) return
    let cancelled = false
    api
      .logStreamToken()
      .then(({ token }) => {
        if (!cancelled)
          setStream({ key: streamKey, url: liveMjpegUrl(token, selected) })
      })
      .catch(() => {
        if (!cancelled) setStream({ key: streamKey, failed: true })
      })
    return () => {
      cancelled = true
    }
  }, [streamKey, selected])

  return (
    <div className="flex flex-col gap-3">
      <div className="flex flex-wrap items-center justify-between gap-2">
        {multi ? (
          <Tabs
            value={String(camera)}
            onValueChange={(v) => setCamera(Number(v))}
          >
            <TabsList>
              {cameras!.map((c) => (
                <TabsTrigger key={c.index} value={String(c.index)}>
                  Camera {c.index + 1}
                </TabsTrigger>
              ))}
            </TabsList>
          </Tabs>
        ) : (
          <span />
        )}
        <div className="flex items-center gap-2">
          <Switch id="live-view" checked={live} onCheckedChange={setLive} />
          <Label htmlFor="live-view" className="flex items-center gap-1">
            <RadioIcon className="size-3.5" />
            Live
          </Label>
        </div>
      </div>
      {live && liveUrl && !liveFailed ? (
        <div className="relative aspect-video w-full overflow-hidden rounded-lg bg-muted">
          <img
            key={liveUrl}
            src={liveUrl}
            alt={`Camera ${camera + 1} live view`}
            className="size-full object-contain"
            onError={() =>
              streamKey && setStream({ key: streamKey, failed: true })
            }
          />
          <span className="absolute top-2 left-2 rounded bg-red-600/90 px-1.5 py-0.5 text-[0.7rem] font-semibold text-white">
            LIVE
          </span>
        </div>
      ) : (
        <>
          {live && liveFailed && (
            <p className="text-xs text-muted-foreground">
              Live view unavailable (another viewer may have it open); showing
              snapshots instead.
            </p>
          )}
          <SnapshotView
            key={selected ?? -1}
            camera={selected}
            logFile={logFile}
          />
        </>
      )}
    </div>
  )
}
