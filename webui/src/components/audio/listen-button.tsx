import * as React from "react"
import { HeadphonesIcon } from "lucide-react"

import { useListen } from "@/components/audio/use-audio"
import { Button } from "@/components/ui/button"
import { api } from "@/lib/api"

/**
 * Toggles live listening to the camera microphone (`/api/audio/listen`).
 * Used on the audio card and next to the video views.
 */
export function ListenButton({
  size,
}: {
  size?: React.ComponentProps<typeof Button>["size"]
}) {
  const { state, start, stop } = useListen()
  const on = state !== "off"
  return (
    <Button
      variant={on ? "default" : "outline"}
      size={size}
      aria-pressed={on}
      onClick={() => (on ? stop() : start())}
    >
      <HeadphonesIcon />
      {state === "connecting"
        ? "Connecting…"
        : state === "playing"
          ? "Listening"
          : "Listen"}
    </Button>
  )
}

/**
 * `ListenButton` for the video views: shown only when the audio service is
 * up and the board has a microphone (checked once on mount).
 */
export function CameraListenButton({
  size = "sm",
}: {
  size?: React.ComponentProps<typeof Button>["size"]
}) {
  const [hasMic, setHasMic] = React.useState(false)
  React.useEffect(() => {
    let cancelled = false
    api
      .audioDevices()
      .then((d) => {
        if (!cancelled) setHasMic(d.devices.some((x) => x.kind === "input"))
      })
      .catch(() => {})
    return () => {
      cancelled = true
    }
  }, [])
  return hasMic ? <ListenButton size={size} /> : null
}
