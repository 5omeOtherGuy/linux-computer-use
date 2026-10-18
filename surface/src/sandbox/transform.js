// REPL source transform: turns one notebook cell into a script that can use
// top-level await and keeps its top-level bindings across cells.
//
//   let/const/var x = v      -> globalThis-backed binding assigned in place
//   class C {}               -> C = class C {}
//   function f() {}          -> hoisted as usual, then published as globalThis.f
//   last expression          -> returned as the cell value
//
// The result is `(async () => { ... })()`, evaluated as a sloppy-mode global
// script inside QuickJS. Line numbers of the cell are preserved. Top-level
// `const` becomes a mutable global (documented REPL semantics).
import * as acorn from 'acorn';

function collectNames(node, out) {
  if (!node) return;
  switch (node.type) {
    case 'Identifier': out.push(node.name); break;
    case 'ObjectPattern':
      for (const p of node.properties) collectNames(p.type === 'RestElement' ? p.argument : p.value, out);
      break;
    case 'ArrayPattern': for (const e of node.elements) collectNames(e, out); break;
    case 'AssignmentPattern': collectNames(node.left, out); break;
    case 'RestElement': collectNames(node.argument, out); break;
    default: break;
  }
}

// Return the completion value of the cell's last statement, like a console:
// an expression, or the last expression inside a trailing block/if/try.
function markCompletion(stmt, edits) {
  if (!stmt) return;
  switch (stmt.type) {
    case 'ExpressionStatement':
      if (stmt.directive === 'use strict') return;
      edits.push({ start: stmt.expression.start, end: stmt.expression.start, text: 'return (' });
      edits.push({ start: stmt.expression.end, end: stmt.expression.end, text: ')' });
      return;
    case 'BlockStatement':
      markCompletion(stmt.body[stmt.body.length - 1], edits);
      return;
    case 'IfStatement':
      markCompletion(stmt.consequent, edits);
      markCompletion(stmt.alternate, edits);
      return;
    case 'TryStatement':
      markCompletion(stmt.block, edits);
      if (stmt.handler) markCompletion(stmt.handler.body, edits);
      return;
    default:
      return;
  }
}

export class CellSyntaxError extends Error {
  constructor(message) {
    super(message);
    this.name = 'SyntaxError';
  }
}

export function transformCell(code) {
  let ast;
  try {
    ast = acorn.parse(code, {
      ecmaVersion: 'latest',
      sourceType: 'script',
      allowAwaitOutsideFunction: true,
      allowReturnOutsideFunction: true,
      allowHashBang: false,
    });
  } catch (err) {
    throw new CellSyntaxError(err.message);
  }
  const edits = [];
  const declared = [];
  const functions = [];
  const body = ast.body;
  for (let i = 0; i < body.length; i++) {
    const stmt = body[i];
    if (stmt.type === 'VariableDeclaration') {
      const parts = [];
      for (const d of stmt.declarations) {
        collectNames(d.id, declared);
        const lhs = code.slice(d.id.start, d.id.end);
        if (d.init) parts.push(`(${lhs} = (${code.slice(d.init.start, d.init.end)}))`);
        else if (stmt.kind !== 'var' && d.id.type === 'Identifier') parts.push(`(${lhs} = undefined)`);
      }
      edits.push({ start: stmt.start, end: stmt.end, text: parts.length ? `;(${parts.join(', ')});` : ';' });
    } else if (stmt.type === 'ClassDeclaration') {
      declared.push(stmt.id.name);
      edits.push({ start: stmt.start, end: stmt.start, text: `;${stmt.id.name} = ` });
      edits.push({ start: stmt.end, end: stmt.end, text: ';' });
    } else if (stmt.type === 'FunctionDeclaration') {
      functions.push(stmt.id.name);
    } else if (i === body.length - 1) {
      markCompletion(stmt, edits);
    }
  }
  edits.sort((a, b) => b.start - a.start || b.end - a.end);
  let out = code;
  for (const e of edits) out = out.slice(0, e.start) + e.text + out.slice(e.end);
  const unique = [...new Set(declared)];
  const prologue = [
    unique.length
      ? `for (const __n of ${JSON.stringify(unique)}) if (!Object.prototype.hasOwnProperty.call(globalThis, __n)) globalThis[__n] = undefined;`
      : '',
    ...functions.map((f) => `globalThis[${JSON.stringify(f)}] = ${f};`),
  ].join(' ');
  // Everything added stays on the first and last line so error line numbers
  // match the cell source.
  return `(async () => { ${prologue} ${out}\n})()`;
}
