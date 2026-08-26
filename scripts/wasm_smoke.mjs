// Executes the compiled wasm_smoke example against Node's WASM runtime.
// Usage: node scripts/wasm_smoke.mjs target/wasm32-unknown-unknown/debug/examples/wasm_smoke.wasm
import { readFileSync } from "node:fs";
import process from "node:process";

const path = process.argv[2];
if (!path) {
  console.error("usage: node scripts/wasm_smoke.mjs <wasm-file>");
  process.exit(2);
}

const bytes = readFileSync(path);
const { instance } = await WebAssembly.instantiate(bytes, {});
const smoke = instance.exports.allox_wasm_smoke;
if (!smoke) {
  console.error("allox_wasm_smoke export missing");
  process.exit(2);
}
const rc = smoke();
if (rc !== 0) {
  console.error(`WASM smoke FAILED with code ${rc}`);
  process.exit(1);
}
console.log("WASM smoke passed");
