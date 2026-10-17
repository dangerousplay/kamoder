import * as React from "react"
import { ScanEyeIcon } from "lucide-react"
import { toast } from "sonner"

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
import { Switch } from "@/components/ui/switch"
import { usePolling } from "@/hooks/use-polling"
import {
  ApiError,
  api,
  errorMessage,
  type FollowState,
  type FollowTarget,
} from "@/lib/api"
import { useAuth } from "@/lib/auth"

type BadgeVariant = "default" | "secondary" | "destructive" | "outline"

const STATES: Record<FollowState, { label: string; variant: BadgeVariant }> = {
  idle: { label: "Idle", variant: "secondary" },
  tracking: { label: "Tracking", variant: "default" },
  settling: { label: "Settling", variant: "default" },
  returning: { label: "Returning", variant: "outline" },
  paused: { label: "Paused", variant: "outline" },
  disabled: { label: "Disabled", variant: "secondary" },
  unavailable: { label: "Unavailable", variant: "destructive" },
}

/** Where a target's centroid sits in the frame, as percentages. */
function describeTarget(t: FollowTarget): string {
  const x = (t.x_permille / 10).toFixed(0)
  const y = (t.y_permille / 10).toFixed(0)
  return `${x}% across, ${y}% down (${t.cells} cell${t.cells === 1 ? "" : "s"})`
}

/**
 * Follow movement (FOL-16): the `follow` service steers the PTZ head toward
 * motion. Viewers see what it is doing; admins switch it on or off. When the
 * service is not running (503) the card says so instead of hiding, since the
 * PTZ page only renders on cameras that have a head to steer.
 */
export function FollowCard() {
  const { isAdmin } = useAuth()
  const { data, error, loading, setData } = usePolling(api.follow, 3000)
  const [pending, setPending] = React.useState(false)

  const down = error instanceof ApiError && error.isUnavailable && !data

  async function toggle(enabled: boolean) {
    setPending(true)
    try {
      setData(await api.updateFollow(enabled))
    } catch (err) {
      toast.error(`Follow: ${errorMessage(err)}`)
    } finally {
      setPending(false)
    }
  }

  const state = data ? STATES[data.state] : undefined

  return (
    <Card>
      <CardHeader>
        <CardTitle className="flex items-center gap-2">
          <ScanEyeIcon className="size-4" />
          Follow movement
        </CardTitle>
        <CardDescription>
          {isAdmin
            ? "Turn the camera toward motion automatically."
            : "Only admins can switch following on or off."}
        </CardDescription>
        {state && (
          <CardAction>
            <Badge variant={state.variant}>{state.label}</Badge>
          </CardAction>
        )}
      </CardHeader>
      <CardContent className="flex flex-col gap-3">
        {loading && !data && <Skeleton className="h-16 w-full" />}
        {down && (
          <p className="text-sm text-muted-foreground">
            Follow service unavailable.
          </p>
        )}
        {!data && !loading && !!error && !down && (
          <p className="text-sm text-destructive">{errorMessage(error)}</p>
        )}
        {data && (
          <>
            <div className="flex items-center justify-between gap-4">
              <Label
                htmlFor="follow-enabled"
                className="font-normal text-muted-foreground"
              >
                Enabled
              </Label>
              <Switch
                id="follow-enabled"
                checked={data.enabled}
                disabled={!isAdmin || pending}
                onCheckedChange={toggle}
              />
            </div>
            <dl
              className="grid grid-cols-[auto_1fr] gap-x-4 gap-y-1 text-sm"
              aria-live="polite"
            >
              <dt className="text-muted-foreground">Target</dt>
              <dd className="text-right tabular-nums">
                {data.target ? describeTarget(data.target) : "None"}
              </dd>
              <dt className="text-muted-foreground">Moves</dt>
              <dd className="text-right tabular-nums">{data.moves}</dd>
            </dl>
            {data.state === "unavailable" && (
              <p className="text-xs text-muted-foreground">
                Enabled, but the motion grid or the PTZ service is missing.
              </p>
            )}
          </>
        )}
      </CardContent>
    </Card>
  )
}
