// Regenerate the checked-in SPIR-V from the original GLSL sources and compare.
//
//   node tools/compile_shaders.mjs
//
// The GLSL in crates/hematite-vk-backend/shaders is the source of truth; the .spv files
// are what the Rust backend embeds with include_bytes!. This script proves the two agree,
// so a stale binary can never ship alongside edited shader source.
//
// The compiler is the pinned glslang WebAssembly build fetched by
// tools/setup_android_tools.py into .cache/vulkan (git-ignored). Both shaders are
// original Hematite sources: no shader pack or third-party shader is redistributed.

import { createRequire } from 'node:module';
import { readFileSync, writeFileSync, existsSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const glslangPath = resolve(root, '.cache/vulkan/glslang/package/dist/node-devel/glslang.js');
if (!existsSync(glslangPath)) {
  console.error(`missing ${glslangPath}`);
  console.error('run: python3 tools/setup_android_tools.py');
  process.exit(2);
}

const require = createRequire(import.meta.url);
const glslang = require(glslangPath)();

const shaders = [
  { file: 'offscreen.vert', stage: 'vertex' },
  { file: 'offscreen.frag', stage: 'fragment' },
];

const directory = resolve(root, 'crates/hematite-vk-backend/shaders');
const update = process.argv.includes('--update');
let failures = 0;

for (const shader of shaders) {
  const source = readFileSync(resolve(directory, shader.file), 'utf8');
  const words = glslang.compileGLSL(source, shader.stage);
  const compiled = Buffer.from(words.buffer, words.byteOffset, words.byteLength);
  const target = resolve(directory, `${shader.file}.spv`);
  if (!existsSync(target)) {
    writeFileSync(target, compiled);
    console.log(`wrote ${shader.file}.spv (${compiled.length} bytes)`);
    continue;
  }
  const checkedIn = readFileSync(target);
  if (checkedIn.equals(compiled)) {
    console.log(`ok: ${shader.file}.spv matches ${shader.file} (${compiled.length} bytes, `
      + `${words.length} SPIR-V words)`);
  } else if (update) {
    writeFileSync(target, compiled);
    console.log(`updated ${shader.file}.spv (${compiled.length} bytes)`);
  } else {
    console.error(`FAIL: ${shader.file}.spv differs from ${shader.file} `
      + `(checked in ${checkedIn.length} bytes, compiled ${compiled.length} bytes)`);
    console.error('re-run with --update after reviewing the shader change');
    failures += 1;
  }
}

process.exit(failures === 0 ? 0 : 1);
