// Runs apex-wasm-smoke as wasm32-unknown-unknown under Node (ADR-020).
//
//   cargo build --release --target wasm32-unknown-unknown -p apex-wasm-smoke
//   node crates/apex-wasm-smoke/run.mjs
//
// Fails when the module traps (a std API with no implementation on the target panics into
// `unreachable`) or when its hash differs from the one the native test pins.
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

const here = dirname(fileURLToPath(import.meta.url));
const wasmPath = join(here, '../../target/wasm32-unknown-unknown/release/apex_wasm_smoke.wasm');
const expected = readFileSync(join(here, 'expected_hash.txt'), 'utf8').trim();

const { instance } = await WebAssembly.instantiate(readFileSync(wasmPath), {});
let hash;
try {
  hash = BigInt.asUintN(64, instance.exports.smoke_run()).toString(16).padStart(16, '0');
} catch (e) {
  console.error(`apex-wasm-smoke: the module trapped: ${e.message}`);
  process.exit(1);
}
if (hash !== expected) {
  console.error(`apex-wasm-smoke: wasm hash ${hash} != native ${expected}`);
  process.exit(1);
}
console.log(`apex-wasm-smoke: wasm hash ${hash} == native`);
