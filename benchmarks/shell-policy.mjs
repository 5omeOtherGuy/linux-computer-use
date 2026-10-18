// Rule check for the agent-desktop controller: every shell command an agent
// runs must consist only of `agent-desktop` invocations on the benchmark's
// workspace. The same check applies to Codex (which executes first, so a
// violation is detected afterwards) and Claude (whose permission rules deny
// most violations up front; the attempt still counts).
//
// The parser understands what agents actually write: quotes, backslashes,
// `&&`/`||`/`;`/newline chains, pipes, heredocs and redirections. Anything
// it cannot classify as an allowed agent-desktop call is reported.

const FORBIDDEN_SUBCOMMANDS = new Set(['new', 'close', 'view']);
const NO_WORKSPACE_SUBCOMMANDS = new Set(['list', 'help', '--help', '-h']);

// Splits a shell command into simple commands. Each segment carries its words
// and the issues found while reading it (substitutions, file redirections).
export function parseShell(src) {
  const segments = [];
  let seg = { words: [], issues: [], pipedFrom: false };
  let word = null;
  let pendingHeredocs = [];
  const endWord = () => {
    if (word !== null) seg.words.push(word);
    word = null;
  };
  const endSegment = (piped = false) => {
    endWord();
    if (seg.words.length || seg.issues.length) segments.push(seg);
    seg = { words: [], issues: [], pipedFrom: piped };
  };
  let i = 0;
  const readWordAt = () => {
    // A redirection or heredoc target: one shell word.
    while (src[i] === ' ' || src[i] === '\t') i++;
    let w = '';
    while (i < src.length && !/[\s;&|<>]/.test(src[i])) {
      if (src[i] === "'") {
        const j = src.indexOf("'", i + 1);
        w += src.slice(i + 1, j < 0 ? src.length : j);
        i = j < 0 ? src.length : j + 1;
      } else if (src[i] === '"') {
        let j = i + 1;
        while (j < src.length && src[j] !== '"') j += src[j] === '\\' ? 2 : 1;
        w += src.slice(i + 1, j);
        i = j + 1;
      } else w += src[i++];
    }
    return w;
  };
  while (i < src.length) {
    const c = src[i];
    if (c === '\n') {
      endSegment();
      i++;
      for (const { delim, strip } of pendingHeredocs) {
        // Skip the heredoc body up to its delimiter line.
        for (;;) {
          const nl = src.indexOf('\n', i);
          const line = src.slice(i, nl < 0 ? src.length : nl);
          i = nl < 0 ? src.length : nl + 1;
          if ((strip ? line.replace(/^\t+/, '') : line) === delim || nl < 0) break;
        }
      }
      pendingHeredocs = [];
    } else if (c === ' ' || c === '\t') {
      endWord();
      i++;
    } else if (c === '#' && word === null) {
      while (i < src.length && src[i] !== '\n') i++;
    } else if (c === ';' || c === '&' || c === '|') {
      const two = src.slice(i, i + 2);
      if (c === '&' && src[i + 1] === '>') {
        seg.issues.push('redirection to a file');
        i += 2;
        readWordAt();
        continue;
      }
      endSegment(c === '|' && two !== '||');
      i += two === '&&' || two === '||' || two === ';;' ? 2 : 1;
    } else if (c === '<' || c === '>') {
      const fd = word !== null && /^\d+$/.test(word) ? word : null;
      if (fd !== null) word = null;
      if (src.startsWith('<<<', i)) {
        i += 3;
        readWordAt(); // here-string: stdin data
      } else if (src.startsWith('<<', i)) {
        i += 2;
        const strip = src[i] === '-';
        if (strip) i++;
        pendingHeredocs.push({ delim: readWordAt(), strip });
      } else {
        i += src[i + 1] === '>' || src[i + 1] === '&' || src[i + 1] === '|' ? 2 : 1;
        const dup = src[i - 1] === '&';
        const target = readWordAt();
        if (!(dup && /^(\d+|-)$/.test(target)) && target !== '/dev/null') seg.issues.push(`redirection ${c} ${target}`);
      }
    } else if (c === "'") {
      const j = src.indexOf("'", i + 1);
      word = (word ?? '') + src.slice(i + 1, j < 0 ? src.length : j);
      i = j < 0 ? src.length : j + 1;
    } else if (c === '"') {
      let j = i + 1;
      let text = '';
      while (j < src.length && src[j] !== '"') {
        if (src[j] === '\\' && '"\\$`\n'.includes(src[j + 1])) {
          if (src[j + 1] !== '\n') text += src[j + 1];
          j += 2;
          continue;
        }
        if (src[j] === '`' || src.startsWith('$(', j)) seg.issues.push('command substitution');
        text += src[j++];
      }
      word = (word ?? '') + text;
      i = j + 1;
    } else if (c === '\\') {
      if (src[i + 1] !== '\n') word = (word ?? '') + (src[i + 1] ?? '');
      i += 2;
    } else if (c === '`' || src.startsWith('$(', i) || src.startsWith('<(', i) || src.startsWith('>(', i)) {
      seg.issues.push('command or process substitution');
      word = (word ?? '') + c;
      i++;
    } else {
      word = (word ?? '') + c;
      i++;
    }
  }
  endSegment();
  return segments;
}

// Violations in one command string; [] when it is allowed. `apps` lists the
// program names that `agent-desktop run W -- PROGRAM` may (re)launch.
export function checkCommand(command, ws, apps = []) {
  let segments = parseShell(command);
  // Codex reports commands as `/bin/bash -lc "<script>"`.
  if (segments.length === 1 && /(^|\/)(ba)?sh$/.test(segments[0].words[0] ?? '') && /^-l?c$/.test(segments[0].words[1] ?? '') && segments[0].words.length === 3) {
    segments = parseShell(segments[0].words[2]);
  }
  const vars = {};
  const resolve = (w) => w.replace(/\$\{?([A-Za-z_][A-Za-z0-9_]*)\}?/g, (m, n) => vars[n] ?? m);
  const out = [];
  for (const s of segments) {
    for (const issue of s.issues) out.push(`${issue}: ${command.slice(0, 200)}`);
    let words = s.words;
    while (words.length && /^[A-Za-z_][A-Za-z0-9_]*=/.test(words[0])) {
      const [k, ...v] = words[0].split('=');
      vars[k] = resolve(v.join('='));
      words = words.slice(1);
    }
    if (!words.length) continue;
    const prog = words[0];
    if (!/(^|\/)agent-desktop$/.test(prog)) {
      out.push(`${s.pipedFrom ? 'pipe into ' : ''}non-agent-desktop command \`${prog}\``);
      continue;
    }
    const sub = words[1] ?? '';
    if (FORBIDDEN_SUBCOMMANDS.has(sub)) out.push(`forbidden \`agent-desktop ${sub}\``);
    else if (sub && !NO_WORKSPACE_SUBCOMMANDS.has(sub) && resolve(words[2] ?? '') !== ws) out.push(`\`agent-desktop ${sub}\` on workspace \`${resolve(words[2] ?? '')}\`, not ${ws}`);
    if (sub === 'run') {
      const k = words.indexOf('--');
      const program = (k >= 0 ? words[k + 1] : '') ?? '';
      if (!apps.includes(program.split('/').pop())) out.push(`\`agent-desktop run\` of \`${program}\`, not one of the task's applications`);
    }
  }
  return out;
}
