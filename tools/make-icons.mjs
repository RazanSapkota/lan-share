// Draws the app mark and writes every icon in src-tauri/icons/ from it.
//
//   node tools/make-icons.mjs
//   node tools/make-icons.mjs --preview out.png
//   node tools/make-icons.mjs --check
//
// Run by hand, never by the build. Nothing here is a dependency of anything --
// only `zlib` from the Node standard library, which is what makes it possible
// to keep the promise on the tin: no bundler, no npm packages, no toolchain.
//
// Nothing is committed that nobody can regenerate: there is no source bitmap,
// only the fifteen-odd numbers in `MARK` below. Delete src-tauri/icons/ and one
// command puts it back, byte for byte.
//
// This replaced a pipeline that downscaled one 800px PNG to all twelve sizes,
// and the reason is the whole point of the file. A 24px taskbar icon made that
// way is a 50:1 reduction: the wires came out 0.84 device pixels wide, which no
// filter can draw -- it can only fade them to grey. Here each size is drawn AT
// its size, so a stroke can be snapped to a whole pixel and detail that will not
// fit can be dropped instead of smeared. See `TIERS`.
//
// `zlib` still does the heavy lifting on the way out -- `deflateSync` per PNG.

import { deflateSync } from "node:zlib";
import { writeFileSync, mkdirSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const OUT = join(HERE, "..", "src-tauri", "icons");
/// The desktop panel's header mark. `ui/` is Tauri's `frontendDist`, so nothing
/// outside it is reachable from that window -- the file has to live there.
const UI_OUT = join(HERE, "..", "ui");

// ---------------------------------------------------------------------------
// Light
// ---------------------------------------------------------------------------

/// sRGB byte -> linear light, one entry per possible value.
///
/// Blending pixels means blending light, and an sRGB byte is not light -- it is
/// light through a curve that spreads the 256 steps where the eye can see them.
/// Blend the curve instead of what it encodes and every result is biased dark:
/// a pixel that is a quarter navy over cyan lands at luminance 60 where it
/// should be 96. That one number is what "the small icons look muddy" was.
///
/// The piecewise curve from the spec, not the 2.2 power approximation: the
/// linear toe below 0.04045 is exactly where the navy lives, and the
/// approximation is worst there.
const SRGB_TO_LINEAR = new Float64Array(256);
for (let i = 0; i < 256; i++) {
  const c = i / 255;
  SRGB_TO_LINEAR[i] = c <= 0.04045 ? c / 12.92 : Math.pow((c + 0.055) / 1.055, 2.4);
}

/// Linear light -> sRGB byte. No lookup table on this side: the input is a
/// continuous blend, not one of 256 known values, so there is nothing to key a
/// table on.
function toSrgb(v) {
  if (!(v > 0)) return 0; // also catches NaN
  if (v >= 1) return 255;
  const s = v <= 0.0031308 ? v * 12.92 : 1.055 * Math.pow(v, 1 / 2.4) - 0.055;
  return Math.round(s * 255);
}

/// "#rrggbb" -> linear RGB triple. Goes through the table, since every input
/// digit pair is a byte.
const rgb = (hex) => [1, 3, 5].map((i) => SRGB_TO_LINEAR[parseInt(hex.slice(i, i + 2), 16)]);

// ---------------------------------------------------------------------------
// Signed distance fields
// ---------------------------------------------------------------------------
//
// Every shape in the mark is one of three primitives, and all three have a
// closed-form distance -- no iteration, no curve flattening, no path rasteriser.
// Negative inside, in output-pixel units.

const clamp01 = (v) => (v < 0 ? 0 : v > 1 ? 1 : v);

const sdCircle = (px, py, cx, cy, r) => Math.hypot(px - cx, py - cy) - r;

/// A segment with round caps -- the wires.
function sdCapsule(px, py, ax, ay, bx, by, half) {
  const vx = bx - ax;
  const vy = by - ay;
  const wx = px - ax;
  const wy = py - ay;
  const t = clamp01((wx * vx + wy * vy) / (vx * vx + vy * vy));
  return Math.hypot(wx - vx * t, wy - vy * t) - half;
}

/// Exact triangle distance. `s` is the winding sign, so the three half-plane
/// tests agree regardless of which way the vertices were listed.
function sdTriangle(px, py, p) {
  const e = [
    [p[1][0] - p[0][0], p[1][1] - p[0][1]],
    [p[2][0] - p[1][0], p[2][1] - p[1][1]],
    [p[0][0] - p[2][0], p[0][1] - p[2][1]],
  ];
  const v = [
    [px - p[0][0], py - p[0][1]],
    [px - p[1][0], py - p[1][1]],
    [px - p[2][0], py - p[2][1]],
  ];
  const s = Math.sign(e[0][0] * e[2][1] - e[0][1] * e[2][0]);
  let d2 = Infinity;
  let w = Infinity;
  for (let i = 0; i < 3; i++) {
    const t = clamp01((v[i][0] * e[i][0] + v[i][1] * e[i][1]) / (e[i][0] ** 2 + e[i][1] ** 2));
    const qx = v[i][0] - e[i][0] * t;
    const qy = v[i][1] - e[i][1] * t;
    d2 = Math.min(d2, qx * qx + qy * qy);
    w = Math.min(w, s * (v[i][0] * e[i][1] - v[i][1] * e[i][0]));
  }
  return -Math.sqrt(d2) * Math.sign(w);
}

/// Coverage of one output pixel, as `ss * ss` sub-positions each carrying its
/// own analytic edge coverage.
///
/// The filter width handed to each sample is the SUB-pixel width, `1 / ss`, not
/// `1`: every sub-sample is antialiasing its own sub-pixel. Passing `1` there
/// blurs each edge by a factor of `ss`, which is the exact mistake this whole
/// file exists to stop making.
///
/// Worth about `ss^2 x ss^2` box supersampling for `ss^2` distance evaluations,
/// because each sample is already exact for a straight edge; the sub-positions
/// are only there to handle the pixels where two edges meet.
function coverage(sdf, x, y, ss) {
  const step = 1 / ss;
  let sum = 0;
  for (let j = 0; j < ss; j++) {
    const py = y + (j + 0.5) * step;
    for (let i = 0; i < ss; i++) {
      sum += clamp01(0.5 - sdf(x + (i + 0.5) * step, py) / step);
    }
  }
  return sum / (ss * ss);
}

// Layer constructors. Each carries its own bounding box, which is not an
// optimisation so much as the difference between a run that finishes and one
// that does not: without it the two 512s alone are 33 million distance
// evaluations.

const circle = (cx, cy, r, lin) => ({
  lin,
  sdf: (x, y) => sdCircle(x, y, cx, cy, r),
  box: [cx - r, cy - r, cx + r, cy + r],
});

const capsule = (ax, ay, bx, by, half, lin) => ({
  lin,
  sdf: (x, y) => sdCapsule(x, y, ax, ay, bx, by, half),
  box: [
    Math.min(ax, bx) - half,
    Math.min(ay, by) - half,
    Math.max(ax, bx) + half,
    Math.max(ay, by) + half,
  ],
});

const triangle = (p, lin) => ({
  lin,
  sdf: (x, y) => sdTriangle(x, y, p),
  box: [
    Math.min(p[0][0], p[1][0], p[2][0]),
    Math.min(p[0][1], p[1][1], p[2][1]),
    Math.max(p[0][0], p[1][0], p[2][0]),
    Math.max(p[0][1], p[1][1], p[2][1]),
  ],
});

/// Paint the layers back to front and encode once.
///
/// The canvas holds PREMULTIPLIED LINEAR RGBA. Both halves of that matter: `over`
/// is only associative on premultiplied values, and see SRGB_TO_LINEAR for the
/// other half.
///
/// `fringe` is written into the fully transparent pixels. They would otherwise
/// be transparent black, and a shell that scales an icon without consulting its
/// alpha -- they exist -- draws that as a dark rim around the disc.
function paint(size, layers, ss, fringe) {
  const buf = new Float64Array(size * size * 4);

  for (const layer of layers) {
    const x0 = Math.max(0, Math.floor(layer.box[0]) - 1);
    const y0 = Math.max(0, Math.floor(layer.box[1]) - 1);
    const x1 = Math.min(size, Math.ceil(layer.box[2]) + 1);
    const y1 = Math.min(size, Math.ceil(layer.box[3]) + 1);

    for (let y = y0; y < y1; y++) {
      for (let x = x0; x < x1; x++) {
        const a = coverage(layer.sdf, x, y, ss);
        if (a === 0) continue;
        const d = (y * size + x) * 4;
        const k = 1 - a;
        buf[d] = layer.lin[0] * a + buf[d] * k;
        buf[d + 1] = layer.lin[1] * a + buf[d + 1] * k;
        buf[d + 2] = layer.lin[2] * a + buf[d + 2] * k;
        buf[d + 3] = a + buf[d + 3] * k;
      }
    }
  }

  const out = Buffer.alloc(size * size * 4);
  const edge = fringe.map(toSrgb);
  for (let i = 0; i < size * size; i++) {
    const d = i * 4;
    const a = buf[d + 3];
    if (a <= 1 / 512) {
      out[d] = edge[0];
      out[d + 1] = edge[1];
      out[d + 2] = edge[2];
      out[d + 3] = 0;
      continue;
    }
    // Un-premultiply first, encode second. The other order is the gamma bug
    // wearing a different hat.
    out[d] = toSrgb(buf[d] / a);
    out[d + 1] = toSrgb(buf[d + 1] / a);
    out[d + 2] = toSrgb(buf[d + 2] / a);
    out[d + 3] = Math.round(a * 255);
  }
  return out;
}

// ---------------------------------------------------------------------------
// The mark
// ---------------------------------------------------------------------------
//
// A cyan disc carrying a navy glyph: three nodes wired to a play triangle.
//
// It used to be the other way up -- a navy squircle with the cyan mark on it --
// and inverting it is what fixed the complaint this file was rewritten for. Two
// measurements say why. A navy plate is 1.02:1 against a dark Windows taskbar,
// so it was not blurry, it was invisible; what anyone actually saw was the mark
// floating on nothing. And a filled squircle covers 95.8% of its canvas where a
// circle covers 78.5%, so it also read a size larger than every icon beside it.
// Chrome's shipped icon measures 78.1%. Navy on cyan is 10:1, which holds on a
// dark taskbar, on Explorer's white, and on the installer's white alike -- the
// glyph is opaque for that reason and is not a knockout, which would have been
// 9.4:1 on dark but 1.81:1 on white.

const BODY = "#22d3ee";
const GLYPH = "#0e1626";

/// How much of the mark is drawn, by size. Fractions of the edge throughout.
///
/// The ladder is the point of drawing per size rather than downscaling. A wire
/// is 0.048 of the edge, which is 2.3 device pixels at 48 and 1.2 at 24 -- at 24
/// it stops being structure and becomes texture, so below 48 it is not drawn at
/// all rather than drawn as a grey smudge. Same argument one size down for the
/// nodes: 16 and 20 have room for the triangle or for the trefoil, not both.
///
/// The triangle grows to 0.32 when it is alone, because it has the room. Tier 2
/// pushes the nodes slightly outward and enlarges them so they clear the rim and
/// read as notches rather than dents.
///
/// `hub` is what the wires converge on, and it is the reason the play symbol
/// flips colour at 48px: with three strokes running into it, the triangle needs
/// a field of its own or it knots with them at exactly the point the eye goes
/// first. Below 48 there are no wires, so it does not, and the triangle can be
/// the bigger and simpler navy-on-cyan shape instead.
const TIERS = [
  { upTo: 20, tri: [0.32, 0.36], node: null, wire: 0, hub: 0 },
  { upTo: 40, tri: [0.26, 0.29], node: [0.355, 0.085], wire: 0, hub: 0 },
  { upTo: Infinity, tri: [0.135, 0.152], node: [0.34, 0.076], wire: 0.048, hub: 0.155 },
];

/// A right-pointing triangle about its CENTROID, not its bounding box. Centring
/// the box puts the visual mass left of centre and the whole mark looks like it
/// is sliding backwards; this is the same correction every play button makes.
const playTriangle = (cx, cy, w, h) => [
  [cx - w / 3, cy - h / 2],
  [cx - w / 3, cy + h / 2],
  [cx + (2 * w) / 3, cy],
];

function render(size) {
  const E = size;
  const c = size / 2;
  const tier = TIERS.find((t) => size <= t.upTo);
  const body = rgb(BODY);
  const glyph = rgb(GLYPH);
  const w = tier.tri[0] * E;
  const h = tier.tri[1] * E;

  // Full bleed: r is exactly half the canvas, centred on it. The neighbours do
  // not pad either -- Chrome, VS Code and Steam all ship a 100% bounding box --
  // and a circle gives back the corner area a squircle keeps.
  const layers = [circle(c, c, c, body)];

  if (tier.node) {
    const [orbit, radius] = tier.node;
    const R = orbit * E;
    // Mirrored about the vertical axis by construction rather than by rotating
    // through 30 and 150 degrees: cos(30 deg) and -cos(150 deg) differ in the
    // last bit, and that is enough to make the icon lean by a fraction of a
    // pixel at 512.
    const ux = Math.cos(Math.PI / 6);
    const uy = Math.sin(Math.PI / 6);
    const dirs = [
      [0, -1],
      [ux, uy],
      [-ux, uy],
    ];

    if (tier.wire) {
      for (const [dxu, dyu] of dirs) {
        layers.push(
          capsule(c, c, c + dxu * R, c + dyu * R, (tier.wire * E) / 2, glyph)
        );
      }
    }
    for (const [dxu, dyu] of dirs) {
      layers.push(circle(c + dxu * R, c + dyu * R, radius * E, glyph));
    }
  }

  // Painted over the wires, so it covers the knot where all three meet.
  if (tier.hub) layers.push(circle(c, c, tier.hub * E, glyph));

  // Cyan when it sits on the hub, navy when it sits on the body. Either way it
  // is an opaque fill and not a hole: a knockout would show whatever is behind
  // the icon, which is 9.4:1 on a dark taskbar but 1.81:1 on Explorer's white.
  layers.push(triangle(playTriangle(c, c, w, h), tier.hub ? body : glyph));

  // 4x4 below 128. At and above it the smallest feature -- the wire, 6.1px at
  // 128 -- is several pixels wide, a second sub-sample is already invisible, and
  // this halves the cost of the two 512s.
  return paint(size, layers, size >= 128 ? 2 : 4, body);
}

const cache = new Map();
function markAt(size) {
  if (!cache.has(size)) cache.set(size, render(size));
  return cache.get(size);
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
  // the spec suggests and every real encoder uses.
  //
  // What it is worth depends entirely on the artwork. Against a fixed Sub filter
  // it saved about 11% on a photographic gradient this mark long predates; on
  // two flat colours and their antialiased boundary it is worth a few percent,
  // and on the rows that are one repeated colour it cannot help at all. It stays
  // because it is never far wrong on either kind of source, and the icons are
  // include_bytes!'d into the binary, so this is binary size.
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
// Every size the tool emits, in one place: `--check` and the write block below
// have to agree about this list or the checks are checking something else.
// ---------------------------------------------------------------------------

const SIZES = [16, 20, 24, 32, 40, 48, 64, 96, 128, 192, 256, 512];

// ---------------------------------------------------------------------------
// Check: `node tools/make-icons.mjs --check`
//
// The failures this catches all look fine in a thumbnail, which is why they are
// assertions and not something to eyeball on the contact sheet.
// ---------------------------------------------------------------------------

if (process.argv.includes("--check")) {
  let failed = 0;
  const check = (name, ok, detail = "") => {
    if (!ok) failed++;
    console.log(`${ok ? "ok  " : "FAIL"}  ${name}${detail ? `  -- ${detail}` : ""}`);
  };

  // The two transfer functions have to be each other's inverse, or every colour
  // in the file is quietly a shade off and nothing else here would notice.
  let roundTrip = true;
  for (let i = 0; i < 256; i++) if (toSrgb(SRGB_TO_LINEAR[i]) !== i) roundTrip = false;
  check("sRGB transfer round-trips over all 256 values", roundTrip);

  const BODY_RGB = rgb(BODY).map(toSrgb);
  const GLYPH_RGB = rgb(GLYPH).map(toSrgb);
  const at = (m, s, x, y) => m.subarray((y * s + x) * 4, (y * s + x) * 4 + 4);
  const same = (a, b) => a[0] === b[0] && a[1] === b[1] && a[2] === b[2];

  for (const size of SIZES) {
    const m = markAt(size);
    const tier = TIERS.find((t) => size <= t.upTo);
    const label = `${size}px`.padStart(5);

    // Alpha is the disc and only the disc -- every glyph element sits inside it,
    // so alpha has to be symmetric under BOTH mirrors. Colour cannot be: the
    // play triangle points right on purpose.
    let alphaSym = true;
    for (let y = 0; y < size; y++) {
      for (let x = 0; x < size; x++) {
        const a = m[(y * size + x) * 4 + 3];
        if (a !== m[(y * size + (size - 1 - x)) * 4 + 3]) alphaSym = false;
        if (a !== m[((size - 1 - y) * size + x) * 4 + 3]) alphaSym = false;
      }
    }
    check(`${label} disc is symmetric under both mirrors`, alphaSym);

    // Colour symmetry outside the triangle's reach, which is what proves the
    // nodes and wires do not lean. The triangle spans 2w/3 right of centre, so
    // it and its mirror are both clear of |x - c| > 2w/3 + 1.
    const guard = (2 * (tier.tri[0] * size)) / 3 + 1.5;
    let markSym = true;
    for (let y = 0; y < size; y++) {
      for (let x = 0; x < size; x++) {
        if (Math.abs(x + 0.5 - size / 2) <= guard) continue;
        if (!same(at(m, size, x, y), at(m, size, size - 1 - x, y))) markSym = false;
      }
    }
    check(`${label} nodes and wires do not lean`, markSym);

    // A feature that never reaches its own colour was drawn too small for this
    // size and should have been dropped by the ladder instead of faded.
    let solidGlyph = false;
    let solidBody = false;
    let mass = 0;
    const seen = new Set();
    for (let i = 0; i < size * size; i++) {
      const px = m.subarray(i * 4, i * 4 + 4);
      mass += px[3] / 255;
      if (px[3] === 255 && same(px, GLYPH_RGB)) solidGlyph = true;
      if (px[3] === 255 && same(px, BODY_RGB)) solidBody = true;
      if (size === 16) seen.add((px[0] << 16) | (px[1] << 8) | px[2]);
    }
    check(`${label} glyph reaches full navy`, solidGlyph);
    check(`${label} body reaches full cyan`, solidBody);

    // A full-bleed disc covers pi/4 of its square. This is the rasteriser's area
    // accuracy stated as one number, and it is why the icon now weighs what its
    // neighbours weigh.
    const frac = mass / (size * size);
    check(
      `${label} covers pi/4 of the canvas`,
      Math.abs(frac - Math.PI / 4) < 0.015,
      `${(frac * 100).toFixed(2)}% vs 78.54%`
    );

    check(`${label} corner is transparent`, m[3] === 0);
    check(`${label} centre is opaque`, at(m, size, size >> 1, size >> 1)[3] === 255);

    if (size === 16) {
      // Two flat colours and one antialiased boundary. If this explodes,
      // something is being blurred that the ladder should have dropped.
      check(`${label} colour budget`, seen.size < 48, `${seen.size} distinct RGB`);
    }
  }

  console.log(failed ? `\n${failed} check(s) failed` : "\nall checks passed");
  process.exit(failed ? 1 : 0);
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

  // 20 and 40 are in icon.ico and were never on this sheet, which is how a
  // ladder boundary could move without anyone seeing what it did.
  const sizes = [16, 20, 24, 32, 40, 48, 64, 128];
  const zoom = 6;
  const pad = 12;
  // Four bands. The icon has to carry itself on a dark taskbar, a light one, the
  // mid-tone of a photo wallpaper, and pure white -- the last because the guest
  // PIN card and the panel's light themes both draw the mark on #ffffff, and
  // that is where a cyan rim has the least to work with.
  const BANDS = [
    [0x18, 0x18, 0x1b],
    [0x80, 0x80, 0x86],
    [0xf4, 0xf4, 0xf5],
    [0xff, 0xff, 0xff],
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
          // `over` in linear light, for the same reason paint() blends there. A
          // sheet that composites in sRGB reproduces the exact bug it exists to
          // report on, and would have shown an edge artifact the icons do not
          // have -- worst on the mid-grey band, where the two are closest.
          const bg = SRGB_TO_LINEAR[sheet[d + c]];
          const fg = SRGB_TO_LINEAR[src[s + c]];
          sheet[d + c] = toSrgb(bg * (1 - a) + fg * a);
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

for (const tier of TIERS) {
  const covers = SIZES.filter((s) => TIERS.find((t) => s <= t.upTo) === tier);
  const drawn = ["disc", tier.node && "nodes", tier.wire && "wires", "play"].filter(Boolean);
  console.log(`${covers.join("/").padEnd(28)} ${drawn.join(" + ")}`);
}

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

// Manifest icons, and what the guest page uses as its favicon and its PIN-screen
// mark.
writeFileSync(join(OUT, "icon-192.png"), png(192, markAt(192)));
writeFileSync(join(OUT, "icon-512.png"), png(512, markAt(512)));

// The desktop header draws this at 32 CSS px; 128 is four times that, so it
// stays sharp up to a 400% display and still costs less than the 192.
writeFileSync(join(UI_OUT, "icon.png"), png(128, markAt(128)));

console.log("wrote icons to", OUT, "and the header mark to", UI_OUT);
