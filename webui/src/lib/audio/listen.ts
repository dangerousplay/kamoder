/**
 * Plays the camera microphone (`/api/audio/listen`: 8 kHz mono S16LE, one
 * 20 ms frame per message) through Web Audio.
 *
 * With AudioWorklet (secure contexts) frames go to `listen-worklet.ts`
 * (jitter buffer + upsampling). Over plain HTTP `audioWorklet` does not
 * exist, so frames are scheduled as 8 kHz AudioBuffers instead, with the same
 * ~200 ms lead and 500 ms cap; the browser resamples them.
 */

import { errorMessage } from "@/lib/api"
import { closeMessage, endSocket, openAudioSocket } from "@/lib/audio/socket"
import listenWorkletUrl from "@/lib/audio/listen-worklet.ts?worker&url"

const RATE = 8000
/** Lead of the scheduled fallback after (re)starting, seconds. */
const LEAD_S = 0.2
/** Frames scheduled further ahead than this are dropped, seconds. */
const MAX_AHEAD_S = 0.5

export interface ListenEvents {
  /** First frame is about to play. */
  onPlaying: () => void
  /** Ended by the server or an error; `message` null for a normal end. */
  onEnd: (message: string | null) => void
}

export class Listener {
  private readonly ctx: AudioContext
  private readonly events: ListenEvents
  private ws: WebSocket | null = null
  private node: AudioWorkletNode | null = null
  /** Fallback: context time the next buffer starts at. */
  private next = 0
  private stopped = false
  private playing = false

  /** Call from the click handler: the context must start in a user gesture. */
  constructor(events: ListenEvents) {
    this.events = events
    this.ctx = new AudioContext()
    void this.ctx.resume()
    void this.start()
  }

  private async start() {
    try {
      const worklet = (this.ctx as Partial<BaseAudioContext>).audioWorklet
      if (worklet) {
        await worklet.addModule(listenWorkletUrl)
        if (this.stopped) return
        this.node = new AudioWorkletNode(this.ctx, "kamoder-listen", {
          numberOfInputs: 0,
          numberOfOutputs: 1,
          outputChannelCount: [1],
        })
        this.node.connect(this.ctx.destination)
      }
      const ws = await openAudioSocket("listen")
      if (this.stopped) {
        endSocket(ws)
        return
      }
      this.ws = ws
      let opened = false
      ws.onopen = () => {
        opened = true
      }
      ws.onmessage = (e: MessageEvent<ArrayBuffer | string>) => {
        if (typeof e.data === "string") return
        if (!this.playing) {
          this.playing = true
          this.events.onPlaying()
        }
        this.play(e.data)
      }
      ws.onclose = (e) => this.finish(closeMessage(e.code, opened))
    } catch (e) {
      this.finish(`Listen: ${errorMessage(e)}`)
    }
  }

  private play(data: ArrayBuffer) {
    if (this.node) {
      this.node.port.postMessage(data, [data])
      return
    }
    const pcm = new Int16Array(data, 0, data.byteLength >> 1)
    if (pcm.length === 0) return
    const now = this.ctx.currentTime
    if (this.next < now) this.next = now + LEAD_S
    if (this.next - now > MAX_AHEAD_S) return
    const buffer = this.ctx.createBuffer(1, pcm.length, RATE)
    const ch = buffer.getChannelData(0)
    for (let i = 0; i < pcm.length; i++) ch[i] = pcm[i] / 32768
    const src = this.ctx.createBufferSource()
    src.buffer = buffer
    src.connect(this.ctx.destination)
    src.start(this.next)
    this.next += pcm.length / RATE
  }

  private finish(message: string | null) {
    if (this.stopped) return
    this.release()
    this.events.onEnd(message)
  }

  private release() {
    this.stopped = true
    endSocket(this.ws)
    this.ws = null
    this.node?.disconnect()
    this.node = null
    void this.ctx.close().catch(() => {})
  }

  /** Stops on the user's request (no `onEnd`). */
  stop() {
    if (!this.stopped) this.release()
  }
}
