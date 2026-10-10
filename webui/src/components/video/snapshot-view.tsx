import * as React from "react"
import { VideoOffIcon } from "lucide-react"
import { Link } from "react-router"

import { Skeleton } from "@/components/ui/skeleton"
import { ApiError, api, errorMessage } from "@/lib/api"
import { cn } from "@/lib/utils"

interface SnapshotViewProps {
  /** Log file of the snapshot service, linked when video is unavailable. */
  logFile?: string
  intervalMs?: number
  /** Render nothing (instead of the "unavailable" panel) on 503. */
  hideWhenUnavailable?: boolean
  className?: string
}

type State =
  | { kind: "loading" }
  | { kind: "ok"; url: string; at: Date }
  | { kind: "unavailable"; reason: string }
  | { kind: "error"; reason: string }

/**
 * Fetches `/api/snapshot.jpg` with the bearer token every `intervalMs` while
 * the element is on screen and the tab is visible (the camera encodes each
 * JPEG on demand, so nothing is fetched when nobody looks).
 */
export function SnapshotView({
  logFile = "snapshot.log",
  intervalMs = 5000,
  hideWhenUnavailable = false,
  className,
}: SnapshotViewProps) {
  const ref = React.useRef<HTMLDivElement>(null)
  const [state, setState] = React.useState<State>({ kind: "loading" })

  React.useEffect(() => {
    const el = ref.current
    const ctrl = new AbortController()
    let timer: number | undefined
    let busy = false
    let onScreen = true
    let objectUrl: string | null = null

    const visible = () => onScreen && !document.hidden

    async function tick() {
      window.clearTimeout(timer)
      if (busy || ctrl.signal.aborted || !visible()) return
      busy = true
      let next = intervalMs
      try {
        const blob = await api.snapshot(ctrl.signal)
        const url = URL.createObjectURL(blob)
        if (objectUrl) URL.revokeObjectURL(objectUrl)
        objectUrl = url
        setState({ kind: "ok", url, at: new Date() })
      } catch (e) {
        if (ctrl.signal.aborted) return
        // Video down: check again, but less often.
        next = intervalMs * 3
        if (e instanceof ApiError && e.isUnavailable) {
          setState({ kind: "unavailable", reason: e.message })
        } else {
          setState({ kind: "error", reason: errorMessage(e) })
        }
      } finally {
        busy = false
        if (!ctrl.signal.aborted) timer = window.setTimeout(tick, next)
      }
    }

    // A hidden (display:none) element never intersects, so the observer is
    // only used when the view stays rendered while video is down.
    const io =
      el && !hideWhenUnavailable && "IntersectionObserver" in window
        ? new IntersectionObserver(([entry]) => {
            onScreen = entry?.isIntersecting ?? true
            if (onScreen) void tick()
          })
        : null
    if (el && io) io.observe(el)
    const onVisibility = () => {
      if (!document.hidden) void tick()
    }
    document.addEventListener("visibilitychange", onVisibility)
    void tick()

    return () => {
      ctrl.abort()
      window.clearTimeout(timer)
      io?.disconnect()
      document.removeEventListener("visibilitychange", onVisibility)
      if (objectUrl) URL.revokeObjectURL(objectUrl)
    }
  }, [intervalMs, hideWhenUnavailable])

  const hidden =
    hideWhenUnavailable &&
    (state.kind === "unavailable" || state.kind === "error")

  return (
    <div
      ref={ref}
      className={cn(
        "relative aspect-video w-full overflow-hidden rounded-lg bg-muted",
        hidden && "hidden",
        className
      )}
    >
      {state.kind === "loading" && <Skeleton className="size-full" />}
      {state.kind === "ok" && (
        <>
          <img
            src={state.url}
            alt="Camera snapshot"
            className="size-full object-contain"
          />
          <span className="absolute right-2 bottom-2 rounded bg-black/60 px-1.5 py-0.5 text-[0.7rem] text-white tabular-nums">
            {state.at.toLocaleTimeString()}
          </span>
        </>
      )}
      {(state.kind === "unavailable" || state.kind === "error") && (
        <div
          className="flex size-full flex-col items-center justify-center gap-1 p-4 text-center"
          role="status"
        >
          <VideoOffIcon className="mb-1 size-6 text-muted-foreground" />
          <p className="text-sm font-medium">
            {state.kind === "unavailable"
              ? "Video unavailable"
              : "Snapshot failed"}
          </p>
          <p className="max-w-sm text-xs text-muted-foreground">
            {state.reason}
          </p>
          <Link
            to={`/logs?file=${encodeURIComponent(logFile)}`}
            className="text-xs underline underline-offset-3 hover:text-foreground"
          >
            View snapshot service log
          </Link>
        </div>
      )}
    </div>
  )
}
