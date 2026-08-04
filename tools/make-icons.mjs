// Generates every raster icon in src-tauri/icons/ from tools/icon-source.png.
//
//   node tools/make-icons.mjs
//   node tools/make-icons.mjs --preview out.png
//
// Run by hand, never by the build. Nothing here is a dependency of anything --
// only `zlib` from the Node standard library, which is what makes it possible
// to keep the promise on the tin: no bundler, no npm packages, no toolchain.
//
// This used to draw the mark procedurally, so that no binary nobody could
// regenerate was ever committed. The artwork is now a raster the app did not
// draw, so that promise is kept a different way: the source PNG lives in the
// repo beside this file, and everything under src-tauri/icons/ is derived from
// it by the code below. Delete the icons and one command puts them back.
//
// `zlib` still does the heavy lifting at both ends -- `inflateSync` to read the
// source, `deflateSync` to write each output.

import { deflateSync, inflateSync } from "node:zlib";
import { writeFileSync, readFileSync, mkdirSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const OUT = join(HERE, "..", "src-tauri", "icons");
const SOURCE = join(HERE, "icon-source.png");

// ---------------------------------------------------------------------------
// PNG decode
// ---------------------------------------------------------------------------

/// Decode a PNG into `{ width, height, rgba }`.
///
/// Deliberately narrow: 8-bit truecolour, with or without alpha, non-interlaced.
/// That is what the source is, and widening this to palettes, 16-bit samples and
/// Adam7 would be a few hundred lines serving a file that does not exist. It
/// throws rather than guessing, so replacing the artwork with something exotic
/// fails loudly here instead of producing quietly wrong icons.
function decodePng(buf) {
  const SIG = [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];
  for (let i = 0; i < SIG.length; i++) {
    if (buf[i] !== SIG[i]) throw new Error("not a PNG");
  }

  let width = 0;
  let height = 0;
  let channels = 0;
  const idat = [];

  // Walk the chunks. Everything that is not IHDR or IDAT is skipped -- the
  // source carries a 29 KB private chunk from the tool that made it.
  for (let at = 8; at + 8 <= buf.length; ) {
    const length = buf.readUInt32BE(at);
    const type = buf.toString("ascii", at + 4, at + 8);
    const body = buf.subarray(at + 8, at + 8 + length);

    if (type === "IHDR") {
      width = body.readUInt32BE(0);
      height = body.readUInt32BE(4);
      const depth = body[8];
      const colour = body[9];
      const interlace = body[12];
      if (depth !== 8) throw new Error(`bit depth ${depth} unsupported, need 8`);
      if (colour !== 2 && colour !== 6) {
        throw new Error(`colour type ${colour} unsupported, need 2 (RGB) or 6 (RGBA)`);
      }
      if (interlace !== 0) throw new Error("interlaced PNG unsupported");
      channels = colour === 6 ? 4 : 3;
    } else if (type === "IDAT") {
      idat.push(body);
    } else if (type === "IEND") {
      break;
    }

    at += 12 + length; // length + type + body + crc
  }
  if (!width || !channels) throw new Error("no IHDR");

  const raw = inflateSync(Buffer.concat(idat));
  const stride = width * channels;
  const rgba = Buffer.alloc(width * height * 4);
  // Reconstructed scanlines, kept because filters reference the row above.
  const line = Buffer.alloc(stride);
  const prev = Buffer.alloc(stride);

  for (let y = 0, at = 0; y < height; y++) {
    const filter = raw[at++];
    raw.copy(line, 0, at, at + stride);
    at += stride;

    // The five filter types from the spec. `a` is the pixel to the left, `b`
    // the one above, `c` above-left -- all zero off the edges.
    for (let i = 0; i < stride; i++) {
      const a = i >= channels ? line[i - channels] : 0;
      const b = prev[i];
      const c = i >= channels ? prev[i - channels] : 0;
      let add = 0;
      switch (filter) {
        case 0: add = 0; break;
        case 1: add = a; break;
        case 2: add = b; break;
        case 3: add = (a + b) >> 1; break;
        case 4: {
          const p = a + b - c;
          const pa = Math.abs(p - a);
          const pb = Math.abs(p - b);
          const pc = Math.abs(p - c);
          add = pa <= pb && pa <= pc ? a : pb <= pc ? b : c;
          break;
        }
        default: throw new Error(`bad filter type ${filter} on row ${y}`);
      }
      line[i] = (line[i] + add) & 0xff;
    }
    line.copy(prev);

    for (let x = 0; x < width; x++) {
      const s = x * channels;
      const d = (y * width + x) * 4;
      rgba[d] = line[s];
      rgba[d + 1] = line[s + 1];
      rgba[d + 2] = line[s + 2];
      rgba[d + 3] = channels === 4 ? line[s + 3] : 0xff;
    }
  }

  return { width, height, rgba };
}

// ---------------------------------------------------------------------------
// Crop + resample
// ---------------------------------------------------------------------------

/// Tightest box containing every pixel that is not fully transparent.
///
/// The artwork is a shape floating in a larger transparent canvas -- roughly a
/// fifth of each axis is empty. Left in, every icon would render a size smaller
/// than its neighbours in the taskbar, which is the whole reason the previous
/// mark ran to the edges of its canvas.
function alphaBounds({ width, height, rgba }) {
  let minX = width;
  let minY = height;
  let maxX = -1;
  let maxY = -1;
  for (let y = 0; y < height; y++) {
    for (let x = 0; x < width; x++) {
      if (rgba[(y * width + x) * 4 + 3] !== 0) {
        if (x < minX) minX = x;
        if (x > maxX) maxX = x;
        if (y < minY) minY = y;
        if (y > maxY) maxY = y;
      }
    }
  }
  if (maxX < 0) throw new Error("source is fully transparent");

  // Square it off around the centre, so a source that is a pixel or two off
  // square is not stretched into one.
  const w = maxX - minX + 1;
  const h = maxY - minY + 1;
  const edge = Math.max(w, h);
  const cx = (minX + maxX + 1) / 2;
  const cy = (minY + maxY + 1) / 2;
  return {
    x: Math.round(cx - edge / 2),
    y: Math.round(cy - edge / 2),
    edge,
  };
}

/// Box-filter downscale of a square region into `size`.
///
/// Averaging every source pixel that lands under a destination pixel, rather
/// than sampling one of them. At 32px each output pixel covers a ~25px box of
/// the source, and picking one sample out of 625 is what makes a downscale
/// look like it has been through a fax machine.
///
/// Alpha-weighted: colour is averaged in proportion to how opaque each
/// contributing pixel is, so the transparent-but-dark pixels around the artwork
/// cannot drag a dark fringe into its edges.
function resample(src, box, size) {
  const out = Buffer.alloc(size * size * 4);
  const step = box.edge / size;

  for (let y = 0; y < size; y++) {
    const sy0 = box.y + y * step;
    const sy1 = sy0 + step;
    for (let x = 0; x < size; x++) {
      const sx0 = box.x + x * step;
      const sx1 = sx0 + step;

      let r = 0;
      let g = 0;
      let b = 0;
      let a = 0;
      let n = 0;

      for (let sy = Math.floor(sy0); sy < Math.ceil(sy1); sy++) {
        if (sy < 0 || sy >= src.height) continue;
        for (let sx = Math.floor(sx0); sx < Math.ceil(sx1); sx++) {
          if (sx < 0 || sx >= src.width) continue;
          const s = (sy * src.width + sx) * 4;
          const alpha = src.rgba[s + 3] / 255;
          r += src.rgba[s] * alpha;
          g += src.rgba[s + 1] * alpha;
          b += src.rgba[s + 2] * alpha;
          a += src.rgba[s + 3];
          n++;
        }
      }

      const d = (y * size + x) * 4;
      if (!n) continue;
      // Un-premultiply: the sum was weighted by alpha, so divide by the weight
      // rather than the count, or every semi-transparent edge comes out dark.
      const weight = a / 255;
      if (weight > 0) {
        out[d] = Math.min(255, Math.round(r / weight));
        out[d + 1] = Math.min(255, Math.round(g / weight));
        out[d + 2] = Math.min(255, Math.round(b / weight));
      }
      out[d + 3] = Math.round(a / n);
    }
  }
  return out;
}

// ---------------------------------------------------------------------------
// PNG encode
// ---------------------------------------------------------------------------

const CRC_TABLE = (() => {
  const table = new Int32Array(256);
  for (let n = 0; n < 256; n++) {
    let c = n;
    for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    table[n] = c;
  }
  return table;
})();

function crc32(buf) {
  let c = 0xffffffff;
  for (let i = 0; i < buf.length; i++) c = CRC_TABLE[(c ^ buf[i]) & 0xff] ^ (c >>> 8);
  return (c ^ 0xffffffff) >>> 0;
}

function chunk(type, data) {
  const out = Buffer.alloc(data.length + 12);
  out.writeUInt32BE(data.length, 0);
  out.write(type, 4, "ascii");
  data.copy(out, 8);
  const crc = crc32(Buffer.concat([Buffer.from(type, "ascii"), data]));
  out.writeUInt32BE(crc, data.length + 8);
  return out;
}

function png2(width, height, rgba) {
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(width, 0);
  ihdr.writeUInt32BE(height, 4);
  ihdr[8] = 8; // bit depth
  ihdr[9] = 6; // truecolour + alpha
  // 10..12: deflate, adaptive filtering, no interlace -- all zero.

  // Adaptive filtering: try all five on each row and keep the one whose output
  // has the smallest sum of absolute signed deviations. That is the heuristic
  // the spec suggests and every real encoder uses; measured on this artwork it
  // is worth about 11% over a fixed Sub filter. Not dramatic -- a photographic
  // gradient is simply not what PNG is good at -- but the icons are embedded in
  // the binary with include_bytes!, so it is binary size, not just disk.
  const stride = width * 4;
  const raw = Buffer.alloc((stride + 1) * height);
  const candidate = Buffer.alloc(stride);
  const best = Buffer.alloc(stride);

  for (let y = 0; y < height; y++) {
    const row = y * stride;
    const above = row - stride;
    let bestScore = Infinity;
    let bestType = 0;

    for (let type = 0; type < 5; type++) {
      let score = 0;
      for (let i = 0; i < stride; i++) {
        const a = i >= 4 ? rgba[row + i - 4] : 0;
        const b = y > 0 ? rgba[above + i] : 0;
        const c = y > 0 && i >= 4 ? rgba[above + i - 4] : 0;
        let predict = 0;
        switch (type) {
          case 0: predict = 0; break;
          case 1: predict = a; break;
          case 2: predict = b; break;
          case 3: predict = (a + b) >> 1; break;
          default: {
            const p = a + b - c;
            const pa = Math.abs(p - a);
            const pb = Math.abs(p - b);
            const pc = Math.abs(p - c);
            predict = pa <= pb && pa <= pc ? a : pb <= pc ? b : c;
          }
        }
        const v = (rgba[row + i] - predict) & 0xff;
        candidate[i] = v;
        // Signed magnitude: 200 is -56, and a byte close to zero either way is
        // what deflate turns into nothing.
        score += v < 128 ? v : 256 - v;
      }
      if (score < bestScore) {
        bestScore = score;
        bestType = type;
        candidate.copy(best);
      }
    }

    const at = y * (stride + 1);
    raw[at] = bestType;
    best.copy(raw, at + 1);
  }

  return Buffer.concat([
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    chunk("IHDR", ihdr),
    chunk("IDAT", deflateSync(raw, { level: 9 })),
    chunk("IEND", Buffer.alloc(0)),
  ]);
}

const png = (size, rgba) => png2(size, size, rgba);

// ---------------------------------------------------------------------------
// ICO
// ---------------------------------------------------------------------------

/// A 32-bit BGRA DIB, bottom-up, with the (unused but mandatory) AND mask.
///
/// Small entries are DIB rather than PNG on purpose: PNG-compressed ICO entries
/// are only reliably honoured at 256x256, and a 16x16 PNG entry is exactly the
/// case old shells render as a black square.
function dib(size, rgba) {
  const rowBytes = size * 4;
  const maskRow = Math.ceil(size / 32) * 4; // AND mask rows pad to 4 bytes
  const header = Buffer.alloc(40);
  header.writeUInt32LE(40, 0);
  header.writeInt32LE(size, 4);
  header.writeInt32LE(size * 2, 8); // XOR + AND stacked
  header.writeUInt16LE(1, 12);
  header.writeUInt16LE(32, 14);
  header.writeUInt32LE(0, 16); // BI_RGB
  header.writeUInt32LE(rowBytes * size + maskRow * size, 20);

  const xor = Buffer.alloc(rowBytes * size);
  for (let y = 0; y < size; y++) {
    const src = (size - 1 - y) * rowBytes; // bottom-up
    for (let x = 0; x < size; x++) {
      const s = src + x * 4;
      const d = y * rowBytes + x * 4;
      xor[d] = rgba[s + 2]; // B
      xor[d + 1] = rgba[s + 1]; // G
      xor[d + 2] = rgba[s]; // R
      xor[d + 3] = rgba[s + 3]; // A
    }
  }

  return Buffer.concat([header, xor, Buffer.alloc(maskRow * size)]);
}

function ico(entries) {
  const header = Buffer.alloc(6);
  header.writeUInt16LE(0, 0);
  header.writeUInt16LE(1, 2); // type: icon
  header.writeUInt16LE(entries.length, 4);

  const dir = Buffer.alloc(16 * entries.length);
  let offset = 6 + dir.length;
  const blobs = [];

  entries.forEach((entry, i) => {
    const at = i * 16;
    // 0 means 256 in the ICO directory.
    dir[at] = entry.size >= 256 ? 0 : entry.size;
    dir[at + 1] = entry.size >= 256 ? 0 : entry.size;
    dir[at + 2] = 0; // palette
    dir[at + 3] = 0;
    dir.writeUInt16LE(1, at + 4); // colour planes
    dir.writeUInt16LE(32, at + 6);
    dir.writeUInt32LE(entry.data.length, at + 8);
    dir.writeUInt32LE(offset, at + 12);
    offset += entry.data.length;
    blobs.push(entry.data);
  });

  return Buffer.concat([header, dir, ...blobs]);
}

// ---------------------------------------------------------------------------
// ICNS
// ---------------------------------------------------------------------------

function icns(entries) {
  const blocks = entries.map(({ type, data }) => {
    const head = Buffer.alloc(8);
    head.write(type, 0, "ascii");
    head.writeUInt32BE(data.length + 8, 4);
    return Buffer.concat([head, data]);
  });
  const body = Buffer.concat(blocks);
  const head = Buffer.alloc(8);
  head.write("icns", 0, "ascii");
  head.writeUInt32BE(body.length + 8, 4);
  return Buffer.concat([head, body]);
}

// ---------------------------------------------------------------------------
// The source, cropped once and resampled on demand
// ---------------------------------------------------------------------------

const SRC = decodePng(readFileSync(SOURCE));
const BOX = alphaBounds(SRC);

const cache = new Map();
function markAt(size) {
  if (!cache.has(size)) cache.set(size, resample(SRC, BOX, size));
  return cache.get(size);
}

// ---------------------------------------------------------------------------
// Preview: `node tools/make-icons.mjs --preview out.png`
//
// A contact sheet of every size at 1:1 and again magnified, because the only
// question that matters -- does this still read at 16px -- is one you have to
// answer with your eyes.
// ---------------------------------------------------------------------------

const previewAt = process.argv.indexOf("--preview");
if (previewAt !== -1) {
  const target = process.argv[previewAt + 1];
  if (!target) {
    console.error("--preview needs an output path");
    process.exit(1);
  }

  const sizes = [16, 24, 32, 48, 64, 128];
  const zoom = 6;
  const pad = 12;
  // Three bands. The icon has to carry itself on a dark taskbar, a light one,
  // and the mid-tone of a photo wallpaper -- so all three are on screen at once
  // rather than checked one at a time and forgotten.
  const BANDS = [
    [0x18, 0x18, 0x1b],
    [0x80, 0x80, 0x86],
    [0xf4, 0xf4, 0xf5],
  ];
  // Magnified row, a gap, the 1:1 row, and padding top and bottom.
  const bandHeight = pad + 128 * zoom + 10 + 128 + pad;
  const width = sizes.reduce((w, s) => w + s * zoom + pad, pad);
  const height = bandHeight * BANDS.length;
  const sheet = Buffer.alloc(width * height * 4);

  const blit = (src, size, dx, dy, scale) => {
    for (let y = 0; y < size * scale; y++) {
      for (let x = 0; x < size * scale; x++) {
        const s = (Math.floor(y / scale) * size + Math.floor(x / scale)) * 4;
        const d = ((dy + y) * width + dx + x) * 4;
        const a = src[s + 3] / 255;
        for (let c = 0; c < 3; c++) {
          sheet[d + c] = Math.round(sheet[d + c] * (1 - a) + src[s + c] * a);
        }
      }
    }
  };

  BANDS.forEach((bg, band) => {
    const top = band * bandHeight;
    for (let y = top; y < top + bandHeight; y++) {
      for (let x = 0; x < width; x++) {
        const i = (y * width + x) * 4;
        sheet[i] = bg[0];
        sheet[i + 1] = bg[1];
        sheet[i + 2] = bg[2];
        sheet[i + 3] = 0xff;
      }
    }

    let x0 = pad;
    for (const size of sizes) {
      const src = markAt(size);
      blit(src, size, x0, top + pad, zoom);
      // The same icon at 1:1 underneath, which is the size it is actually seen
      // at -- the magnified one flatters everything.
      blit(src, size, x0, top + pad + size * zoom + 10, 1);
      x0 += size * zoom + pad;
    }
  });

  writeFileSync(target, png2(width, height, sheet));
  console.log("wrote preview to", target);
  process.exit(0);
}

// ---------------------------------------------------------------------------
// Write everything
// ---------------------------------------------------------------------------

mkdirSync(OUT, { recursive: true });

console.log(
  `source ${SRC.width}x${SRC.height}, artwork ${BOX.edge}x${BOX.edge} at (${BOX.x},${BOX.y})`
);

// Tauri's five, straight from tauri.conf.json.
writeFileSync(join(OUT, "32x32.png"), png(32, markAt(32)));
writeFileSync(join(OUT, "128x128.png"), png(128, markAt(128)));
writeFileSync(join(OUT, "128x128@2x.png"), png(256, markAt(256)));

// The exe icon. Every size Windows actually asks for -- 16/20/24/32 in lists
// and the title bar, 40 at 125% DPI, 48/64 on the taskbar and Alt-Tab, 96 and
// 256 in Explorer's larger views. A missing size is not a missing icon: the
// shell scales a neighbour, and that upscale is exactly what "low quality"
// looks like.
//
// DIB below 64 and PNG above: a 128px DIB is 67 KB of uncompressed BGRA on its
// own, and every Windows this app runs on reads PNG entries fine.
writeFileSync(
  join(OUT, "icon.ico"),
  ico([
    { size: 16, data: dib(16, markAt(16)) },
    { size: 20, data: dib(20, markAt(20)) },
    { size: 24, data: dib(24, markAt(24)) },
    { size: 32, data: dib(32, markAt(32)) },
    { size: 40, data: dib(40, markAt(40)) },
    { size: 48, data: dib(48, markAt(48)) },
    { size: 64, data: png(64, markAt(64)) },
    { size: 96, data: png(96, markAt(96)) },
    { size: 128, data: png(128, markAt(128)) },
    { size: 256, data: png(256, markAt(256)) },
  ])
);

// Just the two sizes a browser tab asks for, so the favicon route embeds one
// kilobyte instead of the whole icon set.
writeFileSync(
  join(OUT, "favicon.ico"),
  ico([
    { size: 16, data: dib(16, markAt(16)) },
    { size: 32, data: dib(32, markAt(32)) },
  ])
);

writeFileSync(
  join(OUT, "icon.icns"),
  icns([
    { type: "ic11", data: png(32, markAt(32)) },
    { type: "ic12", data: png(64, markAt(64)) },
    { type: "ic07", data: png(128, markAt(128)) },
    { type: "ic08", data: png(256, markAt(256)) },
    { type: "ic09", data: png(512, markAt(512)) },
  ])
);

// Manifest icons, and what the guest page uses as its favicon.
writeFileSync(join(OUT, "icon-192.png"), png(192, markAt(192)));
writeFileSync(join(OUT, "icon-512.png"), png(512, markAt(512)));

console.log("wrote icons to", OUT);
