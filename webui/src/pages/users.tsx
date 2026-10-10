import * as React from "react"
import {
  KeyRoundIcon,
  Loader2Icon,
  Trash2Icon,
  UserPlusIcon,
} from "lucide-react"
import { toast } from "sonner"

import { ConfirmDialog } from "@/components/common/confirm-dialog"
import { NewPasswordFields } from "@/components/users/password-fields"
import { Badge } from "@/components/ui/badge"
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
import {
  Field,
  FieldDescription,
  FieldError,
  FieldGroup,
  FieldLabel,
} from "@/components/ui/field"
import { Input } from "@/components/ui/input"
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select"
import { Skeleton } from "@/components/ui/skeleton"
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table"
import { usePolling } from "@/hooks/use-polling"
import { api, errorMessage, type Role, type User } from "@/lib/api"
import { useAuth } from "@/lib/auth"

// `:` separates fields in etc/web-users; keep names shell/URL friendly.
const NAME_RE = /^[A-Za-z0-9._-]{1,32}$/

function RoleSelect({
  id,
  value,
  onChange,
  disabled,
  label,
}: {
  id?: string
  value: Role
  onChange: (r: Role) => void
  disabled?: boolean
  label?: string
}) {
  return (
    <Select
      value={value}
      onValueChange={(v) => onChange(v as Role)}
      disabled={disabled}
    >
      <SelectTrigger id={id} size="sm" className="w-28" aria-label={label}>
        <SelectValue />
      </SelectTrigger>
      <SelectContent>
        <SelectItem value="admin">admin</SelectItem>
        <SelectItem value="viewer">viewer</SelectItem>
      </SelectContent>
    </Select>
  )
}

function AddUserDialog({
  open,
  onOpenChange,
  existing,
  onAdded,
}: {
  open: boolean
  onOpenChange: (o: boolean) => void
  existing: string[]
  onAdded: () => void
}) {
  const [name, setName] = React.useState("")
  const [role, setRole] = React.useState<Role>("viewer")
  const [password, setPassword] = React.useState<string | null>(null)
  const [pending, setPending] = React.useState(false)

  const nameError = !name
    ? null
    : !NAME_RE.test(name)
      ? "1–32 letters, digits, dot, dash or underscore"
      : existing.includes(name)
        ? "A user with this name already exists"
        : null

  function reset() {
    setName("")
    setRole("viewer")
    setPassword(null)
  }

  async function onSubmit(e: React.FormEvent) {
    e.preventDefault()
    if (!name || nameError || !password) return
    setPending(true)
    try {
      await api.createUser({ name, role, password })
      toast.success(`User ${name} added`)
      onAdded()
      reset()
      setPending(false)
      onOpenChange(false)
    } catch (err) {
      toast.error(`Add user failed: ${errorMessage(err)}`)
      setPending(false)
    }
  }

  return (
    <Dialog
      open={open}
      onOpenChange={(o) => {
        if (pending) return
        if (!o) reset()
        onOpenChange(o)
      }}
    >
      <DialogContent className="sm:max-w-sm">
        <form onSubmit={onSubmit} className="flex flex-col gap-4">
          <DialogHeader>
            <DialogTitle>Add user</DialogTitle>
            <DialogDescription>
              Web UI account. Viewers can see status, logs, video and PTZ
              position; admins can change everything.
            </DialogDescription>
          </DialogHeader>
          <FieldGroup>
            <Field data-invalid={!!nameError || undefined}>
              <FieldLabel htmlFor="new-user-name">Name</FieldLabel>
              <Input
                id="new-user-name"
                value={name}
                onChange={(e) => setName(e.target.value.trim())}
                autoComplete="off"
                autoCapitalize="none"
                spellCheck={false}
                required
                autoFocus
                aria-invalid={!!nameError || undefined}
                disabled={pending}
              />
              {nameError && <FieldError>{nameError}</FieldError>}
            </Field>
            <Field>
              <FieldLabel htmlFor="new-user-role">Role</FieldLabel>
              <RoleSelect
                id="new-user-role"
                value={role}
                onChange={setRole}
                disabled={pending}
              />
            </Field>
            <NewPasswordFields
              idPrefix="new-user"
              label="Password"
              onChange={setPassword}
              disabled={pending}
            />
          </FieldGroup>
          <DialogFooter>
            <Button
              type="submit"
              disabled={pending || !name || !!nameError || !password}
            >
              {pending && <Loader2Icon className="animate-spin" />}
              Add user
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  )
}

function ResetPasswordDialog({
  user,
  onOpenChange,
}: {
  user: User | null
  onOpenChange: (o: boolean) => void
}) {
  const [password, setPassword] = React.useState<string | null>(null)
  const [pending, setPending] = React.useState(false)

  async function onSubmit(e: React.FormEvent) {
    e.preventDefault()
    if (!user || !password) return
    setPending(true)
    try {
      await api.updateUser(user.name, { password })
      toast.success(`Password of ${user.name} reset`)
      setPending(false)
      setPassword(null)
      onOpenChange(false)
    } catch (err) {
      toast.error(`Reset failed: ${errorMessage(err)}`)
      setPending(false)
    }
  }

  return (
    <Dialog
      open={user !== null}
      onOpenChange={(o) => {
        if (pending) return
        if (!o) setPassword(null)
        onOpenChange(o)
      }}
    >
      <DialogContent className="sm:max-w-sm">
        <form onSubmit={onSubmit} className="flex flex-col gap-4">
          <DialogHeader>
            <DialogTitle>Reset password</DialogTitle>
            <DialogDescription>
              Sets a new password for <strong>{user?.name}</strong>. Their
              current sessions end.
            </DialogDescription>
          </DialogHeader>
          <FieldGroup>
            <NewPasswordFields
              idPrefix="reset"
              onChange={setPassword}
              disabled={pending}
            />
          </FieldGroup>
          <DialogFooter>
            <Button type="submit" disabled={pending || !password}>
              {pending && <Loader2Icon className="animate-spin" />}
              Reset password
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  )
}

export default function UsersPage() {
  const { user: me, logout } = useAuth()
  const { data, error, loading, refresh, setData } = usePolling(api.users, null)
  const [adding, setAdding] = React.useState(false)
  const [resetting, setResetting] = React.useState<User | null>(null)
  const [deleting, setDeleting] = React.useState<User | null>(null)
  const [busy, setBusy] = React.useState<string | null>(null)

  const admins = data?.filter((u) => u.role === "admin").length ?? 0

  async function changeRole(u: User, role: Role) {
    if (role === u.role) return
    setBusy(u.name)
    try {
      const updated = await api.updateUser(u.name, { role })
      setData((list) =>
        list?.map((x) =>
          x.name === u.name ? { ...x, ...(updated ?? { role }) } : x
        )
      )
      toast.success(`${u.name} is now ${role}`)
      // Demoting yourself: this session lost admin rights.
      if (u.name === me && role !== "admin") logout()
    } catch (err) {
      toast.error(`Change role failed: ${errorMessage(err)}`)
    } finally {
      setBusy(null)
      refresh()
    }
  }

  async function remove(u: User) {
    try {
      await api.deleteUser(u.name)
      toast.success(`User ${u.name} deleted`)
      if (u.name === me) {
        logout()
        return
      }
      refresh()
    } catch (err) {
      toast.error(`Delete failed: ${errorMessage(err)}`)
      throw err
    }
  }

  return (
    <Card>
      <CardHeader>
        <CardTitle>Web users</CardTitle>
        <CardDescription>
          Accounts for this web UI. SSH and ONVIF have their own credentials.
        </CardDescription>
        <CardAction>
          <Button size="sm" onClick={() => setAdding(true)}>
            <UserPlusIcon />
            Add user
          </Button>
        </CardAction>
      </CardHeader>
      <CardContent className="px-0">
        <Table>
          <TableHeader>
            <TableRow>
              <TableHead className="pl-4">Name</TableHead>
              <TableHead>Role</TableHead>
              <TableHead className="pr-4 text-right">
                <span className="sr-only">Actions</span>
              </TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {loading &&
              !data &&
              Array.from({ length: 3 }, (_, i) => (
                <TableRow key={i}>
                  <TableCell colSpan={3} className="px-4">
                    <Skeleton className="h-6 w-full" />
                  </TableCell>
                </TableRow>
              ))}
            {!loading && !data && (
              <TableRow>
                <TableCell
                  colSpan={3}
                  className="py-8 text-center text-destructive"
                >
                  {errorMessage(error)}
                </TableCell>
              </TableRow>
            )}
            {data?.length === 0 && (
              <TableRow>
                <TableCell
                  colSpan={3}
                  className="py-8 text-center text-muted-foreground"
                >
                  No users.
                </TableCell>
              </TableRow>
            )}
            {data?.map((u) => (
              <TableRow key={u.name}>
                <TableCell className="pl-4 font-medium">
                  {u.name}
                  {u.name === me && (
                    <Badge variant="outline" className="ml-2">
                      you
                    </Badge>
                  )}
                </TableCell>
                <TableCell>
                  <RoleSelect
                    value={u.role}
                    onChange={(r) => void changeRole(u, r)}
                    disabled={busy === u.name}
                    label={`Role of ${u.name}`}
                  />
                </TableCell>
                <TableCell className="pr-4">
                  <div className="flex justify-end gap-1">
                    <Button
                      variant="ghost"
                      size="icon-sm"
                      onClick={() => setResetting(u)}
                      aria-label={`Reset password of ${u.name}`}
                      title="Reset password"
                    >
                      <KeyRoundIcon />
                    </Button>
                    <Button
                      variant="ghost"
                      size="icon-sm"
                      className="text-destructive"
                      onClick={() => setDeleting(u)}
                      aria-label={`Delete ${u.name}`}
                      title="Delete"
                    >
                      <Trash2Icon />
                    </Button>
                  </div>
                </TableCell>
              </TableRow>
            ))}
          </TableBody>
        </Table>
        {data && admins === 1 && (
          <FieldDescription className="px-4 pt-3">
            The last admin cannot be deleted or demoted.
          </FieldDescription>
        )}
      </CardContent>

      <AddUserDialog
        open={adding}
        onOpenChange={setAdding}
        existing={data?.map((u) => u.name) ?? []}
        onAdded={refresh}
      />
      <ResetPasswordDialog
        user={resetting}
        onOpenChange={(o) => !o && setResetting(null)}
      />
      <ConfirmDialog
        open={deleting !== null}
        onOpenChange={(o) => !o && setDeleting(null)}
        title={`Delete user ${deleting?.name ?? ""}?`}
        description={
          deleting?.name === me
            ? "This is your own account: you will be logged out."
            : "Their sessions end immediately."
        }
        confirmLabel="Delete"
        destructive
        onConfirm={() => (deleting ? remove(deleting) : undefined)}
      />
    </Card>
  )
}
