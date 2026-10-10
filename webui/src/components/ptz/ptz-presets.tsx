import * as React from "react"
import {
  BookmarkPlusIcon,
  Loader2Icon,
  NavigationIcon,
  Trash2Icon,
} from "lucide-react"
import { toast } from "sonner"

import { ConfirmDialog } from "@/components/common/confirm-dialog"
import { Button } from "@/components/ui/button"
import {
  Card,
  CardAction,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card"
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog"
import { Field, FieldLabel } from "@/components/ui/field"
import { Input } from "@/components/ui/input"
import { Skeleton } from "@/components/ui/skeleton"
import { usePolling } from "@/hooks/use-polling"
import { api, errorMessage, type PtzPreset } from "@/lib/api"

function SavePresetDialog({
  open,
  onOpenChange,
  onSaved,
}: {
  open: boolean
  onOpenChange: (open: boolean) => void
  onSaved: () => void
}) {
  const [pending, setPending] = React.useState(false)

  async function onSubmit(e: React.FormEvent<HTMLFormElement>) {
    e.preventDefault()
    const name = String(new FormData(e.currentTarget).get("name") ?? "").trim()
    if (!name) return
    setPending(true)
    try {
      await api.ptzSavePreset(name)
      toast.success(`Preset "${name}" saved`)
      onSaved()
      onOpenChange(false)
    } catch (err) {
      toast.error(`Save preset failed: ${errorMessage(err)}`)
    } finally {
      setPending(false)
    }
  }

  return (
    <Dialog open={open} onOpenChange={(o) => !pending && onOpenChange(o)}>
      <DialogContent className="sm:max-w-sm">
        <form onSubmit={onSubmit} className="flex flex-col gap-4">
          <DialogHeader>
            <DialogTitle>Save preset</DialogTitle>
            <DialogDescription>
              Stores the current position under a name.
            </DialogDescription>
          </DialogHeader>
          <Field>
            <FieldLabel htmlFor="preset-name">Name</FieldLabel>
            <Input
              id="preset-name"
              name="name"
              maxLength={32}
              required
              autoFocus
              disabled={pending}
            />
          </Field>
          <DialogFooter>
            <Button type="submit" disabled={pending}>
              {pending && <Loader2Icon className="animate-spin" />}
              Save
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  )
}

export function PtzPresets({
  isAdmin,
  onGoto,
}: {
  isAdmin: boolean
  /** Called after a goto was accepted (the page polls position faster). */
  onGoto: () => void
}) {
  const { data, error, loading, refresh } = usePolling(api.ptzPresets, null)
  const [saving, setSaving] = React.useState(false)
  const [deleting, setDeleting] = React.useState<PtzPreset | null>(null)
  const [going, setGoing] = React.useState<number | null>(null)

  async function goto(p: PtzPreset) {
    setGoing(p.id)
    try {
      await api.ptzGoto({ preset: p.id })
      onGoto()
    } catch (e) {
      toast.error(`Go to "${p.name}" failed: ${errorMessage(e)}`)
    } finally {
      setGoing(null)
    }
  }

  async function remove(p: PtzPreset) {
    try {
      await api.ptzDeletePreset(p.id)
      toast.success(`Preset "${p.name}" deleted`)
      refresh()
    } catch (e) {
      toast.error(`Delete failed: ${errorMessage(e)}`)
      throw e
    }
  }

  return (
    <Card>
      <CardHeader>
        <CardTitle>Presets</CardTitle>
        <CardDescription>Stored positions</CardDescription>
        {isAdmin && (
          <CardAction>
            <Button variant="outline" size="sm" onClick={() => setSaving(true)}>
              <BookmarkPlusIcon />
              Save current
            </Button>
          </CardAction>
        )}
      </CardHeader>
      <CardContent>
        {loading && !data && <Skeleton className="h-16 w-full" />}
        {!loading && !data && !!error && (
          <p className="text-sm text-destructive">{errorMessage(error)}</p>
        )}
        {data?.length === 0 && (
          <p className="text-sm text-muted-foreground">
            No presets yet.
            {isAdmin && " Move the camera and use “Save current”."}
          </p>
        )}
        {!!data?.length && (
          <ul className="flex flex-col divide-y text-sm">
            {data.map((p) => (
              <li key={p.id} className="flex items-center gap-2 py-1.5">
                <span className="flex-1 truncate font-medium">{p.name}</span>
                <span className="text-xs text-muted-foreground tabular-nums">
                  {p.pan.toFixed(1)}, {p.tilt.toFixed(1)}
                </span>
                {isAdmin && (
                  <>
                    <Button
                      variant="ghost"
                      size="icon-sm"
                      onClick={() => goto(p)}
                      disabled={going !== null}
                      aria-label={`Go to ${p.name}`}
                      title="Go to"
                    >
                      {going === p.id ? (
                        <Loader2Icon className="animate-spin" />
                      ) : (
                        <NavigationIcon />
                      )}
                    </Button>
                    <Button
                      variant="ghost"
                      size="icon-sm"
                      className="text-destructive"
                      onClick={() => setDeleting(p)}
                      aria-label={`Delete ${p.name}`}
                      title="Delete"
                    >
                      <Trash2Icon />
                    </Button>
                  </>
                )}
              </li>
            ))}
          </ul>
        )}
      </CardContent>
      {isAdmin && (
        <>
          <SavePresetDialog
            open={saving}
            onOpenChange={setSaving}
            onSaved={refresh}
          />
          <ConfirmDialog
            open={deleting !== null}
            onOpenChange={(o) => !o && setDeleting(null)}
            title={`Delete preset "${deleting?.name ?? ""}"?`}
            confirmLabel="Delete"
            destructive
            onConfirm={() => (deleting ? remove(deleting) : undefined)}
          />
        </>
      )}
    </Card>
  )
}
