import * as React from "react"
import { Loader2Icon } from "lucide-react"
import { toast } from "sonner"

import { Button } from "@/components/ui/button"
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card"
import {
  Field,
  FieldDescription,
  FieldGroup,
  FieldLabel,
} from "@/components/ui/field"
import { Input } from "@/components/ui/input"
import { api, errorMessage } from "@/lib/api"

/** ONVIF account (etc/onvif.secret), separate from web users (ONVIF-02). */
export function OnvifCard({
  currentUser,
  passwordSet,
}: {
  currentUser?: string
  passwordSet: boolean
}) {
  const [pending, setPending] = React.useState(false)
  const formRef = React.useRef<HTMLFormElement>(null)

  async function onSubmit(e: React.FormEvent<HTMLFormElement>) {
    e.preventDefault()
    const data = new FormData(e.currentTarget)
    const user = String(data.get("onvif_user") ?? "").trim()
    const password = String(data.get("onvif_password") ?? "")
    if (!user || !password) return
    setPending(true)
    try {
      await api.setOnvifCredentials({ user, password })
      toast.success("ONVIF credentials updated; the ONVIF service restarts")
      const pw = formRef.current?.elements.namedItem("onvif_password")
      if (pw instanceof HTMLInputElement) pw.value = ""
    } catch (err) {
      toast.error(`ONVIF credentials: ${errorMessage(err)}`)
    } finally {
      setPending(false)
    }
  }

  return (
    <Card>
      <CardHeader>
        <CardTitle>ONVIF credentials</CardTitle>
        <CardDescription>
          The account NVRs (Home Assistant, Frigate…) use on port 8000. It is
          not a web user. Both fields are required; the password is write-only.
        </CardDescription>
      </CardHeader>
      <CardContent>
        <form ref={formRef} onSubmit={onSubmit} aria-busy={pending}>
          <FieldGroup>
            <Field>
              <FieldLabel htmlFor="onvif_user">User</FieldLabel>
              <Input
                id="onvif_user"
                name="onvif_user"
                defaultValue={currentUser ?? ""}
                autoComplete="off"
                autoCapitalize="none"
                spellCheck={false}
                required
                disabled={pending}
              />
            </Field>
            <Field>
              <FieldLabel htmlFor="onvif_password">Password</FieldLabel>
              <Input
                id="onvif_password"
                name="onvif_password"
                type="password"
                autoComplete="new-password"
                placeholder={passwordSet ? "set" : "not set"}
                required
                disabled={pending}
              />
              <FieldDescription>
                Stored in plain text on the SD card (ONVIF digest auth needs
                it). Use a password you do not use elsewhere.
              </FieldDescription>
            </Field>
            <Field orientation="horizontal">
              <Button type="submit" disabled={pending}>
                {pending && <Loader2Icon className="animate-spin" />}
                Update ONVIF credentials
              </Button>
            </Field>
          </FieldGroup>
        </form>
      </CardContent>
    </Card>
  )
}
