import * as React from "react"
import { toast } from "sonner"

import { errorMessage } from "@/lib/api"
import { Listener } from "@/lib/audio/listen"
import { listMics, savedMic, type Mic } from "@/lib/audio/mics"
import { Talker, type TalkState } from "@/lib/audio/talk"

export type ListenState = "off" | "connecting" | "playing"

/**
 * Live camera microphone. `start` must run in a click handler (the audio
 * context needs a user gesture). Stops on `stop`, when the tab is hidden and
 * on unmount; server-side ends are toasted.
 */
export function useListen() {
  const [state, setState] = React.useState<ListenState>("off")
  const ref = React.useRef<Listener | null>(null)

  const stop = React.useCallback(() => {
    ref.current?.stop()
    ref.current = null
    setState("off")
  }, [])

  const start = React.useCallback(() => {
    if (ref.current) return
    setState("connecting")
    try {
      const listener: Listener = new Listener({
        onPlaying: () => {
          if (ref.current === listener) setState("playing")
        },
        onEnd: (message) => {
          if (ref.current !== listener) return
          ref.current = null
          setState("off")
          if (message) toast.error(message)
        },
      })
      ref.current = listener
    } catch (e) {
      setState("off")
      toast.error(`Listen: ${errorMessage(e)}`)
    }
  }, [])

  React.useEffect(() => {
    const onVisibility = () => {
      if (document.hidden) stop()
    }
    document.addEventListener("visibilitychange", onVisibility)
    return () => {
      document.removeEventListener("visibilitychange", onVisibility)
      ref.current?.stop()
      ref.current = null
    }
  }, [stop])

  return { state, start, stop }
}

/** RMS 0..1 → meter fill 0..1 over a -50..0 dBFS scale. */
function meterFill(level: number): number {
  const db = 20 * Math.log10(Math.max(level, 1e-5))
  return Math.min(1, Math.max(0, (db + 50) / 50))
}

/**
 * Push-to-talk. `press` must run in the pointer/key handler (user gesture).
 * The level of the outgoing audio is written straight to `meter`'s
 * `transform` (50 updates/s without re-rendering). Released when the tab is
 * hidden; the microphone is freed on unmount.
 */
export function useTalk(meter: React.RefObject<HTMLElement | null>) {
  const [state, setState] = React.useState<TalkState>("idle")
  const [error, setError] = React.useState<string | null>(null)
  const [testing, setTesting] = React.useState(false)
  const [mics, setMics] = React.useState<Mic[]>([])
  const [device, setDeviceState] = React.useState<string | null>(savedMic)
  const ref = React.useRef<Talker | null>(null)

  React.useEffect(() => {
    const refreshMics = () => {
      listMics()
        .then(setMics)
        .catch(() => setMics([]))
    }
    const talker = new Talker({
      onMicOpened: () => {
        // A vanished saved microphone falls back to the default.
        setDeviceState(talker.device())
        refreshMics()
      },
      onTesting: setTesting,
      onState: setState,
      onLevel: (level) => {
        if (meter.current)
          meter.current.style.transform = `scaleX(${meterFill(level)})`
      },
      onError: (message) => {
        setError(message)
        toast.error(message)
      },
    })
    ref.current = talker
    refreshMics()
    navigator.mediaDevices?.addEventListener("devicechange", refreshMics)
    const onVisibility = () => {
      if (document.hidden) {
        talker.release()
        talker.stopTest()
      }
    }
    document.addEventListener("visibilitychange", onVisibility)
    return () => {
      navigator.mediaDevices?.removeEventListener("devicechange", refreshMics)
      document.removeEventListener("visibilitychange", onVisibility)
      talker.dispose()
      ref.current = null
    }
  }, [meter])

  const press = React.useCallback(() => {
    setError(null)
    ref.current?.press()
  }, [])
  const release = React.useCallback(() => ref.current?.release(), [])
  const toggleTest = React.useCallback(() => {
    const talker = ref.current
    if (!talker) return
    setError(null)
    if (testing) talker.stopTest()
    else talker.startTest()
  }, [testing])
  const setDevice = React.useCallback((deviceId: string | null) => {
    ref.current?.setDevice(deviceId)
    setDeviceState(deviceId)
  }, [])

  return {
    state,
    error,
    press,
    release,
    testing,
    toggleTest,
    mics,
    device,
    setDevice,
  }
}
