import * as React from "react"
import { Loader2Icon, PowerIcon } from "lucide-react"
import { createPortal } from "react-dom"
import { toast } from "sonner"

import { ConfirmDialog } from "@/components/common/confirm-dialog"
import { Button } from "@/components/ui/button"
import {
  Card,
  CardAction,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card"
import { api, errorMessage } from "@/lib/api"

const POLL_MS = 3000
/** Treat the camera as "went down" after this even if no failure was seen. */
const ASSUME_DOWN_MS = 30000
const SLOW_MS = 180000

/**
 * Full-screen "rebooting" state: waits for /api/health to fail (camera went
 * down), then to answer again, then reloads the SPA.
 */
function RebootingScreen() {
  const [elapsed, setElapsed] = React.useState(0)

  React.useEffect(() => {
    const started = Date.now()
    let wentDown = false
    let stopped = false
    let timer: number | undefined

    async function poll() {
      const ctrl = new AbortController()
      const abort = window.setTimeout(() => ctrl.abort(), POLL_MS)
      try {
        await api.health(ctrl.signal)
        if (wentDown || Date.now() - started > ASSUME_DOWN_MS) {
          window.location.reload()
          return
        }
      } catch {
        wentDown = true
      } finally {
        window.clearTimeout(abort)
      }
      if (!stopped) timer = window.setTimeout(poll, POLL_MS)
    }

    timer = window.setTimeout(poll, POLL_MS)
    const tick = window.setInterval(
      () => setElapsed(Math.round((Date.now() - started) / 1000)),
      1000
    )
    return () => {
      stopped = true
      window.clearTimeout(timer)
      window.clearInterval(tick)
    }
  }, [])

  return (
    <div
      className="fixed inset-0 z-50 flex flex-col items-center justify-center gap-3 bg-background p-6 text-center"
      role="status"
      aria-live="polite"
    >
      <Loader2Icon className="size-8 animate-spin text-muted-foreground" />
      <p className="text-lg font-medium">Rebooting… reconnecting</p>
      <p className="text-sm text-muted-foreground tabular-nums">
        {elapsed} s — the page reloads when the camera answers again.
      </p>
      {elapsed * 1000 > SLOW_MS && (
        <div className="flex max-w-sm flex-col items-center gap-2">
          <p className="text-sm text-amber-700 dark:text-amber-400">
            This is taking longer than usual. The camera may have moved to
            another Wi-Fi network or be stuck; check its power and network.
          </p>
          <Button variant="outline" onClick={() => window.location.reload()}>
            Reload now
          </Button>
        </div>
      )}
    </div>
  )
}

export function RebootCard() {
  const [confirm, setConfirm] = React.useState(false)
  const [rebooting, setRebooting] = React.useState(false)

  async function reboot() {
    try {
      await api.reboot()
    } catch (e) {
      toast.error(`Reboot failed: ${errorMessage(e)}`)
      throw e
    }
    setRebooting(true)
  }

  return (
    <Card>
      <CardHeader>
        <CardTitle>Reboot</CardTitle>
        <CardDescription>
          Stops all services cleanly, syncs the SD card and restarts the camera.
          Takes about a minute.
        </CardDescription>
        <CardAction>
          <Button variant="destructive" onClick={() => setConfirm(true)}>
            <PowerIcon />
            Reboot
          </Button>
        </CardAction>
      </CardHeader>
      <ConfirmDialog
        open={confirm}
        onOpenChange={setConfirm}
        title="Reboot the camera?"
        description="Video, ONVIF and SSH will be offline until it is back."
        confirmLabel="Reboot"
        destructive
        onConfirm={reboot}
      />
      {rebooting && createPortal(<RebootingScreen />, document.body)}
    </Card>
  )
}
