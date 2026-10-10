import * as React from "react"
import { Loader2Icon } from "lucide-react"
import { toast } from "sonner"

import { NewPasswordFields } from "@/components/users/password-fields"
import { Button } from "@/components/ui/button"
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog"
import { Field, FieldGroup, FieldLabel } from "@/components/ui/field"
import { Input } from "@/components/ui/input"
import { api, errorMessage } from "@/lib/api"
import { useAuth } from "@/lib/auth"

/** USR-02: any user changes their own password (current one required). */
export function ChangePasswordDialog({
  open,
  onOpenChange,
}: {
  open: boolean
  onOpenChange: (open: boolean) => void
}) {
  const { adoptToken } = useAuth()
  const [current, setCurrent] = React.useState("")
  const [next, setNext] = React.useState<string | null>(null)
  const [pending, setPending] = React.useState(false)
  // Remount the fields on every open so nothing lingers.
  const [generation, setGeneration] = React.useState(0)

  function change(o: boolean) {
    if (pending) return
    if (!o) {
      setCurrent("")
      setNext(null)
      setGeneration((g) => g + 1)
    }
    onOpenChange(o)
  }

  async function onSubmit(e: React.FormEvent) {
    e.preventDefault()
    if (!current || !next) return
    setPending(true)
    try {
      const res = await api.changeOwnPassword({ current, new: next })
      adoptToken(res.token)
      toast.success("Password changed")
      setPending(false)
      setCurrent("")
      setNext(null)
      setGeneration((g) => g + 1)
      onOpenChange(false)
    } catch (err) {
      toast.error(`Password not changed: ${errorMessage(err)}`)
      setPending(false)
    }
  }

  return (
    <Dialog open={open} onOpenChange={change}>
      <DialogContent className="sm:max-w-sm">
        <form onSubmit={onSubmit} className="flex flex-col gap-4">
          <DialogHeader>
            <DialogTitle>Change my password</DialogTitle>
            <DialogDescription>
              Applies to the web UI only (not SSH or ONVIF).
            </DialogDescription>
          </DialogHeader>
          <FieldGroup key={generation}>
            <Field>
              <FieldLabel htmlFor="me-current">Current password</FieldLabel>
              <Input
                id="me-current"
                type="password"
                autoComplete="current-password"
                required
                value={current}
                onChange={(e) => setCurrent(e.target.value)}
                disabled={pending}
              />
            </Field>
            <NewPasswordFields
              idPrefix="me-new"
              onChange={setNext}
              disabled={pending}
            />
          </FieldGroup>
          <DialogFooter>
            <Button type="submit" disabled={pending || !current || !next}>
              {pending && <Loader2Icon className="animate-spin" />}
              Change password
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  )
}
