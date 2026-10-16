#!/usr/bin/env node
// MCP stdio entry point. stdout carries MCP only; diagnostics go to stderr.
import { StdioServerTransport } from '@modelcontextprotocol/sdk/server/stdio.js';
import { createSurface } from '../src/server.js';
import { DEFAULT_ENGINE, checkDisplay } from '../src/engine.js';
import { parseExtensions } from '../src/protocol.js';
import { DEFAULT_CONFIRMATION_POLICY } from '../src/host.js';
import path from 'node:path';

const log = (msg) => process.stderr.write(`[linux-cua-mcp] ${msg}\n`);

let surface;
try {
  const env = process.env;
  const display = checkDisplay(env.CUA_DISPLAY);
  // "Always allow" decisions; without a config location, only turn and session scopes are offered.
  const configHome = env.XDG_CONFIG_HOME || (env.HOME ? path.join(env.HOME, '.config') : null);
  const approvalsFile = env.CUA_APPROVALS_FILE || (configHome ? path.join(configHome, 'linux-cua', 'approvals.json') : undefined);
  surface = createSurface({
    engineOptions: { command: env.CUA_ENGINE || DEFAULT_ENGINE, display, env },
    extensions: parseExtensions(env.CUA_ENGINE_EXTENSIONS),
    policyText: env.CUA_CONFIRMATION_POLICY || DEFAULT_CONFIRMATION_POLICY,
    approvalsFile,
    toolTimeoutMs: env.CUA_TOOL_TIMEOUT_MS ? Number(env.CUA_TOOL_TIMEOUT_MS) : undefined,
    sandboxMemoryMb: env.CUA_SANDBOX_MEMORY_MB ? Number(env.CUA_SANDBOX_MEMORY_MB) : undefined,
    log,
  });
} catch (err) {
  log(`configuration error: ${err.message}`);
  process.exit(2);
}

const shutdown = async () => {
  try {
    await surface.close();
  } finally {
    process.exit(0);
  }
};
process.on('SIGINT', shutdown);
process.on('SIGTERM', shutdown);
process.stdin.on('end', shutdown);

await surface.server.connect(new StdioServerTransport());
log('ready');
