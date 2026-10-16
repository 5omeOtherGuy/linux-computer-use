// Benchmark task catalogue. Each task launches real applications on a fresh
// private desktop, gives the agent a goal (never a solution), and verifies the
// outcome from the filesystem afterwards, independently of the agent's claims.
import { readFile, readdir, stat } from 'node:fs/promises';
import { join } from 'node:path';

const exists = (p) => stat(p).then(() => true, () => false);
const read = (p) => readFile(p, 'utf8').catch(() => null);

// Same goal as the agent-desktop "half time" native trial (native4/native5),
// so results compare directly with that controller on the same model.
const reimburse = {
  id: 'reimburse',
  description: 'Calculator -> Xed draft -> Thunar folder/move/rename -> Xed reopen',
  apps: [
    { cmd: ['gnome-calculator'], waitClass: 'gnome-calculator' },
    { cmd: ['xed', '--new-window'], waitClass: 'xed' },
    { cmd: ['thunar', '{out}'], waitClass: 'thunar' },
  ],
  allowApps: ['gnome-calculator', 'xed', 'thunar'],
  prompt: (out) => `A personal assistant needs a reimbursement email draft prepared and filed.
Expense amounts: train 125.50 EUR, hotel deposit 78.25 EUR, taxi 19.95 EUR. Compute their sum plus 19% using the open Calculator application. Round the result to two decimal places in the draft. No mental, programmatic or shell arithmetic.
Use the open Xed editor to write an unsent email draft addressed to travel@example.invalid with subject "Travel reimbursement - October visit". Include the three expense amounts, the subtotal, the total including 19%, and a request to confirm receipt. Save it as reimbursement-draft.eml in ${out}.
Then use the open Thunar file manager to create a folder named Ready in ${out}, move the draft into it, and rename it october-reimbursement.eml. Reopen that final file in Xed and verify the text persisted. Finally report the Calculator display and the final path.
Do not send anything and do not access other files.`,
  async verify(out) {
    const final = join(out, 'Ready', 'october-reimbursement.eml');
    const text = await read(final);
    const checks = {
      final_file: text !== null,
      original_removed: !(await exists(join(out, 'reimbursement-draft.eml'))),
      recipient: !!text?.includes('travel@example.invalid'),
      subject: !!text?.includes('Travel reimbursement - October visit'),
      amounts: ['125.50', '78.25', '19.95'].every((a) => text?.includes(a)),
      subtotal: !!text && /223[.,]70?\b/.test(text),
      total: !!text?.includes('266.20'),
    };
    return { success: Object.values(checks).every(Boolean), checks };
  },
};

// The note task from the first model-driven demo of this engine (Codex
// GPT-6.1 medium over raw RPC: 71.43 s task time, 17 actions).
const xedNote = {
  id: 'xed-note',
  description: 'Xed: write, save, close, reopen, revise, save, reopen and verify a note',
  apps: [{ cmd: ['xed', '--new-window'], waitClass: 'xed' }],
  allowApps: ['xed'],
  prompt: (out) => `In the open Xed editor, write a shift handoff note with exactly these lines:
Shift handoff - Line 3
[ ] Restock labels
[ ] Calibrate scale
Owner: Dana
Status: open
Save it as handoff.txt in ${out} and close the document. Reopen ${out}/handoff.txt in Xed, mark both items done by changing "[ ]" to "[x]", change "Status: open" to "Status: closed", save, close it, reopen it once more and confirm the saved text. Report the final text.`,
  async verify(out) {
    const text = (await read(join(out, 'handoff.txt'))) ?? '';
    const lines = text.trim().split(/\r?\n/).map((l) => l.trim());
    const checks = {
      file: text.length > 0,
      title: lines[0] === 'Shift handoff - Line 3',
      done: lines.includes('[x] Restock labels') && lines.includes('[x] Calibrate scale'),
      owner: lines.includes('Owner: Dana'),
      status: lines.includes('Status: closed') && !lines.includes('Status: open'),
      no_extra_files: (await readdir(out)).filter((f) => !f.startsWith('.')).length === 1,
    };
    return { success: Object.values(checks).every(Boolean), checks };
  },
};

// A canvas app: pointer drags draw, the toolbar and colour choice are
// widgets, saving goes through a file chooser. Verified from the saved PNG:
// something non-trivial in at least two colours.
const paint = {
  id: 'paint',
  description: 'Drawing: draw a house with a roof, a door and a sun in at least two colours; save as PNG',
  apps: [{ cmd: ['drawing'], waitClass: 'drawing' }],
  allowApps: ['drawing'],
  prompt: (out) => `In the open Drawing application, draw a simple picture of a house: a square body, a triangular roof, a door, and a sun in the sky. Use at least two different colours. Then save the picture as house.png in ${out} and report what you drew and which tools you used.`,
  async verify(out) {
    const file = join(out, 'house.png');
    const checks = { file: await exists(file), decodes: false, drawn: false, two_colours: false };
    if (checks.file) {
      try {
        const { PNG } = await import('../surface/node_modules/pngjs/lib/png.js');
        const png = PNG.sync.read(await readFile(file));
        checks.decodes = png.width > 0 && png.height > 0;
        const colours = new Map();
        for (let i = 0; i < png.data.length; i += 4) {
          const key = `${png.data[i] >> 5},${png.data[i + 1] >> 5},${png.data[i + 2] >> 5}`;
          colours.set(key, (colours.get(key) ?? 0) + 1);
        }
        const total = png.width * png.height;
        const sorted = [...colours.values()].sort((a, b) => b - a);
        // The background is the largest colour; the drawing is the rest.
        const drawn = total - (sorted[0] ?? total);
        checks.drawn = drawn >= total * 0.01;
        checks.two_colours = sorted.filter((n) => n >= total * 0.002).length >= 3;
      } catch {
        // not a PNG
      }
    }
    return { success: Object.values(checks).every(Boolean), checks };
  },
};

export const TASKS = Object.fromEntries([reimburse, xedNote, paint].map((t) => [t.id, t]));
