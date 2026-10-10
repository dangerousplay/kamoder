/* eslint-disable react-refresh/only-export-components */
import * as React from "react"
import { Navigate, useLocation } from "react-router"

import {
  api,
  clearToken,
  getToken,
  getTokenExpiry,
  setToken,
  UNAUTHORIZED_EVENT,
  type Role,
} from "@/lib/api"

export interface Session {
  token: string
  user: string
  role: Role
  /**
   * Expiry in browser milliseconds (Date.now() scale). Not the JWT `exp`:
   * the camera has no RTC and signs tokens with a clock that reads 1970, so
   * only the token's lifetime (exp - iat) is meaningful here.
   */
  expiresAt: number
}

interface AuthState {
  session: Session | null
  user: string | null
  role: Role | null
  isAdmin: boolean
  login: (user: string, password: string) => Promise<Session>
  /** Replace the session with a token issued by the server (password change). */
  adoptToken: (token: string) => void
  logout: () => void
}

interface JwtClaims {
  sub?: string
  role?: string
  exp?: number
  iat?: number
}

function decodeClaims(token: string): JwtClaims | null {
  const part = token.split(".")[1]
  if (!part) return null
  try {
    const b64 = part.replace(/-/g, "+").replace(/_/g, "/")
    const padded = b64 + "=".repeat((4 - (b64.length % 4)) % 4)
    const bytes = Uint8Array.from(atob(padded), (c) => c.charCodeAt(0))
    return JSON.parse(new TextDecoder().decode(bytes)) as JwtClaims
  } catch {
    return null
  }
}

/**
 * Builds a session from a token; null when malformed or expired.
 *
 * `expiresAt` is the stored browser-clock deadline for a token restored from
 * storage; for a freshly issued token it is derived from the lifetime.
 */
function sessionFromToken(
  token: string | null,
  expiresAt?: number | null
): Session | null {
  if (!token) return null
  const c = decodeClaims(token)
  if (!c?.sub || typeof c.exp !== "number") return null
  if (c.role !== "admin" && c.role !== "viewer") return null
  let deadline = expiresAt ?? null
  if (deadline === null) {
    const lifetime = typeof c.iat === "number" ? c.exp - c.iat : 0
    if (lifetime <= 0) return null
    deadline = Date.now() + lifetime * 1000
  }
  if (deadline <= Date.now()) return null
  return { token, user: c.sub, role: c.role, expiresAt: deadline }
}

const AuthContext = React.createContext<AuthState | undefined>(undefined)

export function AuthProvider({ children }: { children: React.ReactNode }) {
  const [session, setSession] = React.useState<Session | null>(() => {
    const s = sessionFromToken(getToken(), getTokenExpiry())
    if (!s) clearToken()
    return s
  })

  const logout = React.useCallback(() => {
    clearToken()
    setSession(null)
  }, [])

  const login = React.useCallback(async (user: string, password: string) => {
    const res = await api.login({ user, password })
    const s = sessionFromToken(res.token)
    if (!s) throw new Error("the camera returned an unusable session token")
    setToken(s.token, s.expiresAt)
    setSession(s)
    return s
  }, [])

  const adoptToken = React.useCallback((token: string) => {
    const s = sessionFromToken(token)
    if (!s) return
    setToken(token, s.expiresAt)
    setSession(s)
  }, [])

  // 401 from any API call: drop the session; RequireAuth redirects.
  React.useEffect(() => {
    const onUnauthorized = (e: Event) => {
      e.preventDefault()
      setSession(null)
    }
    window.addEventListener(UNAUTHORIZED_EVENT, onUnauthorized)
    return () => window.removeEventListener(UNAUTHORIZED_EVENT, onUnauthorized)
  }, [])

  // Auto-logout when the JWT expires.
  React.useEffect(() => {
    if (!session) return
    const ms = session.expiresAt - Date.now()
    // setTimeout overflows above ~24.8 days; tokens live <= 12 h anyway.
    const id = window.setTimeout(logout, Math.max(0, Math.min(ms, 2 ** 31 - 1)))
    return () => window.clearTimeout(id)
  }, [session, logout])

  const value = React.useMemo<AuthState>(
    () => ({
      session,
      user: session?.user ?? null,
      role: session?.role ?? null,
      isAdmin: session?.role === "admin",
      login,
      adoptToken,
      logout,
    }),
    [session, login, adoptToken, logout]
  )

  return <AuthContext.Provider value={value}>{children}</AuthContext.Provider>
}

export function useAuth(): AuthState {
  const ctx = React.useContext(AuthContext)
  if (!ctx) throw new Error("useAuth must be used within an AuthProvider")
  return ctx
}

/** Renders children only with a valid session; otherwise goes to /login. */
export function RequireAuth({ children }: { children: React.ReactNode }) {
  const { session } = useAuth()
  const location = useLocation()
  if (!session) {
    return <Navigate to="/login" replace state={{ from: location.pathname }} />
  }
  return children
}

/** Admin-only routes; viewers are sent to the dashboard. */
export function RequireAdmin({ children }: { children: React.ReactNode }) {
  const { session } = useAuth()
  const location = useLocation()
  if (!session) {
    return <Navigate to="/login" replace state={{ from: location.pathname }} />
  }
  if (session.role !== "admin") return <Navigate to="/" replace />
  return children
}
