// Error type shared by the engine client, host API, sandbox runner and adapters.
// `code` is a stable machine-readable string; `data` is optional JSON data.
export class CuaError extends Error {
  constructor(code, message, data) {
    super(message);
    this.name = 'CuaError';
    this.code = code;
    if (data !== undefined) this.data = data;
  }

  toJSON() {
    const out = { code: this.code, message: this.message };
    if (this.data !== undefined) out.data = this.data;
    return out;
  }
}

export function toCuaError(err) {
  if (err instanceof CuaError) return err;
  if (err && err.name === 'AbortError') return new CuaError('cancelled', 'operation cancelled');
  return new CuaError('internal_error', err && err.message ? err.message : String(err));
}
