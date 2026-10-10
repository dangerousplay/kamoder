import * as React from "react"

export interface Polling<T> {
  data: T | undefined
  /** Last error; cleared by the next successful call. */
  error: unknown
  /** True until the first call settles. */
  loading: boolean
  /** Runs the call now (and restarts the interval). */
  refresh: () => void
  /** Local update (e.g. optimistic) until the next poll replaces it. */
  setData: React.Dispatch<React.SetStateAction<T | undefined>>
}

function isAbort(e: unknown) {
  return e instanceof DOMException && e.name === "AbortError"
}

/**
 * Calls `fn` now and then every `ms` milliseconds (after the previous call
 * settles, so slow responses never pile up on the camera). `ms = null` fetches
 * once. Paused while the tab is hidden; resumes with an immediate call.
 */
export function usePolling<T>(
  fn: (signal: AbortSignal) => Promise<T>,
  ms: number | null
): Polling<T> {
  const [data, setData] = React.useState<T | undefined>(undefined)
  const [error, setError] = React.useState<unknown>(undefined)
  const [loading, setLoading] = React.useState(true)
  const [nonce, setNonce] = React.useState(0)
  const call = React.useEffectEvent((signal: AbortSignal) => fn(signal))

  React.useEffect(() => {
    const ctrl = new AbortController()
    let timer: number | undefined
    let busy = false

    const schedule = () => {
      window.clearTimeout(timer)
      if (ms !== null && !ctrl.signal.aborted) {
        timer = window.setTimeout(tick, ms)
      }
    }

    async function tick() {
      if (busy || ctrl.signal.aborted) return
      if (document.hidden && ms !== null) return // resumed by visibilitychange
      busy = true
      try {
        const value = await call(ctrl.signal)
        if (ctrl.signal.aborted) return
        setData(value)
        setError(undefined)
      } catch (e) {
        if (isAbort(e) || ctrl.signal.aborted) return
        setError(e)
      } finally {
        busy = false
        if (!ctrl.signal.aborted) {
          setLoading(false)
          schedule()
        }
      }
    }

    const onVisibility = () => {
      if (!document.hidden) {
        window.clearTimeout(timer)
        void tick()
      }
    }

    void tick()
    document.addEventListener("visibilitychange", onVisibility)
    return () => {
      ctrl.abort()
      window.clearTimeout(timer)
      document.removeEventListener("visibilitychange", onVisibility)
    }
  }, [ms, nonce])

  const refresh = React.useCallback(() => setNonce((n) => n + 1), [])

  return { data, error, loading, refresh, setData }
}
