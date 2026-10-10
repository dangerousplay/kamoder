import * as React from "react"
import { LightbulbIcon, MoonIcon, SunIcon } from "lucide-react"
import { toast } from "sonner"

import { Button } from "@/components/ui/button"
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card"
import { Label } from "@/components/ui/label"
import { Skeleton } from "@/components/ui/skeleton"
import { Switch } from "@/components/ui/switch"
import { usePolling } from "@/hooks/use-polling"
import {
  ApiError,
  api,
  errorMessage,
  type LightsAlarm,
  type LightsState,
  type LightsUpdate,
} from "@/lib/api"
import { useAuth } from "@/lib/auth"

/** One choice of a segmented control. */
interface Choice<T extends string> {
  value: T
  label: string
  icon?: React.ReactNode
}

function Segmented<T extends string>({
  label,
  value,
  choices,
  disabled,
  onChange,
}: {
  label: string
  value: T
  choices: Choice<T>[]
  disabled: boolean
  onChange: (value: T) => void
}) {
  return (
    <div className="flex items-center justify-between gap-4">
      <span className="text-sm text-muted-foreground">{label}</span>
      <div role="radiogroup" aria-label={label} className="flex gap-1">
        {choices.map((c) => (
          <Button
            key={c.value}
            role="radio"
            aria-checked={value === c.value}
            size="sm"
            variant={value === c.value ? "default" : "outline"}
            disabled={disabled}
            onClick={() => value !== c.value && onChange(c.value)}
          >
            {c.icon}
            {c.label}
          </Button>
        ))}
      </div>
    </div>
  )
}

function Toggle({
  id,
  label,
  checked,
  disabled,
  onChange,
}: {
  id: string
  label: string
  checked: boolean
  disabled: boolean
  onChange: (checked: boolean) => void
}) {
  return (
    <div className="flex items-center justify-between gap-4">
      <Label htmlFor={id} className="font-normal text-muted-foreground">
        {label}
      </Label>
      <Switch
        id={id}
        checked={checked}
        disabled={disabled}
        onCheckedChange={onChange}
      />
    </div>
  )
}

const MODES: Choice<LightsState["mode"]>[] = [
  { value: "day", label: "Day", icon: <SunIcon /> },
  { value: "night", label: "Night", icon: <MoonIcon /> },
]

const ALARMS: Choice<LightsAlarm["state"]>[] = [
  { value: "off", label: "Off" },
  { value: "on", label: "On" },
  { value: "blink", label: "Blink" },
]

/**
 * Lights owned by the `lights` service: day/night (IR-cut filter + IR LEDs),
 * white light, red/blue alarm lights and the speaker amp. Hidden when the
 * service is not running (503); viewers see the state, admins change it.
 */
export function LightsCard() {
  const { isAdmin } = useAuth()
  const { data, error, loading, setData } = usePolling(api.lights, 10000)
  const [pending, setPending] = React.useState(false)

  if (error instanceof ApiError && error.isUnavailable && !data) return null

  async function update(change: LightsUpdate) {
    setPending(true)
    try {
      setData(await api.updateLights(change))
    } catch (err) {
      toast.error(`Lights: ${errorMessage(err)}`)
    } finally {
      setPending(false)
    }
  }

  const disabled = !isAdmin || pending

  return (
    <Card>
      <CardHeader>
        <CardTitle className="flex items-center gap-2">
          <LightbulbIcon className="size-4" />
          Lights
        </CardTitle>
        <CardDescription>
          {isAdmin
            ? "Night removes the IR-cut filter and turns the IR LEDs on."
            : "Only admins can change the lights."}
        </CardDescription>
      </CardHeader>
      <CardContent className="flex flex-col gap-3">
        {loading && !data && <Skeleton className="h-32 w-full" />}
        {!data && !loading && !!error && (
          <p className="text-sm text-destructive">{errorMessage(error)}</p>
        )}
        {data && (
          <>
            <Segmented
              label="Mode"
              value={data.mode}
              choices={MODES}
              disabled={disabled}
              onChange={(mode) => update({ mode })}
            />
            <Toggle
              id="lights-white"
              label="White light"
              checked={data.white}
              disabled={disabled}
              onChange={(white) => update({ white })}
            />
            <Segmented
              label="Red / blue"
              value={data.alarm.state}
              choices={ALARMS}
              disabled={disabled}
              onChange={(state) => update({ alarm: { state } })}
            />
            <Toggle
              id="lights-muted"
              label="Speaker muted"
              checked={data.muted}
              disabled={disabled}
              onChange={(muted) => update({ muted })}
            />
          </>
        )}
      </CardContent>
    </Card>
  )
}
