// PNG decode/crop/resize/encode for provider adapters (pure JS via pngjs).
import { PNG } from 'pngjs';
import { CuaError } from './errors.js';

export function decodePng(base64) {
  try {
    const png = PNG.sync.read(Buffer.from(base64, 'base64'));
    return { width: png.width, height: png.height, data: png.data };
  } catch (err) {
    throw new CuaError('bad_image', `cannot decode PNG: ${err.message}`);
  }
}

export function encodePng(img) {
  const png = new PNG({ width: img.width, height: img.height });
  img.data.copy(png.data);
  return PNG.sync.write(png).toString('base64');
}

export function crop(img, x0, y0, x1, y1) {
  const w = x1 - x0;
  const h = y1 - y0;
  const data = Buffer.alloc(w * h * 4);
  for (let y = 0; y < h; y++) {
    const src = ((y0 + y) * img.width + x0) * 4;
    img.data.copy(data, y * w * 4, src, src + w * 4);
  }
  return { width: w, height: h, data };
}

// Area-average when shrinking, bilinear when enlarging.
export function resize(img, width, height) {
  if (width === img.width && height === img.height) return img;
  const out = Buffer.alloc(width * height * 4);
  const sx = img.width / width;
  const sy = img.height / height;
  if (sx >= 1 && sy >= 1) {
    for (let y = 0; y < height; y++) {
      const ya = y * sy;
      const yb = Math.min(img.height, (y + 1) * sy);
      for (let x = 0; x < width; x++) {
        const xa = x * sx;
        const xb = Math.min(img.width, (x + 1) * sx);
        let r = 0, g = 0, b = 0, a = 0, wsum = 0;
        for (let yy = Math.floor(ya); yy < Math.ceil(yb); yy++) {
          const wy = Math.min(yy + 1, yb) - Math.max(yy, ya);
          for (let xx = Math.floor(xa); xx < Math.ceil(xb); xx++) {
            const w = (Math.min(xx + 1, xb) - Math.max(xx, xa)) * wy;
            const i = (yy * img.width + xx) * 4;
            r += img.data[i] * w; g += img.data[i + 1] * w; b += img.data[i + 2] * w; a += img.data[i + 3] * w;
            wsum += w;
          }
        }
        const o = (y * width + x) * 4;
        out[o] = Math.round(r / wsum); out[o + 1] = Math.round(g / wsum); out[o + 2] = Math.round(b / wsum); out[o + 3] = Math.round(a / wsum);
      }
    }
  } else {
    for (let y = 0; y < height; y++) {
      const fy = Math.min(img.height - 1, Math.max(0, (y + 0.5) * sy - 0.5));
      const y0 = Math.floor(fy), y1 = Math.min(img.height - 1, y0 + 1), ty = fy - y0;
      for (let x = 0; x < width; x++) {
        const fx = Math.min(img.width - 1, Math.max(0, (x + 0.5) * sx - 0.5));
        const x0 = Math.floor(fx), x1 = Math.min(img.width - 1, x0 + 1), tx = fx - x0;
        const o = (y * width + x) * 4;
        for (let c = 0; c < 4; c++) {
          const p00 = img.data[(y0 * img.width + x0) * 4 + c], p01 = img.data[(y0 * img.width + x1) * 4 + c];
          const p10 = img.data[(y1 * img.width + x0) * 4 + c], p11 = img.data[(y1 * img.width + x1) * 4 + c];
          out[o + c] = Math.round((p00 * (1 - tx) + p01 * tx) * (1 - ty) + (p10 * (1 - tx) + p11 * tx) * ty);
        }
      }
    }
  }
  return { width, height, data: out };
}
