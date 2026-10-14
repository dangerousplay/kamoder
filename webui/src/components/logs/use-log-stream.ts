import * as React from "react"

import { ApiError, api, errorMessage, logStreamUrl } from "@/lib/api"

/** Lines kept in memory; older ones are dropped. */
export const LINE_CAP = 2000
const TAIL_LINES = 500
const BACKOFF_MIN_MS = 1000
const BACKOFF_MAX_MS = 30000
/** Retry delay when the server is at its stream limit (503). */
const BUSY_RETRY_MS = 15000

export type LineKind = "out" | "err" | "sup" | "plain" | "marker"

export interface LogLine {
  id: number
  kind: LineKind
  /** Local wall time, e.g. "2026-10-14 14:03:07.123 -0300", when the line
   *  was written after the clock was set. */
  wall?: string
  /** Uptime prefix (ms, or s in init.log), e.g. "12345", when the line has one. */
  stamp?: string
  text: string
}

export type StreamStatus =
  | { kind: "loading" }
  | { kind: "live" }
  | { kind: "reconnecting"; inMs: number }
  | { kind: "busy"; message: string }
  | { kind: "error"; message: string }
  | { kind: "idle" }

// Service logs:  `[2026-10-14 14:03:07.123 -0300] [  uptime_ms] out|err|sup text`
// supervisor.log: `[2026-10-14 14:03:07.123 -0300] [  uptime_ms]  INFO text`
// init.log:       `[2026-10-14 14:03:07 -0300] [123.45] init: text`
// The wall time is absent before NTP has set the clock, and in lines written
// by older builds (`[  uptime_ms] out text`); both shapes stay on the card.
const LINE_RE =
  /^(?:\[(\d{4}-\d\d-\d\d \d\d:\d\d:\d\d(?:\.\d+)? [+-]\d{4})\] )?\[\s*(\d+(?:\.\d+)?)\]\s(?:(out|err|sup)(?:\s|$))?(.*)$/

let nextId = 1

function parse(raw: string): LogLine {
  const m = LINE_RE.exec(raw)
  if (m) {
    return {
      id: nextId++,
      kind: (m[3] as LineKind | undefined) ?? "plain",
      wall: m[1],
      stamp: m[2],
      text: m[4] ?? "",
    }
  }
  return { id: nextId++, kind: "plain", text: raw }
}

function marker(text: string): LogLine {
  return { id: nextId++, kind: "marker", text }
}

/**
 * Tail + live stream of one log file. Lines are batched per animation frame
 * and capped at LINE_CAP. Reconnects with exponential backoff, fetching a
 * fresh (60 s) stream token for every attempt.
 */
export function useLogStream(file: string | null, enabled: boolean) {
  const [lines, setLines] = React.useState<LogLine[]>([])
  const [status, setStatus] = React.useState<StreamStatus>({ kind: "idle" })
  const bufRef = React.useRef<LogLine[]>([])

  const clear = React.useCallback(() => {
    bufRef.current = []
    setLines([])
  }, [])

  React.useEffect(() => {
    if (!file || !enabled) return
    let stopped = false
    let es: EventSource | null = null
    let timer: number | undefined
    let frame: number | undefined
    let attempt = 0
    let everOpened = false
    const probe = new AbortController()

    const flush = () => {
      frame = undefined
      if (!stopped) setLines(bufRef.current.slice())
    }
    const push = (items: LogLine[]) => {
      const buf = bufRef.current
      buf.push(...items)
      if (buf.length > LINE_CAP) buf.splice(0, buf.length - LINE_CAP)
      frame ??= window.requestAnimationFrame(flush)
    }

    const retry = (ms?: number) => {
      if (stopped) return
      const delay =
        ms ?? Math.min(BACKOFF_MAX_MS, BACKOFF_MIN_MS * 2 ** attempt)
      attempt++
      if (ms === undefined) setStatus({ kind: "reconnecting", inMs: delay })
      timer = window.setTimeout(connect, delay)
    }

    /** EventSource hides HTTP status; ask once with fetch to explain it. */
    async function diagnose() {
      try {
        const { token } = await api.logStreamToken()
        const res = await fetch(logStreamUrl(file!, token), {
          signal: probe.signal,
          headers: { Accept: "text/event-stream" },
        })
        if (res.ok) {
          await res.body?.cancel()
          return retry()
        }
        let message = res.statusText || `HTTP ${res.status}`
        try {
          const body = (await res.json()) as { error?: string }
          if (body.error) message = body.error
        } catch {
          // keep status text
        }
        if (res.status === 503) {
          setStatus({ kind: "busy", message })
          return retry(BUSY_RETRY_MS)
        }
        if (res.status === 404) {
          setStatus({ kind: "error", message: `${file}: ${message}` })
          return retry(BACKOFF_MAX_MS)
        }
        retry()
      } catch (e) {
        if (stopped) return
        if (e instanceof ApiError && e.isUnavailable) {
          setStatus({ kind: "busy", message: e.message })
          return retry(BUSY_RETRY_MS)
        }
        retry()
      }
    }

    async function connect() {
      if (stopped) return
      let token: string
      try {
        token = (await api.logStreamToken()).token
      } catch (e) {
        if (stopped) return
        if (e instanceof ApiError && e.isUnavailable) {
          setStatus({ kind: "busy", message: e.message })
          return retry(BUSY_RETRY_MS)
        }
        return retry()
      }
      if (stopped) return
      let opened = false
      const source = new EventSource(logStreamUrl(file!, token))
      es = source
      source.onopen = () => {
        opened = true
        attempt = 0
        if (everOpened) push([marker("stream reconnected — lines may be missing")])
        everOpened = true
        setStatus({ kind: "live" })
      }
      source.onmessage = (ev: MessageEvent<string>) => {
        push(ev.data.split("\n").map(parse))
      }
      source.onerror = () => {
        // Never let EventSource auto-retry: its token is single-use/short-lived.
        source.close()
        if (es === source) es = null
        if (stopped) return
        if (opened) retry()
        else void diagnose()
      }
    }

    async function start() {
      setStatus({ kind: "loading" })
      bufRef.current = []
      try {
        const tail = await api.logTail(file!, TAIL_LINES, probe.signal)
        if (stopped) return
        push(tail.lines.map(parse))
        flush()
      } catch (e) {
        if (stopped) return
        setLines([])
        if (e instanceof ApiError && e.isUnavailable) {
          setStatus({ kind: "busy", message: e.message })
        } else {
          setStatus({ kind: "error", message: errorMessage(e) })
          if (e instanceof ApiError && e.status === 404) return
        }
      }
      void connect()
    }

    void start()
    return () => {
      stopped = true
      probe.abort()
      es?.close()
      window.clearTimeout(timer)
      if (frame !== undefined) window.cancelAnimationFrame(frame)
      setStatus({ kind: "idle" })
    }
  }, [file, enabled])

  return { lines, status, clear }
}
