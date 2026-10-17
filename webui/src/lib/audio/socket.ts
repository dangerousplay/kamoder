/**
 * Shared bits of the audio WebSockets (`/api/audio/talk`, `/api/audio/listen`;
 * see crates/web/src/audio.rs). The server only ever sends close frames to
 * report a failure; these turn them into messages for the user.
 */

import { api, audioSocketUrl, type StreamToken } from "@/lib/api"

/** Close codes of the audio WebSockets. */
export const CLOSE = {
  normal: 1000,
  shutdown: 1001,
  /** No frame from the browser for 10 s (talk). */
  idle: 4408,
  /** Another talker holds the speaker. */
  busy: 4409,
  /** Listener limit (3) reached. */
  tooMany: 4429,
  /** The audio service is down or failed. */
  unavailable: 4503,
} as const

/**
 * Why an audio socket ended, for a toast; `null` when it ended normally.
 * `opened` is false when the upgrade itself failed (signed out, not an
 * admin, server unreachable): browsers hide the HTTP status then.
 */
export function closeMessage(code: number, opened: boolean): string | null {
  switch (code) {
    case CLOSE.normal:
      return null
    case CLOSE.shutdown:
      return "The camera web server is restarting"
    case CLOSE.idle:
      return "Talk stopped: no audio was sent for 10 s"
    case CLOSE.busy:
      return "The speaker is busy: someone else is talking"
    case CLOSE.tooMany:
      return "Too many people are listening (3 at most)"
    case CLOSE.unavailable:
      return "Audio unavailable: the audio service is not running"
    default:
      return opened
        ? "Connection to the camera lost"
        : "Could not connect to the camera (signed out or not allowed?)"
  }
}

/** Opens `/api/audio/{kind}` with a fresh (or already requested) token. */
export async function openAudioSocket(
  kind: "talk" | "listen",
  streamToken: Promise<StreamToken> = api.logStreamToken()
): Promise<WebSocket> {
  const { token } = await streamToken
  const ws = new WebSocket(audioSocketUrl(kind, token))
  ws.binaryType = "arraybuffer"
  return ws
}

/** Asks the server to stop (text "end"), then closes. Safe in any state. */
export function endSocket(ws: WebSocket | null) {
  if (!ws) return
  ws.onopen = null
  ws.onmessage = null
  ws.onclose = null
  ws.onerror = null
  if (ws.readyState === WebSocket.OPEN) {
    try {
      ws.send("end")
    } catch {
      // Closing anyway.
    }
  }
  if (
    ws.readyState === WebSocket.OPEN ||
    ws.readyState === WebSocket.CONNECTING
  )
    ws.close(CLOSE.normal)
}
