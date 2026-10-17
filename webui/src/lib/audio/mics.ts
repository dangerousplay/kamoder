/**
 * The browser's microphones, for push-to-talk's input picker.
 *
 * Labels are empty until the page has been granted the microphone once, so
 * the list is re-read after every successful `getUserMedia` and on
 * `devicechange`. The choice is kept in localStorage per browser; an id
 * that no longer exists falls back to the system default.
 */

const STORAGE_KEY = "kamoder.talk.mic"

export interface Mic {
  deviceId: string
  label: string
}

export async function listMics(): Promise<Mic[]> {
  if (!navigator.mediaDevices?.enumerateDevices) return []
  const devices = await navigator.mediaDevices.enumerateDevices()
  return devices
    .filter((d) => d.kind === "audioinput" && d.deviceId)
    .map((d, i) => ({
      deviceId: d.deviceId,
      label: d.label || `Microphone ${i + 1}`,
    }))
}

/** The saved microphone id, or null for the system default. */
export function savedMic(): string | null {
  try {
    return localStorage.getItem(STORAGE_KEY)
  } catch {
    return null
  }
}

export function saveMic(deviceId: string | null) {
  try {
    if (deviceId) localStorage.setItem(STORAGE_KEY, deviceId)
    else localStorage.removeItem(STORAGE_KEY)
  } catch {
    // Private mode: the choice lasts for this page only.
  }
}
