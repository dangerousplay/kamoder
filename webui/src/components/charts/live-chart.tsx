import * as React from "react"
import { LineChart } from "echarts/charts"
import {
  GridComponent,
  LegendComponent,
  TooltipComponent,
} from "echarts/components"
import * as echarts from "echarts/core"
import { SVGRenderer } from "echarts/renderers"

import { cn } from "@/lib/utils"

// Only what these charts use, so the camera serves a small chunk.
echarts.use([
  LineChart,
  GridComponent,
  TooltipComponent,
  LegendComponent,
  SVGRenderer,
])

export interface ChartSeries {
  key: string
  label: string
  /** CSS custom property holding the color, e.g. `--series-1`. */
  colorVar: string
  values: (number | null)[]
}

interface LiveChartProps {
  series: ChartSeries[]
  /** Seconds before "now" for each point, oldest first (same length as values). */
  ages: number[]
  format: (v: number) => string
  /** Fixed top of the y axis (e.g. 100 for percent); otherwise from the data. */
  max?: number
  height?: number
  className?: string
  /** Accessible name of the figure. */
  label: string
}

/** Re-renders when the light/dark class on <html> changes. */
function useThemeClass() {
  return React.useSyncExternalStore(
    (cb) => {
      const o = new MutationObserver(cb)
      o.observe(document.documentElement, {
        attributes: true,
        attributeFilter: ["class"],
      })
      return () => o.disconnect()
    },
    () => document.documentElement.className,
    () => ""
  )
}

let probe: CanvasRenderingContext2D | null = null

/**
 * A theme token as `rgba()`: the tokens are `oklch()`, which ECharts' own
 * color parser does not read, so the browser converts them via a canvas.
 */
function cssVar(name: string) {
  const raw = getComputedStyle(document.documentElement)
    .getPropertyValue(name)
    .trim()
  probe ??= document.createElement("canvas").getContext("2d", {
    willReadFrequently: true,
  })
  if (!probe || !raw) return raw
  probe.clearRect(0, 0, 1, 1)
  probe.fillStyle = raw
  probe.fillRect(0, 0, 1, 1)
  const [r, g, b, a] = probe.getImageData(0, 0, 1, 1).data
  return `rgba(${r}, ${g}, ${b}, ${(a / 255).toFixed(3)})`
}

/**
 * Time-series line chart for the dashboard's live metrics (Apache ECharts):
 * one y axis, recessive grid, axis-crosshair tooltip, a legend when there
 * are several series, and a screen-reader table of the latest values.
 */
export function LiveChart({
  series,
  ages,
  format,
  max,
  height = 120,
  className,
  label,
}: LiveChartProps) {
  const el = React.useRef<HTMLDivElement>(null)
  const chart = React.useRef<echarts.ECharts | null>(null)
  const themeClass = useThemeClass()
  const formatRef = React.useRef(format)
  React.useEffect(() => {
    formatRef.current = format
  })

  React.useEffect(() => {
    if (!el.current) return
    const c = echarts.init(el.current, null, { renderer: "svg" })
    chart.current = c
    const ro = new ResizeObserver(() => c.resize())
    ro.observe(el.current)
    return () => {
      ro.disconnect()
      c.dispose()
      chart.current = null
    }
  }, [])

  React.useEffect(() => {
    const c = chart.current
    if (!c) return
    const muted = cssVar("--muted-foreground")
    const border = cssVar("--border")
    const popover = cssVar("--popover")
    const popoverText = cssVar("--popover-foreground")
    const fmt = (v: number) => formatRef.current(v)
    c.setOption(
      {
        animation: false,
        grid: {
          left: 56,
          right: 8,
          top: series.length > 1 ? 28 : 8,
          bottom: 22,
        },
        legend:
          series.length > 1
            ? {
                top: 0,
                left: 0,
                icon: "roundRect",
                itemWidth: 12,
                itemHeight: 3,
                textStyle: { color: muted, fontSize: 11 },
              }
            : { show: false },
        tooltip: {
          trigger: "axis",
          backgroundColor: popover,
          borderColor: border,
          textStyle: { color: popoverText, fontSize: 12 },
          axisPointer: {
            type: "line",
            lineStyle: { color: muted, type: "dashed" },
          },
          valueFormatter: (v: unknown) =>
            typeof v === "number" ? fmt(v) : "—",
        },
        xAxis: {
          type: "value",
          min: -Math.max(60, Math.ceil(ages[0] ?? 0)),
          max: 0,
          interval: 15,
          axisLabel: {
            color: muted,
            fontSize: 11,
            formatter: (v: number) => (v === 0 ? "now" : `${-v}s`),
          },
          axisLine: { lineStyle: { color: border } },
          splitLine: { show: false },
          axisPointer: { label: { show: false } },
        },
        yAxis: {
          type: "value",
          min: 0,
          max: max ?? undefined,
          splitNumber: 2,
          axisLabel: { color: muted, fontSize: 11, formatter: fmt },
          splitLine: { lineStyle: { color: border } },
        },
        series: series.map((s) => ({
          id: s.key,
          name: s.label,
          type: "line",
          showSymbol: false,
          symbolSize: 8,
          connectNulls: false,
          lineStyle: { width: 2 },
          color: cssVar(s.colorVar),
          data: s.values.map((v, i) => [-(ages[i] ?? 0), v]),
        })),
      },
      { replaceMerge: ["series"] }
    )
  }, [series, ages, max, themeClass])

  const n = ages.length
  return (
    <figure className={cn("flex flex-col", className)}>
      <div ref={el} style={{ height }} role="img" aria-label={label} />
      <table className="sr-only">
        <caption>{label}, latest value</caption>
        <tbody>
          {series.map((s) => {
            const v = s.values[n - 1]
            return (
              <tr key={s.key}>
                <th scope="row">{s.label}</th>
                <td>{v == null ? "—" : format(v)}</td>
              </tr>
            )
          })}
        </tbody>
      </table>
    </figure>
  )
}
