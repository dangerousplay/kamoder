/**
 * AudioWorklet: plays 8 kHz mono S16LE frames from `/api/audio/listen`.
 *
 * The main thread posts each frame's ArrayBuffer (transferred). Samples go
 * into a ring buffer; playback starts once ~200 ms are queued (jitter
 * buffer) and goes back to buffering on underrun. When more than 500 ms pile
 * up (tab throttled, network burst) the oldest are dropped so latency stays
 * bounded. Output is upsampled to the context rate by linear interpolation.
 */

// AudioWorkletGlobalScope (not in lib.dom).
declare const sampleRate: number
declare class AudioWorkletProcessor {
  readonly port: MessagePort
}
declare function registerProcessor(
  name: string,
  ctor: new () => AudioWorkletProcessor
): void

const IN_RATE = 8000
/** Ring size (power of two, ~1 s). */
const CAPACITY = 8192
const MASK = CAPACITY - 1
/** Queued samples needed before (re)starting: 200 ms. */
const TARGET = (IN_RATE * 200) / 1000
/** Above this backlog (500 ms) the oldest samples are dropped to TARGET. */
const MAX_BACKLOG = (IN_RATE * 500) / 1000

class ListenProcessor extends AudioWorkletProcessor {
  private readonly ring = new Float32Array(CAPACITY)
  /** Samples written / read so far (read = index of the left neighbour). */
  private written = 0
  private read = 0
  /** Position between `read` and `read + 1`. */
  private frac = 0
  private playing = false
  /** Input samples per output sample. */
  private readonly step = IN_RATE / sampleRate

  constructor() {
    super()
    this.port.onmessage = (e: MessageEvent<ArrayBuffer>) => {
      if (!(e.data instanceof ArrayBuffer)) return
      const pcm = new Int16Array(e.data, 0, e.data.byteLength >> 1)
      for (const s of pcm) this.ring[this.written++ & MASK] = s / 32768
      if (this.written - this.read > MAX_BACKLOG) {
        this.read = this.written - TARGET
        this.frac = 0
      }
    }
  }

  process(_inputs: Float32Array[][], outputs: Float32Array[][]): boolean {
    const channels = outputs[0]
    if (!channels || channels.length === 0) return true
    const out = channels[0]
    for (let i = 0; i < out.length; i++) {
      const queued = this.written - this.read
      if (!this.playing && queued >= TARGET) this.playing = true
      if (!this.playing || queued < 2) {
        this.playing = false
        out[i] = 0
        continue
      }
      const a = this.ring[this.read & MASK]
      const b = this.ring[(this.read + 1) & MASK]
      out[i] = a + (b - a) * this.frac
      this.frac += this.step
      while (this.frac >= 1) {
        this.frac -= 1
        this.read++
      }
    }
    for (let c = 1; c < channels.length; c++) channels[c].set(out)
    return true
  }
}

registerProcessor("kamoder-listen", ListenProcessor)
