import { mkdtempSync, readFileSync, existsSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { Client } from '@modelcontextprotocol/sdk/client/index.js';
import { StdioClientTransport } from '@modelcontextprotocol/sdk/client/stdio.js';
import { ElicitRequestSchema } from '@modelcontextprotocol/sdk/types.js';

const here = path.dirname(fileURLToPath(import.meta.url));
export const SURFACE = path.resolve(here, '..');
export const FAKE_ENGINE = path.join(here, 'fake-engine.js');
export const BIN = path.join(SURFACE, 'bin', 'linux-cua-mcp.js');

export const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

export function engineLog(file) {
  if (!existsSync(file)) return [];
  return readFileSync(file, 'utf8').trim().split('\n').filter(Boolean).map((l) => JSON.parse(l));
}

// Start the MCP server (bin) against the fake engine with an elicitation-capable client.
export async function startClient({ env = {}, decide = () => 'accept', elicitation = true } = {}) {
  const dir = mkdtempSync(path.join(tmpdir(), 'cua-surface-test-'));
  const log = path.join(dir, 'engine.log');
  const elicitations = [];
  const transport = new StdioClientTransport({
    command: process.execPath,
    args: [BIN],
    env: { PATH: process.env.PATH, CUA_ENGINE: FAKE_ENGINE, CUA_DISPLAY: ':99', FAKE_ENGINE_LOG: log, ...env },
    stderr: 'pipe',
  });
  let stderr = '';
  transport.stderr?.on('data', (d) => (stderr += d));
  const client = new Client({ name: 'surface-test', version: '0.0.0' }, { capabilities: elicitation ? { elicitation: { form: {} } } : {} });
  if (elicitation) {
    client.setRequestHandler(ElicitRequestSchema, async (req) => {
      elicitations.push(req.params);
      const d = await decide(req.params);
      if (d === 'accept') return { action: 'accept', content: { approve: true } };
      if (d === 'accept-false') return { action: 'accept', content: { approve: false } };
      return { action: d };
    });
  }
  await client.connect(transport);
  return {
    client,
    elicitations,
    log,
    events: () => engineLog(log),
    performed: () => engineLog(log).filter((e) => e.event === 'performed'),
    stderr: () => stderr,
    js: (code, extra = {}, opts) => client.callTool({ name: 'js', arguments: { code, ...extra } }, undefined, opts),
    text: (res) => res.content.filter((c) => c.type === 'text').map((c) => c.text).join('\n'),
    async close() {
      await client.close();
      rmSync(dir, { recursive: true, force: true });
    },
  };
}
