import * as React from "react"
import { toast } from "sonner"

import { api, errorMessage } from "@/lib/api"

export interface Direction {
  pan: number
  tilt: number
}

/**
 * The server stops a continuous move on its own after ~1 s unless it is
 * repeated (a deadman, so a closed tab or lost network cannot leave the motor
 * running). While a direction is held the move is re-sent at this interval.
 */
const KEEPALIVE_MS = 300

/**
 * Continuous-move controller: `move(dir)` starts or changes a move, `stop()`
 * ends it. Duplicate commands are dropped (the server applies "latest command
 * wins"); a stop is also sent when the window loses focus or the tab is
 * hidden, and on unmount, so the head never keeps moving unattended.
 */
export function usePtzControl(speed: number) {
  const [moving, setMoving] = React.useState(false)
  const current = React.useRef<Direction | null>(null)
  const sentSpeed = React.useRef(speed)

  const fail = React.useCallback((what: string, e: unknown) => {
    toast.error(`PTZ ${what} failed: ${errorMessage(e)}`, { id: "ptz" })
  }, [])

  const send = React.useCallback(
    (dir: Direction, s: number) => {
      sentSpeed.current = s
      api
        .ptzMove({ pan: dir.pan * s, tilt: dir.tilt * s })
        .catch((e) => fail("move", e))
    },
    [fail]
  )

  const move = React.useCallback(
    (dir: Direction) => {
      if (dir.pan === 0 && dir.tilt === 0) return
      const cur = current.current
      if (cur && cur.pan === dir.pan && cur.tilt === dir.tilt) return
      current.current = dir
      setMoving(true)
      send(dir, speed)
    },
    [send, speed]
  )

  const stop = React.useCallback(() => {
    if (!current.current) return
    current.current = null
    setMoving(false)
    api.ptzStop().catch((e) => fail("stop", e))
  }, [fail])

  // Keep the held move alive; see KEEPALIVE_MS.
  React.useEffect(() => {
    if (!moving) return
    const id = window.setInterval(() => {
      const cur = current.current
      if (cur) send(cur, sentSpeed.current)
    }, KEEPALIVE_MS)
    return () => window.clearInterval(id)
  }, [moving, send])

  // Speed changed mid-move: re-issue the move at the new speed.
  React.useEffect(() => {
    const cur = current.current
    if (cur && sentSpeed.current !== speed) send(cur, speed)
  }, [speed, send])

  React.useEffect(() => {
    const onHidden = () => {
      if (document.hidden) stop()
    }
    window.addEventListener("blur", stop)
    document.addEventListener("visibilitychange", onHidden)
    return () => {
      window.removeEventListener("blur", stop)
      document.removeEventListener("visibilitychange", onHidden)
    }
  }, [stop])

  // Unmount: stop without touching state.
  React.useEffect(
    () => () => {
      if (current.current) {
        current.current = null
        api.ptzStop().catch(() => {})
      }
    },
    []
  )

  return { moving, move, stop }
}
