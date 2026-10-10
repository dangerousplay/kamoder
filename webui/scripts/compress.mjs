// Precompress dist/ for the camera (served from SD by web, design D6):
// writes <file>.gz and <file>.br next to each compressible asset.
import { readdir, readFile, stat, writeFile } from "node:fs/promises"
import { join, relative } from "node:path"
import { fileURLToPath } from "node:url"
import { brotliCompressSync, constants, gzipSync } from "node:zlib"

const dist = fileURLToPath(new URL("../dist/", import.meta.url))
const COMPRESSIBLE = /\.(html|js|mjs|css|svg|json|txt|map|webmanifest|ico)$/i

async function* walk(dir) {
  for (const entry of await readdir(dir, { withFileTypes: true })) {
    const p = join(dir, entry.name)
    if (entry.isDirectory()) yield* walk(p)
    else yield p
  }
}

let raw = 0
let gz = 0
let br = 0
for await (const file of walk(dist)) {
  if (!COMPRESSIBLE.test(file)) continue
  const data = await readFile(file)
  const g = gzipSync(data, { level: 9 })
  const b = brotliCompressSync(data, {
    params: {
      [constants.BROTLI_PARAM_QUALITY]: constants.BROTLI_MAX_QUALITY,
      [constants.BROTLI_PARAM_SIZE_HINT]: data.length,
    },
  })
  await writeFile(file + ".gz", g)
  await writeFile(file + ".br", b)
  raw += data.length
  gz += g.length
  br += b.length
  console.log(
    `${relative(dist, file).padEnd(40)} ${String(data.length).padStart(8)}  gz ${String(g.length).padStart(7)}  br ${String(b.length).padStart(7)}`
  )
}
const kb = (n) => (n / 1024).toFixed(1) + " KiB"
console.log(`total raw ${kb(raw)}  gz ${kb(gz)}  br ${kb(br)}`)
if (!(await stat(join(dist, "index.html")).catch(() => null))) {
  console.error("dist/index.html missing")
  process.exit(1)
}
