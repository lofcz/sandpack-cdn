/**
 * E2E: fetch transformed packages from the live Priprava CDN proxy and assert
 * the v4 inline-helpers contract Sandpack needs for $csb$eval.
 */
import { createRequire } from "node:module";

const require = createRequire("C:/Users/mstagl-dev/Documents/sandpack-bundler/package.json");
const { decode } = require("@msgpack/msgpack");

const CDN = process.env.SANDPACK_CDN_URL ?? "http://localhost:44298/sandpack-cdn";
const CDN_VERSION = 5;
const enc = (p) => Buffer.from(`${CDN_VERSION}(${p})`).toString("base64");

const ESM_IMPORT = /(^|\n)\s*import\s/;
const ESM_EXPORT = /(^|\n)\s*export\s/;
const BARE_HELPER_CALL = /_interop_require_(?:wildcard|default)\s*\(/;
const HELPER_FN_DEF = /function\s+_interop_require_(?:wildcard|default)/;
const SWC_HELPERS = /@swc\/helpers/;

const PACKAGES = [
  ["lucide-react", "0.511.0"],
  ["lucide-react", "0.469.0"],
  ["lucide-react", "0.525.0"],
  ["@tailwindcss/browser", "4.3.3"],
  ["clsx", "2.1.1"],
  ["framer-motion", "12.0.0"],
  ["react", "19.2.7"],
];

function analyzeFile(path, file) {
  if (typeof file !== "object" || !file || typeof file.c !== "string") {
    return { path, skipped: true, reason: typeof file };
  }
  const c = file.c;
  const hasBareCall = BARE_HELPER_CALL.test(c);
  const hasDef = HELPER_FN_DEF.test(c);
  return {
    path,
    t: file.t,
    len: c.length,
    hasEsmImport: ESM_IMPORT.test(c),
    hasEsmExport: ESM_EXPORT.test(c),
    hasSwcHelpers: SWC_HELPERS.test(c),
    hasBareCall,
    hasDef,
    // Contract: if a helper is called, its body must be inlined (or unused).
    bareHelperWithoutDef: hasBareCall && !hasDef,
  };
}

async function fetchPkg(name, version) {
  const url = `${CDN.replace(/\/$/, "")}/package/${enc(`${name}@${version}`)}`;
  const res = await fetch(url);
  if (!res.ok) {
    throw new Error(`${name}@${version} HTTP ${res.status} ${url}`);
  }
  return decode(new Uint8Array(await res.arrayBuffer()));
}

function barrelPaths(mod) {
  return Object.keys(mod.f || {}).filter(
    (k) =>
      /lucide-react\.js$/.test(k) ||
      /\/index\.(m?js|cjs)$/.test(k) ||
      k === "index.js" ||
      k.endsWith("/browser.js") ||
      k.includes("dist/esm/lucide-react"),
  );
}

let failed = 0;
const summary = [];

console.log(`CDN: ${CDN}`);
console.log("---");

for (const [name, version] of PACKAGES) {
  const label = `${name}@${version}`;
  try {
    const mod = await fetchPkg(name, version);
    const files = Object.entries(mod.f || {});
    const analyses = [];
    let bare = 0;
    let esm = 0;
    let swc = 0;
    let jsFiles = 0;

    for (const [path, file] of files) {
      if (typeof file !== "object" || !file?.c) continue;
      if (!/\.(m?js|cjs)$/i.test(path)) continue;
      jsFiles++;
      const a = analyzeFile(path, file);
      analyses.push(a);
      if (a.bareHelperWithoutDef) bare++;
      if (a.hasEsmImport || a.hasEsmExport) esm++;
      if (a.hasSwcHelpers) swc++;
    }

    const barrels = barrelPaths(mod);
    const barrelReports = [];
    for (const bp of barrels.slice(0, 3)) {
      const file = mod.f[bp];
      if (typeof file === "object" && file?.c) {
        const a = analyzeFile(bp, file);
        barrelReports.push(a);
        console.log(
          `${label} barrel ${bp}: t=${a.t} swc=${a.hasSwcHelpers} bareWoDef=${a.bareHelperWithoutDef} esm=${a.hasEsmImport || a.hasEsmExport} def=${a.hasDef}`,
        );
        console.log(`  head: ${file.c.slice(0, 220).replace(/\n/g, "\\n")}`);
      }
    }

    const ok = bare === 0 && esm === 0 && swc === 0;
    if (!ok) failed++;
    summary.push({
      label,
      ok,
      jsFiles,
      bareHelperWithoutDef: bare,
      residualEsm: esm,
      swcHelpersRefs: swc,
      barrels: barrels.length,
    });
    console.log(
      `${ok ? "PASS" : "FAIL"} ${label}: js=${jsFiles} bareWoDef=${bare} esm=${esm} @swc/helpers=${swc}`,
    );
  } catch (e) {
    failed++;
    summary.push({ label, ok: false, error: e.message });
    console.log(`FAIL ${label}: ${e.message}`);
  }
  console.log("---");
}

console.log("\nSUMMARY");
for (const s of summary) {
  console.log(JSON.stringify(s));
}
console.log(failed === 0 ? "\nE2E OK" : `\nE2E FAILED (${failed})`);
process.exit(failed === 0 ? 0 : 1);
