import * as React from "react"
import {
  ArrowDownIcon,
  ArrowDownLeftIcon,
  ArrowDownRightIcon,
  ArrowLeftIcon,
  ArrowRightIcon,
  ArrowUpIcon,
  ArrowUpLeftIcon,
  ArrowUpRightIcon,
  SquareIcon,
  type LucideIcon,
} from "lucide-react"

import type { Direction } from "@/components/ptz/use-ptz-control"
import { Button } from "@/components/ui/button"
import { cn } from "@/lib/utils"

interface PadKey extends Direction {
  label: string
  icon: LucideIcon
}

// Row-major 3x3; tilt > 0 is up (ONVIF convention).
const PAD: PadKey[] = [
  { label: "Up left", icon: ArrowUpLeftIcon, pan: -1, tilt: 1 },
  { label: "Up", icon: ArrowUpIcon, pan: 0, tilt: 1 },
  { label: "Up right", icon: ArrowUpRightIcon, pan: 1, tilt: 1 },
  { label: "Left", icon: ArrowLeftIcon, pan: -1, tilt: 0 },
  { label: "Stop", icon: SquareIcon, pan: 0, tilt: 0 },
  { label: "Right", icon: ArrowRightIcon, pan: 1, tilt: 0 },
  { label: "Down left", icon: ArrowDownLeftIcon, pan: -1, tilt: -1 },
  { label: "Down", icon: ArrowDownIcon, pan: 0, tilt: -1 },
  { label: "Down right", icon: ArrowDownRightIcon, pan: 1, tilt: -1 },
]

const ARROWS: Record<string, Direction> = {
  ArrowUp: { pan: 0, tilt: 1 },
  ArrowDown: { pan: 0, tilt: -1 },
  ArrowLeft: { pan: -1, tilt: 0 },
  ArrowRight: { pan: 1, tilt: 0 },
}

function isTyping(target: EventTarget | null) {
  if (!(target instanceof HTMLElement)) return false
  return (
    target.isContentEditable ||
    ["INPUT", "TEXTAREA", "SELECT"].includes(target.tagName) ||
    target.getAttribute("role") === "slider"
  )
}

/**
 * Press-and-hold pad: pointerdown starts a continuous move, release / leave /
 * cancel stops it. Arrow keys (combinable for diagonals) work page-wide.
 */
export function PtzPad({
  moving,
  onMove,
  onStop,
  disabled,
}: {
  moving: boolean
  onMove: (dir: Direction) => void
  onStop: () => void
  disabled?: boolean
}) {
  const [active, setActive] = React.useState<string | null>(null)

  const onMoveEvent = React.useEffectEvent(onMove)
  const onStopEvent = React.useEffectEvent(onStop)

  // Page-wide arrow keys.
  React.useEffect(() => {
    if (disabled) return
    const held = new Set<string>()
    const apply = () => {
      let pan = 0
      let tilt = 0
      for (const k of held) {
        pan += ARROWS[k]!.pan
        tilt += ARROWS[k]!.tilt
      }
      pan = Math.sign(pan)
      tilt = Math.sign(tilt)
      if (pan === 0 && tilt === 0) onStopEvent()
      else onMoveEvent({ pan, tilt })
    }
    const down = (e: KeyboardEvent) => {
      if (!(e.key in ARROWS) || e.altKey || e.ctrlKey || e.metaKey) return
      if (isTyping(e.target)) return
      e.preventDefault()
      if (e.repeat || held.has(e.key)) return
      held.add(e.key)
      apply()
    }
    const up = (e: KeyboardEvent) => {
      if (!held.delete(e.key)) return
      apply()
    }
    const reset = () => held.clear()
    window.addEventListener("keydown", down)
    window.addEventListener("keyup", up)
    window.addEventListener("blur", reset)
    return () => {
      window.removeEventListener("keydown", down)
      window.removeEventListener("keyup", up)
      window.removeEventListener("blur", reset)
    }
  }, [disabled])

  // onStop is a no-op when nothing moves, so hover-leave without a press is fine.
  function release() {
    setActive(null)
    onStop()
  }

  return (
    <div
      className="grid w-full max-w-64 touch-none grid-cols-3 gap-2 select-none"
      role="group"
      aria-label="Pan and tilt (hold a direction; arrow keys also work)"
    >
      {PAD.map((k) => {
        const isStop = k.pan === 0 && k.tilt === 0
        const Icon = k.icon
        const common = {
          "aria-label": k.label,
          title: k.label,
          disabled,
          variant: (isStop ? "outline" : "secondary") as
            "outline" | "secondary",
          className: cn(
            "aspect-square h-auto w-full [&_svg:not([class*='size-'])]:size-6",
            active === k.label && "bg-primary text-primary-foreground",
            isStop && moving && "border-destructive text-destructive"
          ),
        }
        if (isStop) {
          return (
            <Button key={k.label} {...common} onClick={onStop}>
              <Icon />
            </Button>
          )
        }
        return (
          <Button
            key={k.label}
            {...common}
            onPointerDown={(e) => {
              if (e.button !== 0) return
              e.preventDefault()
              e.currentTarget.setPointerCapture(e.pointerId)
              setActive(k.label)
              onMove(k)
            }}
            onPointerUp={release}
            onPointerCancel={release}
            onLostPointerCapture={release}
            onPointerLeave={release}
            onContextMenu={(e) => e.preventDefault()}
            onKeyDown={(e) => {
              if ((e.key === " " || e.key === "Enter") && !e.repeat) {
                e.preventDefault()
                setActive(k.label)
                onMove(k)
              }
            }}
            onKeyUp={(e) => {
              if (e.key === " " || e.key === "Enter") release()
            }}
            onBlur={release}
          >
            <Icon />
          </Button>
        )
      })}
    </div>
  )
}
