// Screenshot sizing and coordinate transforms shared by provider adapters.
//
// Provider models see a (possibly downscaled) screenshot of one window and
// return coordinates in that screenshot's pixel space. The engine expects
// window-local pixels. ScreenGeometry maps between the two and rejects
// coordinates outside the screenshot.
import { CuaError } from '../errors.js';
import { decodePng, encodePng, resize, crop } from '../image.js';

// Anthropic: models that support computer_toolset_20260801 accept up to 2576 px
// on the long edge and 4784 visual tokens (ceil(w/28) * ceil(h/28)).
export const ANTHROPIC_TOOLSET_LIMITS = Object.freeze({ maxLongEdge: 2576, maxVisualTokens: 4784, tokenPatch: 28 });
// Anthropic: earlier models (1568 px long edge, ~1.15 MP).
export const ANTHROPIC_LEGACY_LIMITS = Object.freeze({ maxLongEdge: 1568, maxPixels: 1_150_000 });

export function fitSize(width, height, limits = {}) {
  if (!(width > 0 && height > 0)) throw new CuaError('bad_geometry', `invalid size ${width}x${height}`);
  let s = 1;
  if (limits.maxLongEdge) s = Math.min(s, limits.maxLongEdge / Math.max(width, height));
  if (limits.maxPixels) s = Math.min(s, Math.sqrt(limits.maxPixels / (width * height)));
  if (limits.maxWidth) s = Math.min(s, limits.maxWidth / width);
  if (limits.maxHeight) s = Math.min(s, limits.maxHeight / height);
  const p = limits.tokenPatch || 28;
  const tokens = (w, h) => Math.ceil(w / p) * Math.ceil(h / p);
  if (limits.maxVisualTokens) s = Math.min(s, Math.sqrt((limits.maxVisualTokens * p * p) / (width * height)));
  let w = Math.max(1, Math.floor(width * s));
  let h = Math.max(1, Math.floor(height * s));
  while (limits.maxVisualTokens && tokens(w, h) > limits.maxVisualTokens) {
    s *= 0.995;
    w = Math.max(1, Math.floor(width * s));
    h = Math.max(1, Math.floor(height * s));
  }
  return { width: w, height: h };
}

export class ScreenGeometry {
  constructor(windowWidth, windowHeight, limits) {
    this.windowWidth = windowWidth;
    this.windowHeight = windowHeight;
    const fit = fitSize(windowWidth, windowHeight, limits);
    this.width = fit.width; // screenshot space
    this.height = fit.height;
    this.sx = this.width / windowWidth;
    this.sy = this.height / windowHeight;
  }

  get scaled() {
    return this.width !== this.windowWidth || this.height !== this.windowHeight;
  }

  checkPoint(name, pt) {
    if (!Array.isArray(pt) || pt.length !== 2 || !pt.every((v) => typeof v === 'number' && Number.isFinite(v))) {
      throw new CuaError('invalid_input', `${name} must be [x, y] numbers`);
    }
    const [x, y] = pt;
    if (x < 0 || y < 0 || x >= this.width || y >= this.height) {
      throw new CuaError('out_of_bounds', `${name} [${x}, ${y}] is outside the ${this.width}x${this.height} screenshot`);
    }
    return [x, y];
  }

  // screenshot pixel -> window-local pixel (pixel-center mapping, clamped)
  toWindow(pt, name = 'coordinate') {
    const [x, y] = this.checkPoint(name, pt);
    return [
      Math.min(this.windowWidth - 1, Math.max(0, Math.floor((Math.floor(x) + 0.5) / this.sx))),
      Math.min(this.windowHeight - 1, Math.max(0, Math.floor((Math.floor(y) + 0.5) / this.sy))),
    ];
  }

  // window-local pixel -> screenshot pixel
  toScreenshot([wx, wy]) {
    return [
      Math.min(this.width - 1, Math.floor((wx + 0.5) * this.sx)),
      Math.min(this.height - 1, Math.floor((wy + 0.5) * this.sy)),
    ];
  }

  // screenshot-space distance -> window-space distance
  lengthToWindow(d, axis = 'y') {
    return Math.max(1, Math.round(Math.abs(d) / (axis === 'x' ? this.sx : this.sy)));
  }
}

// Resize a daemon screenshot into the geometry's screenshot space.
export function scaleShot(shot, geom) {
  if (!geom.scaled && shot.width === geom.width && shot.height === geom.height) return { data: shot.data, mimeType: shot.mimeType };
  if (shot.mimeType !== 'image/png') throw new CuaError('bad_image', `cannot resize ${shot.mimeType}`);
  const img = decodePng(shot.data);
  return { data: encodePng(resize(img, geom.width, geom.height)), mimeType: 'image/png' };
}

// Crop a window-space region at full resolution, then fit it into `box`
// (width/height) preserving the aspect ratio.
export function zoomShot(shot, [x0, y0, x1, y1], box) {
  if (shot.mimeType !== 'image/png') throw new CuaError('bad_image', `cannot crop ${shot.mimeType}`);
  const img = decodePng(shot.data);
  const c = crop(img, x0, y0, Math.min(x1, img.width), Math.min(y1, img.height));
  const s = Math.min(box.width / c.width, box.height / c.height);
  const w = Math.max(1, Math.floor(c.width * s));
  const h = Math.max(1, Math.floor(c.height * s));
  return { data: encodePng(resize(c, w, h)), mimeType: 'image/png', width: w, height: h };
}
