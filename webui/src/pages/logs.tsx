import * as React from "react"
import {
  ArrowDownToLineIcon,
  EraserIcon,
  PauseIcon,
  PlayIcon,
  SearchIcon,
} from "lucide-react"
import { useSearchParams } from "react-router"

import {
  LINE_CAP,
  useLogStream,
  type LogLine,
  type StreamStatus,
} from "@/components/logs/use-log-stream"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Card } from "@/components/ui/card"
import { Input } from "@/components/ui/input"
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select"
import { Skeleton } from "@/components/ui/skeleton"
import { usePageVisible } from "@/hooks/use-page-visible"
import { usePolling } from "@/hooks/use-polling"
import { api, errorMessage } from "@/lib/api"
import { formatBytes } from "@/lib/format"
import { cn } from "@/lib/utils"

const KIND_CLASS: Record<LogLine["kind"], string> = {
  out: "",
  plain: "",
  err: "text-red-600 dark:text-red-400",
  sup: "text-sky-700 dark:text-sky-400",
  marker: "text-muted-foreground italic",
}

function StatusBadge({ status }: { status: StreamStatus }) {
  switch (status.kind) {
    case "live":
      return (
        <Badge
          variant="outline"
          className="border-emerald-600/30 text-emerald-700 dark:text-emerald-400"
        >
          <span className="size-1.5 animate-pulse rounded-full bg-current" />
          live
        </Badge>
      )
    case "loading":
      return <Badge variant="secondary">loading</Badge>
    case "reconnecting":
      return (
        <Badge variant="outline" className="text-amber-700 dark:text-amber-400">
          reconnecting in {Math.round(status.inMs / 1000)} s
        </Badge>
      )
    case "busy":
    case "error":
      return <Badge variant="destructive">offline</Badge>
    default:
      return <Badge variant="secondary">paused</Badge>
  }
}

const Line = React.memo(function Line({ line }: { line: LogLine }) {
  return (
    <div
      className={cn(
        "px-3 break-all whitespace-pre-wrap [contain-intrinsic-size:auto_1.25rem] [content-visibility:auto]",
        KIND_CLASS[line.kind]
      )}
    >
      {line.stamp !== undefined && (
        <span className="text-muted-foreground select-none">
          [{line.stamp.padStart(9)}]{" "}
        </span>
      )}
      {line.kind === "marker" ? `— ${line.text} —` : line.text}
    </div>
  )
})

export default function LogsPage() {
  const [params, setParams] = useSearchParams()
  const files = usePolling(api.logs, 30000)
  const visible = usePageVisible()

  const list = files.data
  const file =
    params.get("file") ??
    list?.find((f) => f.name === "init.log")?.name ??
    list?.[0]?.name ??
    null

  const { lines, status, clear } = useLogStream(file, visible)
  const [follow, setFollow] = React.useState(true)
  const [frozen, setFrozen] = React.useState<LogLine[]>([])
  const [filter, setFilter] = React.useState("")
  const deferredFilter = React.useDeferredValue(filter.trim().toLowerCase())
  const scrollRef = React.useRef<HTMLDivElement>(null)

  const source = follow ? lines : frozen
  const shown = React.useMemo(
    () =>
      deferredFilter
        ? source.filter((l) => l.text.toLowerCase().includes(deferredFilter))
        : source,
    [source, deferredFilter]
  )
  const lastFrozen = frozen.at(-1)?.id ?? 0
  const newWhilePaused = follow
    ? 0
    : lines.filter((l) => l.id > lastFrozen).length

  // Set while our own scrollTop write is pending, so its scroll event is not
  // mistaken for the user scrolling up (scroll events fire before rAF).
  const autoScroll = React.useRef(false)

  // Keep the newest line in view while following.
  React.useLayoutEffect(() => {
    const el = scrollRef.current
    if (!follow || !el) return
    autoScroll.current = true
    el.scrollTop = el.scrollHeight
    const id = window.requestAnimationFrame(() => {
      autoScroll.current = false
    })
    return () => window.cancelAnimationFrame(id)
  }, [shown, follow])

  function pause() {
    setFrozen(lines)
    setFollow(false)
  }

  function onScroll(e: React.UIEvent<HTMLDivElement>) {
    if (autoScroll.current) return
    const el = e.currentTarget
    const fromBottom = el.scrollHeight - el.scrollTop - el.clientHeight
    // Scrolling up while following freezes the view so it stops jumping.
    if (follow && fromBottom > 48) pause()
  }

  function selectFile(name: string) {
    setParams({ file: name }, { replace: true })
    setFollow(true)
  }

  function onClear() {
    clear()
    setFrozen([])
  }

  return (
    <div className="flex min-h-0 flex-1 flex-col gap-3">
      <div className="flex flex-wrap items-center gap-2">
        {files.loading && !list ? (
          <Skeleton className="h-8 w-48" />
        ) : (
          <Select value={file ?? ""} onValueChange={selectFile}>
            <SelectTrigger className="w-full sm:w-64" aria-label="Log file">
              <SelectValue placeholder="Select a log file" />
            </SelectTrigger>
            <SelectContent>
              {file && !list?.some((f) => f.name === file) && (
                <SelectItem value={file}>{file}</SelectItem>
              )}
              {list?.map((f) => (
                <SelectItem key={f.name} value={f.name}>
                  <span className="flex w-full justify-between gap-4">
                    <span>{f.name}</span>
                    <span className="text-muted-foreground tabular-nums">
                      {formatBytes(f.size)}
                    </span>
                  </span>
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
        )}
        <div className="relative min-w-40 flex-1">
          <SearchIcon className="pointer-events-none absolute top-1/2 left-2.5 size-3.5 -translate-y-1/2 text-muted-foreground" />
          <Input
            value={filter}
            onChange={(e) => setFilter(e.target.value)}
            placeholder="Filter lines"
            aria-label="Filter lines"
            className="pl-8"
          />
        </div>
        <div className="flex items-center gap-1">
          <StatusBadge status={status} />
          <Button
            variant={follow ? "outline" : "default"}
            size="sm"
            onClick={() => (follow ? pause() : setFollow(true))}
            aria-pressed={!follow}
          >
            {follow ? <PauseIcon /> : <PlayIcon />}
            {follow ? "Pause" : "Follow"}
          </Button>
          <Button
            variant="ghost"
            size="icon-sm"
            onClick={onClear}
            aria-label="Clear view"
            title="Clear view"
          >
            <EraserIcon />
          </Button>
        </div>
      </div>

      {files.error && !list ? (
        <p className="text-sm text-destructive">
          Could not list log files: {errorMessage(files.error)}
        </p>
      ) : null}
      {(status.kind === "busy" || status.kind === "error") && (
        <p className="text-sm text-destructive" role="status">
          {status.kind === "busy"
            ? `Live streaming unavailable: ${status.message} (the camera allows 4 log streams at once; close other log tabs). Retrying…`
            : status.message}
        </p>
      )}

      <Card className="relative min-h-80 flex-1 gap-0 overflow-hidden py-0">
        <div
          ref={scrollRef}
          onScroll={onScroll}
          role="log"
          aria-live="off"
          aria-label={file ? `Contents of ${file}` : "Log contents"}
          className="absolute inset-0 overflow-auto py-2 font-mono text-xs leading-5"
        >
          {status.kind === "loading" && lines.length === 0 ? (
            <div className="flex flex-col gap-1.5 px-3">
              {Array.from({ length: 8 }, (_, i) => (
                <Skeleton key={i} className="h-3.5 w-full" />
              ))}
            </div>
          ) : shown.length === 0 ? (
            <p className="px-3 py-6 text-center font-sans text-sm text-muted-foreground">
              {!file
                ? "No log files yet."
                : deferredFilter
                  ? "No lines match the filter."
                  : "No lines yet."}
            </p>
          ) : (
            shown.map((l) => <Line key={l.id} line={l} />)
          )}
        </div>
        {!follow && (
          <Button
            size="sm"
            className="absolute right-4 bottom-3 shadow-md"
            onClick={() => setFollow(true)}
          >
            <ArrowDownToLineIcon />
            {newWhilePaused > 0 ? `${newWhilePaused} new lines` : "Follow"}
          </Button>
        )}
      </Card>
      <p className="text-xs text-muted-foreground">
        Last {LINE_CAP} lines are kept. Paused views keep receiving in the
        background.
      </p>
    </div>
  )
}
