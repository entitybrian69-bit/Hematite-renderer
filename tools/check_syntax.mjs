#!/usr/bin/env node
// Verify that every Rust file parses, using tree-sitter-rust.
//
// This is NOT a substitute for `cargo check`. It proves syntax only: names, types, borrows
// and test outcomes are all unchecked. It exists because the development environment had no
// Rust toolchain (see docs/ENVIRONMENT.md), and a syntax check is still better than none —
// it caught a real "methods inside a struct body" error during development.
//
// Setup:
//   npm install tree-sitter tree-sitter-rust
// Usage:
//   node tools/check_syntax.mjs $(git ls-files '*.rs')
//
// Exit code is 1 if any file has a syntax error.

import Parser from 'tree-sitter';
import Rust from 'tree-sitter-rust';
import { readFileSync } from 'node:fs';

const files = process.argv.slice(2);
if (files.length === 0) {
  console.error('usage: node tools/check_syntax.mjs <file.rs>...');
  process.exit(2);
}

const parser = new Parser();
parser.setLanguage(Rust);

let bad = 0;

for (const file of files) {
  const src = readFileSync(file, 'utf8');
  const tree = parser.parse(src);

  const problems = [];
  const walk = (node) => {
    if (node.type === 'ERROR' || node.isMissing) {
      problems.push({
        line: node.startPosition.row + 1,
        column: node.startPosition.column + 1,
        kind: node.isMissing ? `missing ${node.type}` : 'unexpected syntax',
        snippet: src.slice(node.startIndex, Math.min(node.endIndex, node.startIndex + 70)),
      });
    }
    for (const child of node.children) walk(child);
  };
  walk(tree.rootNode);

  if (problems.length > 0) {
    bad += 1;
    console.error(`\n${file}: ${problems.length} syntax problem(s)`);
    const seen = new Set();
    for (const p of problems) {
      const key = `${p.line}:${p.column}`;
      if (seen.has(key)) continue;
      seen.add(key);
      console.error(`  ${p.line}:${p.column} ${p.kind}`);
      console.error(`    ${JSON.stringify(p.snippet)}`);
    }
  }
}

const ok = files.length - bad;
console.log(`\n${ok}/${files.length} files parse cleanly`);

if (bad > 0) {
  console.error(
    '\nNote: this only checks syntax. Run `cargo check --workspace` for anything meaningful.'
  );
  process.exit(1);
}
