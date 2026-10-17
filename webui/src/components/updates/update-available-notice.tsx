import { ArrowUpCircleIcon } from "lucide-react"
import { Link } from "react-router"

import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert"
import { usePolling } from "@/hooks/use-polling"
import { api } from "@/lib/api"

/** Dashboard hint when the update service knows a newer release. */
export function UpdateAvailableNotice() {
  // Fetched once; errors (service down, old firmware) just hide the notice.
  const { data } = usePolling(api.updateStatus, null)
  const a = data?.available
  if (!a || a.downgrade) return null
  return (
    <Alert>
      <ArrowUpCircleIcon />
      <AlertTitle>Update available: {a.version}</AlertTitle>
      <AlertDescription>
        <span>
          {a.downloaded ? "Downloaded and ready to install. " : ""}
          <Link to="/updates" className="underline underline-offset-4">
            View on the Updates page
          </Link>
        </span>
      </AlertDescription>
    </Alert>
  )
}
