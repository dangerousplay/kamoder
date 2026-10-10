import * as React from "react"
import { CameraIcon, Loader2Icon } from "lucide-react"
import { Navigate, useLocation, useNavigate } from "react-router"
import { toast } from "sonner"

import { ThemeToggle } from "@/components/layout/theme-toggle"
import { Button } from "@/components/ui/button"
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card"
import { Field, FieldGroup, FieldLabel } from "@/components/ui/field"
import { Input } from "@/components/ui/input"
import { ApiError, errorMessage } from "@/lib/api"
import { useAuth } from "@/lib/auth"

function loginErrorMessage(e: unknown): string {
  if (e instanceof ApiError) {
    if (e.status === 401) return "Invalid username or password"
    if (e.status === 429)
      return "Too many attempts. Wait a few minutes and try again."
  }
  return errorMessage(e)
}

export default function LoginPage() {
  const { session, login } = useAuth()
  const navigate = useNavigate()
  const location = useLocation()
  const from = (location.state as { from?: string } | null)?.from ?? "/"
  const [pending, setPending] = React.useState(false)

  if (session) return <Navigate to={from} replace />

  async function onSubmit(e: React.FormEvent<HTMLFormElement>) {
    e.preventDefault()
    const data = new FormData(e.currentTarget)
    const user = String(data.get("user") ?? "").trim()
    const password = String(data.get("password") ?? "")
    if (!user || !password) return
    setPending(true)
    try {
      await login(user, password)
      navigate(from, { replace: true })
    } catch (err) {
      toast.error(loginErrorMessage(err))
      setPending(false)
    }
  }

  return (
    <div className="relative flex min-h-svh items-center justify-center bg-muted/40 p-4">
      <div className="absolute top-4 right-4">
        <ThemeToggle />
      </div>
      <Card className="w-full max-w-sm">
        <CardHeader>
          <div className="mb-2 flex size-10 items-center justify-center rounded-lg bg-primary text-primary-foreground">
            <CameraIcon className="size-5" />
          </div>
          <CardTitle>Sign in</CardTitle>
          <CardDescription>Anyka camera administration</CardDescription>
        </CardHeader>
        <CardContent>
          <form onSubmit={onSubmit} aria-busy={pending}>
            <FieldGroup>
              <Field>
                <FieldLabel htmlFor="user">Username</FieldLabel>
                <Input
                  id="user"
                  name="user"
                  autoComplete="username"
                  autoCapitalize="none"
                  spellCheck={false}
                  required
                  autoFocus
                  disabled={pending}
                />
              </Field>
              <Field>
                <FieldLabel htmlFor="password">Password</FieldLabel>
                <Input
                  id="password"
                  name="password"
                  type="password"
                  autoComplete="current-password"
                  required
                  disabled={pending}
                />
              </Field>
              <Field>
                <Button type="submit" disabled={pending}>
                  {pending && <Loader2Icon className="animate-spin" />}
                  {pending ? "Signing in..." : "Sign in"}
                </Button>
                {pending && (
                  <p
                    className="text-center text-xs text-muted-foreground"
                    role="status"
                  >
                    Verifying credentials on the camera, this takes a second or
                    two.
                  </p>
                )}
              </Field>
            </FieldGroup>
          </form>
        </CardContent>
      </Card>
    </div>
  )
}
