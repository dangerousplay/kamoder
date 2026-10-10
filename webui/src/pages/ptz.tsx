import * as React from "react"
import { MoveIcon } from "lucide-react"

import { PtzPad } from "@/components/ptz/ptz-pad"
import { PtzPresets } from "@/components/ptz/ptz-presets"
import { usePtzControl } from "@/components/ptz/use-ptz-control"
import { Badge } from "@/components/ui/badge"
import {
  Card,
  CardAction,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card"
import { Label } from "@/components/ui/label"
import { Skeleton } from "@/components/ui/skeleton"
import { Slider } from "@/components/ui/slider"
import { SnapshotView } from "@/components/video/snapshot-view"
import { usePolling } from "@/hooks/use-polling"
import { ApiError, api, errorMessage, type PtzStatus } from "@/lib/api"
import { useAuth } from "@/lib/auth"

/** Fast position polling for this long after a goto. */
const GOTO_BOOST_MS = 10000

function Position({ status }: { status: PtzStatus }) {
  return (
    <dl className="grid grid-cols-2 gap-2 text-center" aria-live="polite">
      {(
        [
          ["Pan", status.pan],
          ["Tilt", status.tilt],
        ] as const
      ).map(([label, v]) => (
        <div
          key={label}
          className="flex flex-col-reverse rounded-lg border p-2"
        >
          <dt className="text-xs text-muted-foreground">{label}</dt>
          <dd className="text-lg font-semibold tabular-nums">{v.toFixed(1)}</dd>
        </div>
      ))}
    </dl>
  )
}

function Unavailable({ reason }: { reason?: string }) {
  return (
    <Card>
      <CardHeader>
        <CardTitle className="flex items-center gap-2">
          <MoveIcon className="size-4" />
          PTZ not available on this camera
        </CardTitle>
        <CardDescription>
          {reason ?? "No pan/tilt motor backend was found."}
        </CardDescription>
      </CardHeader>
    </Card>
  )
}

export default function PtzPage() {
  const { isAdmin } = useAuth()
  const [speed, setSpeed] = React.useState(0.5)
  const [boost, setBoost] = React.useState(false)
  const [serverMoving, setServerMoving] = React.useState(false)
  const control = usePtzControl(speed)
  // Poll position every 500 ms while anything moves (pad, goto, ONVIF).
  const fastPoll = control.moving || boost || serverMoving
  const fetchStatus = React.useCallback(async (signal: AbortSignal) => {
    const s = await api.ptz(signal)
    setServerMoving(s.moving)
    return s
  }, [])
  const { data, error, loading } = usePolling(
    fetchStatus,
    fastPoll ? 500 : 3000
  )
  const status = data

  React.useEffect(() => {
    if (!boost) return
    const id = window.setTimeout(() => setBoost(false), GOTO_BOOST_MS)
    return () => window.clearTimeout(id)
  }, [boost])

  if (loading && !data) {
    return (
      <div className="grid gap-4 md:grid-cols-2">
        <Skeleton className="aspect-video w-full" />
        <Skeleton className="h-64 w-full" />
      </div>
    )
  }

  const unavailable =
    (error instanceof ApiError && error.isUnavailable && !data) ||
    data?.available === false
  if (unavailable) {
    return (
      <div className="flex flex-col gap-4">
        <Unavailable
          reason={error instanceof ApiError ? error.message : undefined}
        />
        <SnapshotView hideWhenUnavailable className="max-w-2xl" />
      </div>
    )
  }

  if (!status) {
    return (
      <p className="text-sm text-destructive">
        PTZ status unavailable: {errorMessage(error)}
      </p>
    )
  }

  return (
    <div className="flex flex-col gap-4">
      <div className="grid items-start gap-4 md:grid-cols-[minmax(0,3fr)_minmax(0,2fr)]">
        <SnapshotView hideWhenUnavailable intervalMs={fastPoll ? 2000 : 5000} />
        <Card>
          <CardHeader>
            <CardTitle>Position</CardTitle>
            <CardDescription>
              {status.backend ? `Backend: ${status.backend}` : "Pan / tilt"}
            </CardDescription>
            <CardAction>
              <Badge variant={status.moving ? "default" : "secondary"}>
                {status.moving ? "moving" : "idle"}
              </Badge>
            </CardAction>
          </CardHeader>
          <CardContent className="flex flex-col items-center gap-6">
            <Position status={status} />
            {isAdmin && (
              <>
                <PtzPad
                  moving={control.moving}
                  onMove={control.move}
                  onStop={control.stop}
                />
                <div className="flex w-full max-w-64 flex-col gap-2">
                  <div className="flex justify-between">
                    <Label htmlFor="ptz-speed">Speed</Label>
                    <span className="text-sm text-muted-foreground tabular-nums">
                      {Math.round(speed * 100)}%
                    </span>
                  </div>
                  <Slider
                    id="ptz-speed"
                    aria-label="Speed"
                    min={0.1}
                    max={1}
                    step={0.1}
                    value={[speed]}
                    onValueChange={([v]) => v !== undefined && setSpeed(v)}
                  />
                </div>
                <p className="text-center text-xs text-muted-foreground">
                  Hold a direction to move; release to stop. Arrow keys work
                  too.
                </p>
              </>
            )}
          </CardContent>
        </Card>
      </div>
      {isAdmin && <PtzPresets isAdmin onGoto={() => setBoost(true)} />}
    </div>
  )
}
