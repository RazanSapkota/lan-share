// Generates every raster icon in src-tauri/icons/ from the mark defined below.
//
//   node tools/make-icons.mjs
//
// Run by hand, never by the build. Nothing here is a dependency of anything --
// only `zlib` from the Node standard library, which is what makes it possible
// to keep the promise on the tin: no bundler, no npm packages, no toolchain.
// The alternative was five binary files nobody could regenerate or explain.
//
// The mark is drawn procedurally rather than rasterized from the SVG, because
// rasterizing SVG needs a renderer we deliberately do not have. Both are cut
// from the same numbers -- see GEOM -- so the app icon and the inline SVGs in
// ui/index.html and web/index.html stay the same shape.

import { deflateSync } from "node:zlib";
import { writeFileSync, mkdirSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const OUT = join(dirname(fileURLToPath(import.meta.url)), "..", "src-tauri", "icons");

// ---------------------------------------------------------------------------
// The mark
// ---------------------------------------------------------------------------
//
// A folder in the lower-left, two quarter-arcs radiating from its top-right
// corner: what you share, and who can reach it. Laid out on the same 24x24 grid
// as every other icon in the UI, then scaled.

const GEOM = {
  // Folder body, and the tab that steps up on its left. The tab overlaps the
  // body rather than sitting on top of it: two shapes that merely touch leave a
  // seam wherever their corner radii differ.
  folder: {
    left: 1.9,
    right: 14.2,
    top: 9.4,
    bottom: 20.6,
    radius: 1.7,
    tabTop: 6.4,
    tabRight: 8.2,
    tabRadius: 1.2,
    // The tab's right edge leans outward as it descends, so it runs into the
    // body instead of stopping dead.
    tabSlant: 1.4,
  },
  // Radiating from the folder's TOP-RIGHT CORNER -- not its mid-height, which
  // parks the first arc's cap against the folder's flank and welds the two
  // shapes into one blob.
  arcs: {
    cx: 14.2,
    cy: 9.4,
    inner: 4.0,
    outer: 7.0,
    width: 2.3,
    // Clear air punched through the folder around each arc. Without it the
    // shapes merge at exactly the sizes where separation matters most.
    gap: 0.85,
  },
  // The drawn shape spans roughly y 1.2..20.6 on the 24 grid, so it would sit
  // high if the grid itself were centred. Nudge it, rather than rewriting every
  // coordinate.
  offset: { x: -0.1, y: 1.0 },
};

/// Arc thickness and the gap around it, for a given tile size.
///
/// Both grow at small sizes. Below about 32px the gap is a pixel or less, and
/// a folder welded to its own signal is just a green blob.
function strokeFor(size) {
  if (size <= 20) return { width: 3.0, gap: 1.5 };
  if (size <= 32) return { width: 2.7, gap: 1.2 };
  return { width: GEOM.arcs.width, gap: GEOM.arcs.gap };
}

// Emerald, top to bottom. Flat colour reads as a sticker at 256px; two stops is
// enough depth without turning the glyph into a gradient exercise.
//
// The bottom stop is lifted from the emerald-600 it started at: with no tile
// behind it the mark has to hold its own against a white taskbar as well as a
// black one, and the darker green went muddy on light.
const GLYPH_TOP = [0x34, 0xd3, 0x99];
const GLYPH_BOTTOM = [0x0f, 0x9f, 0x76];

// ---------------------------------------------------------------------------
// Coverage functions. Each returns 0..1 for a point in 24x24 space; the
// renderer supersamples, so they only have to be right, not anti-aliased.
// ---------------------------------------------------------------------------

function insideRoundedRect(x, y, l, t, r, b, radius) {
  if (x < l || x > r || y < t || y > b) return false;
  const cx = Math.min(Math.max(x, l + radius), r - radius);
  const cy = Math.min(Math.max(y, t + radius), b - radius);
  const dx = x - cx;
  const dy = y - cy;
  return dx * dx + dy * dy <= radius * radius;
}

/// The tab: a rounded box on the left whose right edge leans outward, drawn
/// down INTO the body so the union has no seam.
function insideTab(x, y, f) {
  const bottom = f.top + f.radius + 0.5; // safely inside the body
  if (y < f.tabTop || y > bottom) return false;
  if (x < f.left) return false;
  // Only the top corners are rounded; the bottom is buried in the body.
  if (y < f.tabTop + f.tabRadius) {
    const cy = f.tabTop + f.tabRadius;
    if (x < f.left + f.tabRadius) {
      const dx = x - (f.left + f.tabRadius);
      const dy = y - cy;
      if (dx < 0 && dy < 0 && dx * dx + dy * dy > f.tabRadius * f.tabRadius) return false;
    }
  }
  const progress = (y - f.tabTop) / (bottom - f.tabTop);
  return x <= f.tabRight + f.tabSlant * progress;
}

function folderCoverage(x, y) {
  const f = GEOM.folder;
  if (insideRoundedRect(x, y, f.left, f.top, f.right, f.bottom, f.radius)) return 1;
  return insideTab(x, y, f) ? 1 : 0;
}

/// Distance from the centre line of a quarter-arc in the north-east quadrant of
/// (cx, cy), measured with round caps. Returns Infinity nowhere -- the caps make
/// it defined everywhere.
function arcDistance(x, y, radius) {
  const a = GEOM.arcs;
  const dx = x - a.cx;
  const dy = y - a.cy;
  // Inside the sweep (due east round to due north): distance to the circle.
  if (dx >= 0 && dy <= 0) return Math.abs(Math.hypot(dx, dy) - radius);
  // Outside it: distance to the nearer end point, which is what gives the
  // round cap.
  return Math.min(
    Math.hypot(x - (a.cx + radius), y - a.cy),
    Math.hypot(x - a.cx, y - (a.cy - radius))
  );
}

/// `simple` drops the outer arc. Two thin arcs turn to mush at 16px; one thick
/// one still reads as a signal.
function glyphCoverage(x, y, simple, stroke) {
  const a = GEOM.arcs;
  const radii = simple ? [a.inner] : [a.inner, a.outer];

  for (const r of radii) {
    if (arcDistance(x, y, r) <= stroke.width / 2) return 1;
  }
  if (!folderCoverage(x, y)) return 0;
  // Punch the gap: folder, minus the halo around every arc.
  for (const r of radii) {
    if (arcDistance(x, y, r) <= stroke.width / 2 + stroke.gap) return 0;
  }
  return 1;
}

// ---------------------------------------------------------------------------
// Renderer
// ---------------------------------------------------------------------------

// 8x8 coverage samples per pixel. These are a few hundred kilopixels in total,
// so the cost is a second of wall clock, and the payoff is edges that survive
// being scaled by the shell to whatever size it feels like.
const SAMPLES = 8;

/// Padding around the mark, as a fraction of the edge.
///
/// Almost none. There is no tile to sit inside any more, so the drawing runs to
/// the edges of the canvas the way every other taskbar icon does -- anything
/// more and this one looks a size smaller than its neighbours, which is the
/// entire complaint the tile caused. The sliver that remains is so the arcs'
/// round caps are not clipped by the edge.
function insetFor(_size) {
  return 0.02;
}

function mix(a, b, t) {
  return [
    Math.round(a[0] + (b[0] - a[0]) * t),
    Math.round(a[1] + (b[1] - a[1]) * t),
    Math.round(a[2] + (b[2] - a[2]) * t),
  ];
}

/// Render the mark at `size`, returning RGBA bytes.
///
/// The mark alone, on transparency. There is no plate behind it: a tile inset
/// from the canvas edges, with the glyph inset again inside that, is why this
/// icon used to sit visibly smaller in the taskbar than everything beside it.
function render(size, { simple = false } = {}) {
  const px = Buffer.alloc(size * size * 4);
  const pad = size * insetFor(size);
  const scale = (size - pad * 2) / 24;
  const stroke = strokeFor(size);
  const total = SAMPLES * SAMPLES;

  for (let y = 0; y < size; y++) {
    // One gradient stop per row: the ramp runs down the icon, so every pixel in
    // a row is the same colour and only the coverage changes.
    const glyph = mix(GLYPH_TOP, GLYPH_BOTTOM, size === 1 ? 0 : y / (size - 1));

    for (let x = 0; x < size; x++) {
      let hits = 0;
      for (let sy = 0; sy < SAMPLES; sy++) {
        for (let sx = 0; sx < SAMPLES; sx++) {
          const gx = (x + (sx + 0.5) / SAMPLES - pad) / scale - GEOM.offset.x;
          const gy = (y + (sy + 0.5) / SAMPLES - pad) / scale - GEOM.offset.y;
          if (glyphCoverage(gx, gy, simple, stroke)) hits++;
        }
      }

      const i = (y * size + x) * 4;
      // Straight alpha with the colour carried through even at zero coverage:
      // a transparent pixel that is black underneath fringes dark when the
      // shell composites it against a light background.
      px[i] = glyph[0];
      px[i + 1] = glyph[1];
      px[i + 2] = glyph[2];
      px[i + 3] = Math.round((hits / total) * 255);
    }
  }
  return px;
}

// ---------------------------------------------------------------------------
// PNG
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

  // Filter type 0 on every row. The shapes are flat colour, so deflate does the
  // work and per-row filter heuristics would buy nothing.
  const stride = width * 4;
  const raw = Buffer.alloc((stride + 1) * height);
  for (let y = 0; y < height; y++) {
    const at = y * (stride + 1);
    raw[at] = 0;
    rgba.copy(raw, at + 1, y * stride, (y + 1) * stride);
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
// Write everything
// ---------------------------------------------------------------------------

const cache = new Map();
function markAt(size, simple = false) {
  const key = `${size}:${simple}`;
  if (!cache.has(key)) cache.set(key, render(size, { simple }));
  return cache.get(key);
}

// ---------------------------------------------------------------------------
// Preview: `node tools/make-icons.mjs --preview out.png`
//
// Writes a contact sheet of every size at 1:1 and again magnified, because the
// only question that matters -- does this still read as a folder at 16px -- is
// one you have to answer with your eyes.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// `node tools/make-icons.mjs --svg`
//
// Prints the same mark as SVG paths, derived from GEOM, for the three places
// that carry an inline copy: assets.rs ICON_SVG, ui/index.html (the header
// mark) and web/index.html (the PIN screen). Change GEOM, run this, paste --
// otherwise the drawing and the icons drift apart and only one of them is ever
// noticed.
// ---------------------------------------------------------------------------

if (process.argv.includes("--svg")) {
  const f = GEOM.folder;
  const a = GEOM.arcs;
  const o = GEOM.offset;
  const n = (v) => Number(v.toFixed(1));
  // Offset baked in, because the renderer applies it and the SVG has no
  // equivalent step.
  const L = n(f.left + o.x);
  const R = n(f.right + o.x);
  const T = n(f.top + o.y);
  const B = n(f.bottom + o.y);
  const r = f.radius;
  const TT = n(f.tabTop + o.y);
  const TR = n(f.tabRight + o.x);
  const SL = n(f.tabRight + f.tabSlant + o.x);
  const cx = n(a.cx + o.x);
  const cy = n(a.cy + o.y);

  const folder =
    `M${n(L + r)} ${TT}H${TR}L${SL} ${T}H${n(R - r)}` +
    `A${r} ${r} 0 0 1 ${R} ${n(T + r)}V${n(B - r)}` +
    `A${r} ${r} 0 0 1 ${n(R - r)} ${B}H${n(L + r)}` +
    `A${r} ${r} 0 0 1 ${L} ${n(B - r)}V${n(TT + r)}` +
    `A${r} ${r} 0 0 1 ${n(L + r)} ${TT}Z`;
  const arc = (radius) =>
    `M${n(cx + radius)} ${cy}A${radius} ${radius} 0 0 0 ${cx} ${n(cy - radius)}`;

  console.log(`<path d="${folder}" fill="currentColor"/>`);
  for (const radius of [a.inner, a.outer]) {
    console.log(
      `<path d="${arc(radius)}" stroke="currentColor" stroke-width="${a.width}" stroke-linecap="round"/>`
    );
  }
  process.exit(0);
}

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
  // Three bands. With no tile behind it the mark has to carry itself on a dark
  // taskbar, a light one, and the mid-tone of a photo wallpaper -- so all three
  // are on screen at once rather than checked one at a time and forgotten.
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
      const src = markAt(size, size <= 32);
      blit(src, size, x0, top + pad, zoom);
      // The same mark at 1:1 underneath, which is the size it is actually seen
      // at -- the magnified one flatters everything.
      blit(src, size, x0, top + pad + size * zoom + 10, 1);
      x0 += size * zoom + pad;
    }
  });

  writeFileSync(target, png2(width, height, sheet));
  console.log("wrote preview to", target);
  process.exit(0);
}

mkdirSync(OUT, { recursive: true });

// Tauri's five, straight from tauri.conf.json.
writeFileSync(join(OUT, "32x32.png"), png(32, markAt(32, true)));
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
    { size: 16, data: dib(16, markAt(16, true)) },
    { size: 20, data: dib(20, markAt(20, true)) },
    { size: 24, data: dib(24, markAt(24, true)) },
    { size: 32, data: dib(32, markAt(32, true)) },
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
    { size: 16, data: dib(16, markAt(16, true)) },
    { size: 32, data: dib(32, markAt(32, true)) },
  ])
);

writeFileSync(
  join(OUT, "icon.icns"),
  icns([
    { type: "ic11", data: png(32, markAt(32, true)) },
    { type: "ic12", data: png(64, markAt(64)) },
    { type: "ic07", data: png(128, markAt(128)) },
    { type: "ic08", data: png(256, markAt(256)) },
    { type: "ic09", data: png(512, markAt(512)) },
  ])
);

// Manifest icons. Chrome on Android will not offer "Add to Home Screen" for an
// SVG-only icon set, so these are what make the manifest more than decorative.
writeFileSync(join(OUT, "icon-192.png"), png(192, markAt(192)));
writeFileSync(join(OUT, "icon-512.png"), png(512, markAt(512)));

console.log("wrote icons to", OUT);
