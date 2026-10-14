import * as React from "react"
import { KeyRoundIcon, Loader2Icon, Trash2Icon } from "lucide-react"
import { toast } from "sonner"

import { NewPasswordFields } from "@/components/users/password-fields"
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
  Field,
  FieldDescription,
  FieldGroup,
  FieldLabel,
} from "@/components/ui/field"
import { Separator } from "@/components/ui/separator"
import { Skeleton } from "@/components/ui/skeleton"
import { api, errorMessage, type SshStatus } from "@/lib/api"
import { cn } from "@/lib/utils"

/** sshd's port (supervisor.toml). */
const SSH_PORT = 2222

/**
 * SSH access: the sshd password and its authorized public keys. sshd re-reads
 * both files on the next login, so changes apply without a restart.
 */
export function SshCard({
  onChange,
  className,
}: {
  /** Called with the new status after any change. */
  onChange?: (status: SshStatus) => void
  className?: string
}) {
  const [status, setStatus] = React.useState<SshStatus | null>(null)
  const [error, setError] = React.useState<unknown>(null)
  const [password, setPassword] = React.useState<string | null>(null)
  const [fieldsKey, setFieldsKey] = React.useState(0)
  const [key, setKey] = React.useState("")
  const [pending, setPending] = React.useState<string | null>(null)

  React.useEffect(() => {
    const ctrl = new AbortController()
    api
      .ssh(ctrl.signal)
      .then(setStatus)
      .catch((e) => {
        if (!ctrl.signal.aborted) setError(e)
      })
    return () => ctrl.abort()
  }, [])

  function adopt(s: SshStatus) {
    setStatus(s)
    onChange?.(s)
  }

  async function run(what: string, action: () => Promise<SshStatus>) {
    setPending(what)
    try {
      adopt(await action())
      return true
    } catch (e) {
      toast.error(errorMessage(e))
      return false
    } finally {
      setPending(null)
    }
  }

  async function savePassword(e: React.FormEvent) {
    e.preventDefault()
    if (!password) return
    if (await run("password", () => api.setSshPassword(password))) {
      toast.success("SSH password set")
      setPassword(null)
      setFieldsKey((k) => k + 1)
    }
  }

  async function addKey(e: React.FormEvent) {
    e.preventDefault()
    if (!key.trim()) return
    if (await run("add", () => api.addSshKey(key.trim()))) {
      toast.success("Public key added")
      setKey("")
    }
  }

  const host = window.location.hostname
  const user = status?.user ?? "root"

  return (
    <Card className={className}>
      <CardHeader>
        <CardTitle className="flex items-center gap-2">
          <KeyRoundIcon className="size-4" />
          SSH access
        </CardTitle>
        <CardDescription>
          Shell on port {SSH_PORT}:{" "}
          <code className="font-mono text-xs">
            ssh -p {SSH_PORT} {user}@{host}
          </code>
          . Public keys are safer than a password; you can use both.
        </CardDescription>
      </CardHeader>
      <CardContent className="flex flex-col gap-6">
        {!status && !error && <Skeleton className="h-40 w-full" />}
        {!!error && !status && (
          <p className="text-sm text-destructive">{errorMessage(error)}</p>
        )}
        {status && (
          <>
            <form onSubmit={savePassword} aria-busy={pending === "password"}>
              <FieldGroup key={fieldsKey}>
                <div className="flex items-center justify-between gap-2 text-sm">
                  <span className="font-medium">Password for {user}</span>
                  <Badge
                    variant={status.password_set ? "secondary" : "outline"}
                  >
                    {status.password_set ? "set" : "not set"}
                  </Badge>
                </div>
                <NewPasswordFields
                  idPrefix="ssh"
                  label={status.password_set ? "New password" : "Password"}
                  onChange={setPassword}
                  disabled={!!pending}
                />
                <FieldDescription>At least 8 characters.</FieldDescription>
                <Field orientation="horizontal">
                  <Button
                    type="submit"
                    variant="outline"
                    disabled={!!pending || !password || password.length < 8}
                  >
                    {pending === "password" && (
                      <Loader2Icon className="animate-spin" />
                    )}
                    {status.password_set ? "Change password" : "Set password"}
                  </Button>
                </Field>
              </FieldGroup>
            </form>

            <Separator />

            <div className="flex flex-col gap-3">
              <span className="text-sm font-medium">
                Authorized public keys ({status.keys.length})
              </span>
              {status.keys.length === 0 ? (
                <p className="text-sm text-muted-foreground">
                  No keys yet. Paste the contents of your{" "}
                  <code className="font-mono text-xs">
                    ~/.ssh/id_ed25519.pub
                  </code>{" "}
                  below.
                </p>
              ) : (
                <ul className="flex flex-col divide-y rounded-lg border text-sm">
                  {status.keys.map((k) => (
                    <li
                      key={k.fingerprint}
                      className="flex items-center justify-between gap-2 px-3 py-2"
                    >
                      <div className="min-w-0">
                        <div className="truncate font-medium">
                          {k.comment || k.algorithm}
                        </div>
                        <div className="truncate font-mono text-xs text-muted-foreground">
                          {k.algorithm} {k.fingerprint}
                        </div>
                      </div>
                      <Button
                        variant="ghost"
                        size="icon"
                        aria-label={`Remove key ${k.comment || k.fingerprint}`}
                        disabled={!!pending}
                        onClick={() =>
                          run("remove", () => api.removeSshKey(k.fingerprint))
                        }
                      >
                        <Trash2Icon />
                      </Button>
                    </li>
                  ))}
                </ul>
              )}
              <form onSubmit={addKey} aria-busy={pending === "add"}>
                <FieldGroup>
                  <Field>
                    <FieldLabel htmlFor="ssh-key">Add a public key</FieldLabel>
                    <textarea
                      id="ssh-key"
                      rows={3}
                      value={key}
                      onChange={(e) => setKey(e.target.value)}
                      placeholder="ssh-ed25519 AAAAC3Nza… you@laptop"
                      spellCheck={false}
                      autoCapitalize="none"
                      disabled={!!pending}
                      className={cn(
                        "w-full min-w-0 rounded-lg border border-input bg-transparent px-2.5 py-2 font-mono text-xs break-all outline-none",
                        "focus-visible:border-ring focus-visible:ring-3 focus-visible:ring-ring/50 disabled:opacity-50"
                      )}
                    />
                    <FieldDescription>
                      ssh-ed25519, ecdsa-sha2-* or ssh-rsa, one key. Create one
                      with{" "}
                      <code className="font-mono">ssh-keygen -t ed25519</code>.
                    </FieldDescription>
                  </Field>
                  <Field orientation="horizontal">
                    <Button
                      type="submit"
                      variant="outline"
                      disabled={!!pending || !key.trim()}
                    >
                      {pending === "add" && (
                        <Loader2Icon className="animate-spin" />
                      )}
                      Add key
                    </Button>
                  </Field>
                </FieldGroup>
              </form>
            </div>
          </>
        )}
      </CardContent>
    </Card>
  )
}
