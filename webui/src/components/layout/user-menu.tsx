import * as React from "react"
import { KeyRoundIcon, LogOutIcon, UserIcon } from "lucide-react"

import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu"
import { ChangePasswordDialog } from "@/components/users/change-password-dialog"
import { useAuth } from "@/lib/auth"

export function UserMenu() {
  const { user, role, session, logout } = useAuth()
  const expires = session ? new Date(session.expiresAt) : null
  const [pwOpen, setPwOpen] = React.useState(false)

  return (
    <>
      <DropdownMenu>
        <DropdownMenuTrigger asChild>
          <Button variant="ghost" className="gap-2">
            <UserIcon />
            <span className="max-w-32 truncate">{user}</span>
            <Badge variant={role === "admin" ? "default" : "secondary"}>
              {role}
            </Badge>
          </Button>
        </DropdownMenuTrigger>
        <DropdownMenuContent align="end" className="min-w-48">
          <DropdownMenuLabel className="font-normal">
            <div className="text-sm font-medium">{user}</div>
            {expires && (
              <div className="text-xs text-muted-foreground">
                Session until {expires.toLocaleTimeString()}
              </div>
            )}
          </DropdownMenuLabel>
          <DropdownMenuSeparator />
          <DropdownMenuItem onSelect={() => setPwOpen(true)}>
            <KeyRoundIcon />
            Change my password
          </DropdownMenuItem>
          <DropdownMenuItem onSelect={logout}>
            <LogOutIcon />
            Log out
          </DropdownMenuItem>
        </DropdownMenuContent>
      </DropdownMenu>
      <ChangePasswordDialog open={pwOpen} onOpenChange={setPwOpen} />
    </>
  )
}
