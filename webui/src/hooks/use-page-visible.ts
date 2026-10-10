import * as React from "react"

function subscribe(cb: () => void) {
  document.addEventListener("visibilitychange", cb)
  return () => document.removeEventListener("visibilitychange", cb)
}

/** False while the browser tab is hidden. */
export function usePageVisible(): boolean {
  return React.useSyncExternalStore(
    subscribe,
    () => !document.hidden,
    () => true
  )
}
