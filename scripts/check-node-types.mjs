#!/usr/bin/env node
// Compare each Node binding's `index.d.ts` with what the built module really
// exports: the same function names, the same number of parameters, and for a
// class the same methods. It also requires `catch_unwind` on every `#[napi]`
// function, method and constructor, so a Rust panic is a thrown error. A declaration nobody can call, or an export nobody
// declared, fails the run. Needs the `.node` binaries (scripts/build-node.sh).
//
// Usage: node scripts/check-node-types.mjs [crate-dir ...]
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';
import { readFileSync, readdirSync, existsSync } from 'node:fs';
import path from 'node:path';

const require = createRequire(import.meta.url);
const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');

// Number of parameters in a parameter list, counting only top-level commas.
function arity(params) {
  const text = params.replace(/\/\*[\s\S]*?\*\/|\/\/[^\n]*/g, '').trim();
  if (!text) return 0;
  let depth = 0;
  let count = 1;
  for (const ch of text) {
    if ('([{<'.includes(ch)) depth += 1;
    else if (')]}>'.includes(ch)) depth -= 1;
    else if (ch === ',' && depth === 0) count += 1;
  }
  return /,\s*$/.test(text) ? count - 1 : count;
}

// The text between the parenthesis at `open` and its match.
function parenthesized(text, open) {
  let depth = 0;
  for (let i = open; i < text.length; i += 1) {
    if (text[i] === '(') depth += 1;
    else if (text[i] === ')' && (depth -= 1) === 0) return text.slice(open + 1, i);
  }
  throw new Error('unbalanced parameter list');
}

const camel = (snake) => snake.replace(/_([a-z0-9])/g, (_, c) => c.toUpperCase());

// Every `#[napi]` function, method and constructor in a crate: its parameter
// count and whether it is built with `catch_unwind`. Free functions are keyed
// by JavaScript name, methods by `Class.method`, constructors by
// `Class.constructor`, so a method never answers for a function of the same name.
function rustItems(source) {
  const impls = [];
  const implRe = /#\[napi\]\s*impl (\w+)\s*\{/g;
  for (let m = implRe.exec(source); m; m = implRe.exec(source)) {
    let depth = 0;
    let end = implRe.lastIndex - 1;
    for (; end < source.length; end += 1) {
      if (source[end] === '{') depth += 1;
      else if (source[end] === '}' && (depth -= 1) === 0) break;
    }
    impls.push({ name: m[1], start: m.index, end });
  }
  const out = new Map();
  const re = /#\[napi(?:\(([^)]*)\))?\]\s*pub fn (\w+)\s*\(/g;
  for (let m = re.exec(source); m; m = re.exec(source)) {
    const attrs = (m[1] || '').split(',').map((a) => a.trim());
    const params = parenthesized(source, re.lastIndex - 1)
      .split(',').map((p) => p.trim()).filter((p) => p && !/^&?(mut )?self$/.test(p));
    const owner = impls.find((i) => m.index > i.start && m.index < i.end);
    const name = attrs.includes('constructor') ? 'constructor' : camel(m[2]);
    out.set(owner ? `${owner.name}.${name}` : name, {
      rustName: owner ? `${owner.name}::${m[2]}` : m[2],
      arity: arity(params.join(',')),
      catchUnwind: attrs.includes('catch_unwind'),
    });
  }
  return out;
}

function check(crate) {
  const dir = path.join(root, crate);
  const problems = [];
  const dts = readFileSync(path.join(dir, 'index.d.ts'), 'utf8');
  const js = readFileSync(path.join(dir, 'index.js'), 'utf8');
  const rust = rustItems(readFileSync(path.join(dir, 'src', 'lib.rs'), 'utf8'));
  // A panic that reaches Node-API unwinds out of an `extern "C"` function and
  // ends the process. `catch_unwind` turns it into a thrown error instead.
  for (const item of rust.values()) {
    if (!item.catchUnwind) problems.push(`${item.rustName}: #[napi] without catch_unwind, so a panic would end the process`);
  }
  const mod = require(dir);

  const declared = new Map();
  const fnRe = /^export (?:declare )?function (\w+)\s*\(/gm;
  for (let m = fnRe.exec(dts); m; m = fnRe.exec(dts)) {
    declared.set(m[1], arity(parenthesized(dts, fnRe.lastIndex - 1)));
  }
  const classes = new Map();
  const classRe = /^export (?:declare )?class (\w+)\s*\{([\s\S]*?)^\}/gm;
  for (let m = classRe.exec(dts); m; m = classRe.exec(dts)) {
    const methods = new Map();
    const body = m[2].replace(/\/\*[\s\S]*?\*\//g, '');
    const methodRe = /^\s*(\w+)\s*\(/gm;
    for (let mm = methodRe.exec(body); mm; mm = methodRe.exec(body)) {
      methods.set(mm[1], arity(parenthesized(body, methodRe.lastIndex - 1)));
    }
    classes.set(m[1], methods);
  }

  const exported = Object.keys(mod);
  const functions = exported.filter((k) => typeof mod[k] === 'function' && !/^class\b/.test(Function.prototype.toString.call(mod[k])) && !classes.has(k));
  for (const name of functions) if (!declared.has(name)) problems.push(`${name} is exported but not declared`);
  for (const name of declared.keys()) if (!functions.includes(name)) problems.push(`${name} is declared but not exported`);
  for (const name of classes.keys()) if (typeof mod[name] !== 'function') problems.push(`class ${name} is declared but not exported`);
  for (const name of exported) {
    if (typeof mod[name] !== 'function') problems.push(`${name} is exported and is not a function`);
  }

  for (const name of functions.filter((n) => declared.has(n))) {
    const text = Function.prototype.toString.call(mod[name]);
    let real;
    if (text.includes('[native code]')) {
      // Re-exported straight from the native module: count the Rust parameters.
      const native = new RegExp(`\\b${name}\\s*:\\s*native\\.(\\w+)`).exec(js);
      const item = rust.get(native ? native[1] : name);
      if (item === undefined) { problems.push(`${name}: no #[napi] function found for it`); continue; }
      real = item.arity;
    } else {
      real = arity(parenthesized(text, text.indexOf('(')));
    }
    if (real !== declared.get(name)) problems.push(`${name}: declared with ${declared.get(name)} parameter(s), takes ${real}`);
  }

  for (const [name, methods] of classes) {
    if (typeof mod[name] !== 'function') continue;
    const real = Object.getOwnPropertyNames(mod[name].prototype).filter((k) => k !== 'constructor');
    for (const method of real) if (!methods.has(method)) problems.push(`${name}.${method} exists but is not declared`);
    for (const [method, count] of methods) {
      if (method !== 'constructor' && !real.includes(method)) { problems.push(`${name}.${method} is declared but does not exist`); continue; }
      const item = rust.get(`${name}.${method}`);
      if (item === undefined) { problems.push(`${name}.${method}: no #[napi] method found for it`); continue; }
      if (item.arity !== count) problems.push(`${name}.${method}: declared with ${count} parameter(s), takes ${item.arity}`);
    }
  }
  return { problems, summary: `${functions.length} function(s), ${classes.size} class(es)` };
}

const wanted = process.argv.slice(2).length
  ? process.argv.slice(2)
  : readdirSync(root).filter((d) => d.endsWith('-node') && existsSync(path.join(root, d, 'index.d.ts'))).sort();
let failures = 0;
for (const crate of wanted) {
  try {
    const { problems, summary } = check(crate);
    if (problems.length) {
      failures += 1;
      console.error(`FAIL ${crate}:\n  ${problems.join('\n  ')}`);
    } else {
      console.log(`ok ${crate}: index.d.ts matches the module (${summary})`);
    }
  } catch (err) {
    failures += 1;
    console.error(`FAIL ${crate}: ${err && err.stack ? err.stack : err}`);
  }
}
if (failures) { console.error(`${failures} package(s) with type mismatches`); process.exit(1); }
console.log('all node-binding type declarations match');
