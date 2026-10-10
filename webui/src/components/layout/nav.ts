import {
  ActivityIcon,
  LayoutDashboardIcon,
  MoveIcon,
  ScrollTextIcon,
  SettingsIcon,
  UsersIcon,
  type LucideIcon,
} from "lucide-react"

export interface NavItem {
  title: string
  to: string
  icon: LucideIcon
  adminOnly?: boolean
}

export const NAV_ITEMS: NavItem[] = [
  { title: "Dashboard", to: "/", icon: LayoutDashboardIcon },
  { title: "Services", to: "/services", icon: ActivityIcon },
  { title: "Logs", to: "/logs", icon: ScrollTextIcon },
  { title: "PTZ", to: "/ptz", icon: MoveIcon },
  { title: "Settings", to: "/settings", icon: SettingsIcon, adminOnly: true },
  { title: "Users", to: "/users", icon: UsersIcon, adminOnly: true },
]
