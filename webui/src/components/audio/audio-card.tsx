import * as React from "react"
import { ActivityIcon, MicIcon, SpeakerIcon, Volume2Icon } from "lucide-react"
import { toast } from "sonner"

import { ListenButton } from "@/components/audio/listen-button"
import { useTalk } from "@/components/audio/use-audio"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card"
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select"
import { Separator } from "@/components/ui/separator"
import { Skeleton } from "@/components/ui/skeleton"
import { Slider } from "@/components/ui/slider"
import { usePolling } from "@/hooks/use-polling"
import {
  ApiError,
  api,
  errorMessage,
  type AudioDevice,
  type AudioDeviceState,
  type AudioDevices,
} from "@/lib/api"
import { Talker } from "@/lib/audio/talk"
import { useAuth } from "@/lib/auth"
import { cn } from "@/lib/utils"

/** Gain changes are sent this long after the last slider commit. */
const GAIN_DEBOUNCE_MS = 300

const STATE_STYLES: Record<AudioDeviceState, string> = {
  active:
    "border-emerald-600/30 bg-emerald-500/10 text-emerald-700 dark:text-emerald-400",
  idle: "bg-secondary text-secondary-foreground",
  error: "border-destructive/30 bg-destructive/10 text-destructive",
}

function formatRate(rate: number): string {
  return `${rate / 1000} kHz G.711`
}

/** Mic gain / speaker volume; sends the value once the user settles. */
function GainControl({
  device,
  onCommit,
}: {
  device: AudioDevice
  onCommit: (gain: number) => void
}) {
  const [draft, setDraft] = React.useState<number | null>(null)
  const timer = React.useRef<number | undefined>(undefined)
  React.useEffect(() => () => window.clearTimeout(timer.current), [])

  const [min, max] = device.gain_range
  const value = draft ?? device.gain ?? min
  const label = device.kind === "input" ? "Gain" : "Volume"
  const id = `audio-gain-${device.id}`

  return (
    <div className="flex items-center gap-3 pl-7">
      <span id={id} className="w-14 text-sm text-muted-foreground">
        {label}
      </span>
      <Slider
        aria-labelledby={id}
        min={min}
        max={max}
        step={1}
        value={[value]}
        onValueChange={([v]) => setDraft(v)}
        onValueCommit={([v]) => {
          window.clearTimeout(timer.current)
          timer.current = window.setTimeout(() => {
            setDraft(null)
            if (v !== device.gain) onCommit(v)
          }, GAIN_DEBOUNCE_MS)
        }}
      />
      <span className="w-8 text-right text-sm tabular-nums">{value}</span>
    </div>
  )
}

function DeviceRow({
  device,
  canEdit,
  onGain,
}: {
  device: AudioDevice
  canEdit: boolean
  onGain: (gain: number) => void
}) {
  const Icon = device.kind === "input" ? MicIcon : SpeakerIcon
  return (
    <li className="flex flex-col gap-2">
      <div className="flex items-center gap-3">
        <Icon
          className="size-4 shrink-0 text-muted-foreground"
          aria-label={device.kind === "input" ? "Microphone" : "Speaker"}
        />
        <div className="min-w-0 flex-1">
          <div className="truncate text-sm font-medium">{device.name}</div>
          <div className="text-xs text-muted-foreground">
            {formatRate(device.rate)}
          </div>
        </div>
        <Badge
          variant="outline"
          className={STATE_STYLES[device.state] ?? ""}
          title={device.error ?? undefined}
        >
          {device.state}
        </Badge>
      </div>
      {device.state === "error" && device.error && (
        <p className="pl-7 text-xs text-destructive">{device.error}</p>
      )}
      {canEdit && device.gain !== null && (
        <GainControl device={device} onCommit={onGain} />
      )}
    </li>
  )
}

/** Why Talk cannot work on this page, or null when it can. */
function TalkHint({ httpsPort }: { httpsPort: number | null }) {
  if (window.isSecureContext) {
    return (
      <p className="text-xs text-muted-foreground">
        This browser cannot capture audio (no microphone or AudioWorklet
        support).
      </p>
    )
  }
  if (httpsPort === null) {
    return (
      <p className="text-xs text-muted-foreground">
        Push-to-talk needs the page over HTTPS, and HTTPS is not enabled on this
        camera.
      </p>
    )
  }
  const port = httpsPort === 443 ? "" : `:${httpsPort}`
  const url = `https://${window.location.hostname}${port}/`
  return (
    <p className="text-xs text-muted-foreground">
      Browsers only give the microphone to secure pages. Open{" "}
      <a href={url} className="font-medium text-primary underline">
        {url}
      </a>{" "}
      to talk (accept the camera's self-signed certificate the first time).
    </p>
  )
}

/** Radix Select needs a non-empty value for "system default". */
const DEFAULT_MIC = "default-mic"

function TalkControl({ httpsPort }: { httpsPort: number | null }) {
  const meter = React.useRef<HTMLDivElement>(null)
  const {
    state,
    error,
    press,
    release,
    testing,
    toggleTest,
    mics,
    device,
    setDevice,
  } = useTalk(meter)
  const supported = Talker.supported()
  const active = state !== "idle"
  const known = device === null || mics.some((m) => m.deviceId === device)

  return (
    <div className="flex flex-col gap-2">
      {supported && (
        <div className="flex flex-wrap items-center gap-2">
          <Select
            value={device ?? DEFAULT_MIC}
            onValueChange={(v) => setDevice(v === DEFAULT_MIC ? null : v)}
          >
            <SelectTrigger
              aria-label="Microphone for push-to-talk"
              className="min-w-0 flex-1 sm:max-w-72"
            >
              <MicIcon />
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              <SelectItem value={DEFAULT_MIC}>System default</SelectItem>
              {!known && device && (
                <SelectItem value={device}>Saved microphone</SelectItem>
              )}
              {mics.map((m) => (
                <SelectItem key={m.deviceId} value={m.deviceId}>
                  {m.label}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
          <Button
            variant={testing ? "default" : "outline"}
            aria-pressed={testing}
            onClick={toggleTest}
          >
            <ActivityIcon />
            {testing ? "Stop test" : "Test mic"}
          </Button>
        </div>
      )}
      {supported &&
        mics.length > 0 &&
        mics.every((m) => /^Microphone \d+$/.test(m.label)) && (
          <p className="text-xs text-muted-foreground">
            Press Test mic once to allow the microphone; the names appear after
            that.
          </p>
        )}
      <div className="flex items-center gap-3">
        <Button
          variant={active ? "default" : "secondary"}
          disabled={!supported}
          aria-label="Hold to talk"
          aria-pressed={active}
          className="touch-none select-none"
          onPointerDown={(e) => {
            if (e.button !== 0) return
            e.preventDefault()
            e.currentTarget.focus()
            press()
          }}
          onPointerUp={release}
          onPointerCancel={release}
          onPointerLeave={release}
          onContextMenu={(e) => e.preventDefault()}
          onKeyDown={(e) => {
            if ((e.key === " " || e.key === "Enter") && !e.repeat) {
              e.preventDefault()
              press()
            }
          }}
          onKeyUp={(e) => {
            if (e.key === " " || e.key === "Enter") release()
          }}
          onBlur={release}
        >
          <MicIcon />
          {state === "starting"
            ? "Starting…"
            : state === "talking"
              ? "Talking…"
              : "Hold to talk"}
        </Button>
        <div
          className={cn(
            "h-2 flex-1 overflow-hidden rounded-full bg-muted transition-opacity",
            active || testing ? "opacity-100" : "opacity-0"
          )}
          role="presentation"
        >
          <div
            ref={meter}
            className="h-full w-full origin-left scale-x-0 bg-emerald-500"
          />
        </div>
      </div>
      {!supported && <TalkHint httpsPort={httpsPort} />}
      {error && <p className="text-xs text-destructive">{error}</p>}
    </div>
  )
}

/**
 * Microphone and speaker of the `audio` service: device list with state,
 * gain/volume (admins), live listening and push-to-talk (admins). Hidden on
 * boards without audio (no `audio` service); "unavailable" when the service
 * is listed but down (503).
 */
export function AudioCard({ listed }: { listed: boolean }) {
  const { isAdmin } = useAuth()
  const { data, error, loading, setData, refresh } = usePolling(
    api.audioDevices,
    10000
  )
  const unavailable = error instanceof ApiError && error.isUnavailable

  if (unavailable && !data && !listed) return null

  async function setGain(device: AudioDevice, gain: number) {
    const patch = (d: AudioDevices | undefined, next: AudioDevice) =>
      d && {
        ...d,
        devices: d.devices.map((x) => (x.id === next.id ? next : x)),
      }
    setData((d) => patch(d, { ...device, gain }))
    try {
      const updated = await api.updateAudioDevice(device.id, gain)
      setData((d) => patch(d, updated))
    } catch (err) {
      toast.error(`${device.name}: ${errorMessage(err)}`)
      refresh()
    }
  }

  const devices = data?.devices ?? []
  const hasInput = devices.some((d) => d.kind === "input")
  const hasOutput = devices.some((d) => d.kind === "output")

  return (
    <Card>
      <CardHeader>
        <CardTitle className="flex items-center gap-2">
          <Volume2Icon className="size-4" />
          Audio
        </CardTitle>
        <CardDescription>
          {isAdmin
            ? "Listen to the camera, or hold Talk to speak through it."
            : "Listen to the camera microphone."}
        </CardDescription>
      </CardHeader>
      <CardContent className="flex flex-col gap-4">
        {loading && !data && !error && <Skeleton className="h-24 w-full" />}
        {unavailable && (
          <p className="text-sm text-muted-foreground">
            Audio unavailable: the audio service is not running.
          </p>
        )}
        {!unavailable && !data && !!error && (
          <p className="text-sm text-destructive">{errorMessage(error)}</p>
        )}
        {data && !unavailable && (
          <>
            {devices.length === 0 ? (
              <p className="text-sm text-muted-foreground">
                This board has no audio devices.
              </p>
            ) : (
              <ul className="flex flex-col gap-3">
                {devices.map((d) => (
                  <DeviceRow
                    key={d.id}
                    device={d}
                    canEdit={isAdmin}
                    onGain={(gain) => void setGain(d, gain)}
                  />
                ))}
              </ul>
            )}
            {(hasInput || (isAdmin && hasOutput)) && <Separator />}
            {hasInput && (
              <div>
                <ListenButton />
              </div>
            )}
            {isAdmin && hasOutput && (
              <TalkControl httpsPort={data.talk.https_port} />
            )}
          </>
        )}
      </CardContent>
    </Card>
  )
}
