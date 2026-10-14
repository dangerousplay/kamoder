import * as React from "react"
import { ActivityIcon } from "lucide-react"

import { LiveChart } from "@/components/charts/live-chart"
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card"
import { Skeleton } from "@/components/ui/skeleton"
import { usePageVisible } from "@/hooks/use-page-visible"
import { api, errorMessage, type SystemStats } from "@/lib/api"
import { formatBytes } from "@/lib/format"

/** Samples kept: one a second, so a one-minute window. */
const WINDOW = 60
const INTERVAL_MS = 1000

interface Point {
  /** Camera uptime of the sample. */
  t: number
  cpu: number | null
  memUsedKb: number
  memTotalKb: number
  rx: number | null
  tx: number | null
}

/** Byte counters of every interface but loopback, summed. */
function bytes(s: SystemStats) {
  return s.interfaces.reduce(
    (a, i) => ({ rx: a.rx + i.rx_bytes, tx: a.tx + i.tx_bytes }),
    { rx: 0, tx: 0 }
  )
}

function toPoint(prev: SystemStats | null, s: SystemStats): Point {
  const memUsedKb = s.mem_total_kb - s.mem_available_kb
  const base = { t: s.uptime, memUsedKb, memTotalKb: s.mem_total_kb }
  const dt = prev ? s.uptime - prev.uptime : 0
  if (!prev || dt <= 0) return { ...base, cpu: null, rx: null, tx: null }
  const dTotal = s.cpu_total - prev.cpu_total
  const a = bytes(prev)
  const b = bytes(s)
  return {
    ...base,
    cpu:
      dTotal > 0
        ? Math.max(
            0,
            Math.min(100, ((s.cpu_busy - prev.cpu_busy) / dTotal) * 100)
          )
        : null,
    // Counters reset when an interface goes down and up again.
    rx: b.rx >= a.rx ? (b.rx - a.rx) / dt : null,
    tx: b.tx >= a.tx ? (b.tx - a.tx) / dt : null,
  }
}

/** Polls `/api/system/stats` every second while the page is visible. */
function useLiveStats() {
  const visible = usePageVisible()
  const [points, setPoints] = React.useState<Point[]>([])
  const [error, setError] = React.useState<unknown>(null)
  const last = React.useRef<SystemStats | null>(null)

  React.useEffect(() => {
    if (!visible) return
    const ctrl = new AbortController()
    let timer: number | undefined
    const tick = async () => {
      try {
        const s = await api.stats(ctrl.signal)
        const p = toPoint(last.current, s)
        last.current = s
        setError(null)
        setPoints((old) => [...old, p].slice(-WINDOW))
      } catch (e) {
        if (ctrl.signal.aborted) return
        setError(e)
      }
      if (!ctrl.signal.aborted) timer = window.setTimeout(tick, INTERVAL_MS)
    }
    void tick()
    return () => {
      ctrl.abort()
      window.clearTimeout(timer)
      // A gap while hidden must not become one huge rate sample.
      last.current = null
    }
  }, [visible])

  return { points, error }
}

const rate = (v: number) => `${formatBytes(Math.round(v))}/s`

/** Live CPU, memory and network graphs, one sample a second. */
export function SystemMetricsCard() {
  const { points, error } = useLiveStats()
  const now = points.at(-1)?.t ?? 0
  const ages = points.map((p) => now - p.t)
  const latest = points.at(-1)
  const memTotalMb = (latest?.memTotalKb ?? 0) / 1024

  return (
    <Card>
      <CardHeader>
        <CardTitle className="flex items-center gap-2">
          <ActivityIcon className="size-4" />
          System
        </CardTitle>
        <CardDescription>
          Live, updated every second (last minute)
        </CardDescription>
      </CardHeader>
      <CardContent className="flex flex-col gap-5">
        {points.length === 0 && !error && <Skeleton className="h-48 w-full" />}
        {!!error && points.length === 0 && (
          <p className="text-sm text-destructive">{errorMessage(error)}</p>
        )}
        {points.length > 0 && (
          <>
            <section className="flex flex-col gap-1">
              <div className="flex items-baseline justify-between text-sm">
                <h3 className="font-medium">CPU</h3>
                <span className="text-muted-foreground tabular-nums">
                  {latest?.cpu == null ? "—" : `${latest.cpu.toFixed(0)} %`}
                </span>
              </div>
              <LiveChart
                label="CPU usage, percent"
                ages={ages}
                max={100}
                format={(v) => `${v.toFixed(0)}%`}
                series={[
                  {
                    key: "cpu",
                    label: "CPU",
                    colorVar: "--series-1",
                    values: points.map((p) => p.cpu),
                  },
                ]}
              />
            </section>
            <section className="flex flex-col gap-1">
              <div className="flex items-baseline justify-between text-sm">
                <h3 className="font-medium">Memory used</h3>
                <span className="text-muted-foreground tabular-nums">
                  {latest
                    ? `${(latest.memUsedKb / 1024).toFixed(1)} of ${memTotalMb.toFixed(0)} MiB`
                    : "—"}
                </span>
              </div>
              <LiveChart
                label="Memory used, MiB"
                ages={ages}
                max={memTotalMb || undefined}
                format={(v) => `${v.toFixed(0)} MiB`}
                series={[
                  {
                    key: "mem",
                    label: "Memory used",
                    colorVar: "--series-1",
                    values: points.map((p) => p.memUsedKb / 1024),
                  },
                ]}
              />
            </section>
            <section className="flex flex-col gap-1">
              <div className="flex items-baseline justify-between text-sm">
                <h3 className="font-medium">Network</h3>
                <span className="text-muted-foreground tabular-nums">
                  ↓ {latest?.rx == null ? "—" : rate(latest.rx)} · ↑{" "}
                  {latest?.tx == null ? "—" : rate(latest.tx)}
                </span>
              </div>
              <LiveChart
                label="Network throughput, bytes per second"
                ages={ages}
                format={rate}
                series={[
                  {
                    key: "rx",
                    label: "Received",
                    colorVar: "--series-1",
                    values: points.map((p) => p.rx),
                  },
                  {
                    key: "tx",
                    label: "Sent",
                    colorVar: "--series-2",
                    values: points.map((p) => p.tx),
                  },
                ]}
              />
            </section>
          </>
        )}
      </CardContent>
    </Card>
  )
}
