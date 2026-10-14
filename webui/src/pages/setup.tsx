import * as React from "react"
import {
  CheckCircle2Icon,
  CircleIcon,
  Loader2Icon,
  ShieldCheckIcon,
} from "lucide-react"
import { useNavigate } from "react-router"
import { toast } from "sonner"

import { OnvifCard } from "@/components/settings/onvif-card"
import { SshCard } from "@/components/settings/ssh-card"
import { HardwareCard } from "@/components/system/hardware-card"
import { NewPasswordFields } from "@/components/users/password-fields"
import { Button } from "@/components/ui/button"
import {
  Card,
  CardContent,
  CardDescription,
  CardFooter,
  CardHeader,
  CardTitle,
} from "@/components/ui/card"
import { Field, FieldGroup, FieldLabel } from "@/components/ui/field"
import { Input } from "@/components/ui/input"
import { usePolling } from "@/hooks/use-polling"
import { api, errorMessage, type SetupStatus } from "@/lib/api"
import { useAuth } from "@/lib/auth"
import { cn } from "@/lib/utils"

const STEPS = [
  { id: "welcome", title: "Your camera" },
  { id: "admin", title: "Admin password" },
  { id: "onvif", title: "ONVIF" },
  { id: "ssh", title: "SSH" },
  { id: "done", title: "Done" },
] as const

function Stepper({ step }: { step: number }) {
  return (
    <ol className="flex flex-wrap items-center gap-x-4 gap-y-2 text-sm">
      {STEPS.map((s, i) => (
        <li
          key={s.id}
          className={cn(
            "flex items-center gap-1.5",
            i === step ? "font-semibold" : "text-muted-foreground"
          )}
          aria-current={i === step ? "step" : undefined}
        >
          {i < step ? (
            <CheckCircle2Icon className="size-4 text-emerald-600" />
          ) : (
            <CircleIcon
              className={cn("size-4", i === step && "fill-current")}
            />
          )}
          {s.title}
        </li>
      ))}
    </ol>
  )
}

/** Step 2: the signed-in admin's own web password. */
function AdminPasswordStep({ onDone }: { onDone: () => void }) {
  const { user, adoptToken } = useAuth()
  const [current, setCurrent] = React.useState("")
  const [next, setNext] = React.useState<string | null>(null)
  const [pending, setPending] = React.useState(false)

  async function submit(e: React.FormEvent) {
    e.preventDefault()
    if (!current || !next) return
    setPending(true)
    try {
      const res = await api.changeOwnPassword({ current, new: next })
      adoptToken(res.token)
      toast.success("Admin password changed")
      onDone()
    } catch (err) {
      toast.error(`Password not changed: ${errorMessage(err)}`)
    } finally {
      setPending(false)
    }
  }

  return (
    <Card>
      <CardHeader>
        <CardTitle>Admin password</CardTitle>
        <CardDescription>
          Replace the password you signed in with (from camera.conf) by one only
          you know. This is the web admin password for <b>{user}</b>; SSH and
          ONVIF have their own.
        </CardDescription>
      </CardHeader>
      <CardContent>
        <form id="admin-pw" onSubmit={submit} aria-busy={pending}>
          <FieldGroup>
            <Field>
              <FieldLabel htmlFor="setup-current">Current password</FieldLabel>
              <Input
                id="setup-current"
                type="password"
                autoComplete="current-password"
                value={current}
                onChange={(e) => setCurrent(e.target.value)}
                disabled={pending}
              />
            </Field>
            <NewPasswordFields
              idPrefix="setup-new"
              onChange={setNext}
              disabled={pending}
            />
            <Field orientation="horizontal">
              <Button type="submit" disabled={pending || !current || !next}>
                {pending && <Loader2Icon className="animate-spin" />}
                Change password
              </Button>
            </Field>
          </FieldGroup>
        </form>
      </CardContent>
    </Card>
  )
}

function Summary({ status }: { status?: SetupStatus }) {
  const items = [
    {
      ok: !!status?.onvif_configured,
      text: status?.onvif_configured
        ? `ONVIF account "${status.onvif_user}" configured`
        : "ONVIF not configured (NVRs cannot connect)",
    },
    {
      ok: !!status && (status.ssh_password_set || status.ssh_keys > 0),
      text: status
        ? `SSH: ${status.ssh_password_set ? "password set" : "no password"}, ${status.ssh_keys} public key${status.ssh_keys === 1 ? "" : "s"}`
        : "SSH",
    },
  ]
  return (
    <ul className="flex flex-col gap-2 text-sm">
      {items.map((i) => (
        <li key={i.text} className="flex items-center gap-2">
          {i.ok ? (
            <CheckCircle2Icon className="size-4 text-emerald-600" />
          ) : (
            <CircleIcon className="size-4 text-muted-foreground" />
          )}
          {i.text}
        </li>
      ))}
    </ul>
  )
}

/**
 * First-run wizard: shown to an admin until finished or skipped. Every step
 * is optional and uses the regular API, so the same settings stay editable
 * later under Settings.
 */
export default function SetupPage() {
  const navigate = useNavigate()
  const [step, setStep] = React.useState(0)
  const [finishing, setFinishing] = React.useState(false)
  const hardware = usePolling(api.hardware, null)
  const setup = usePolling(api.setup, null)
  const last = STEPS.length - 1

  const go = (n: number) => {
    setStep(Math.max(0, Math.min(last, n)))
    if (n === last) setup.refresh()
    window.scrollTo({ top: 0 })
  }

  async function finish() {
    setFinishing(true)
    try {
      await api.completeSetup()
      toast.success("Setup complete")
      navigate("/", { replace: true })
    } catch (e) {
      toast.error(errorMessage(e))
      setFinishing(false)
    }
  }

  const current = STEPS[step].id

  return (
    <div className="mx-auto flex w-full max-w-2xl flex-col gap-4">
      <div className="flex flex-col gap-3">
        <h1 className="flex items-center gap-2 text-xl font-semibold">
          <ShieldCheckIcon className="size-5" />
          Welcome to Kamoder
        </h1>
        <p className="text-sm text-muted-foreground">
          A few steps to secure the camera and connect it to your recorder.
          Every step can be skipped and changed later in Settings.
        </p>
        <Stepper step={step} />
      </div>

      {current === "welcome" && (
        <>
          <HardwareCard hw={hardware.data} error={hardware.error} />
          {hardware.data && hardware.data.camera_count > 1 && (
            <p className="text-sm text-muted-foreground">
              This board has {hardware.data.camera_count} image sensors; the
              dashboard shows one tab per camera.
            </p>
          )}
        </>
      )}
      {current === "admin" && <AdminPasswordStep onDone={() => go(step + 1)} />}
      {current === "onvif" && (
        <OnvifCard
          currentUser={setup.data?.onvif_user ?? undefined}
          passwordSet={!!setup.data?.onvif_configured}
        />
      )}
      {current === "ssh" && <SshCard />}
      {current === "done" && (
        <Card>
          <CardHeader>
            <CardTitle>All set</CardTitle>
            <CardDescription>
              Review below, then finish. You can run this wizard again from
              Settings.
            </CardDescription>
          </CardHeader>
          <CardContent>
            <Summary status={setup.data} />
          </CardContent>
          <CardFooter>
            <Button onClick={finish} disabled={finishing}>
              {finishing && <Loader2Icon className="animate-spin" />}
              Finish setup
            </Button>
          </CardFooter>
        </Card>
      )}

      <div className="flex items-center justify-between gap-2">
        <Button
          variant="ghost"
          onClick={() => go(step - 1)}
          disabled={step === 0 || finishing}
        >
          Back
        </Button>
        <div className="flex gap-2">
          {step < last && (
            <>
              <Button variant="ghost" onClick={finish} disabled={finishing}>
                Skip setup
              </Button>
              <Button onClick={() => go(step + 1)}>
                {step === 0 ? "Start" : "Next"}
              </Button>
            </>
          )}
        </div>
      </div>
    </div>
  )
}
