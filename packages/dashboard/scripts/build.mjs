// Build what the head serves: one script, one stylesheet, and the icon.
//
// The script is `src/boot.ts` with the dashboard and the Zest components it
// uses, in one file. A page with no bundle would load each Zest module as its
// own request, and the content security policy is `default-src 'self'`, so an
// inline import map cannot resolve the package names. The stylesheet holds the
// Zest tokens and `static/dashboard.css`.
//
// The CSS target keeps `light-dark()`. An older target rewrites it to a media
// query for the whole page, and the Zest colors then stop following
// `color-scheme`.

import { build } from "esbuild";
import { copyFileSync, mkdirSync, rmSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const packageRoot = join(dirname(fileURLToPath(import.meta.url)), "..");
const out = join(packageRoot, "dist", "app");

rmSync(out, { recursive: true, force: true });
mkdirSync(out, { recursive: true });

await build({
  entryPoints: { dashboard: join(packageRoot, "src", "boot.ts") },
  outdir: out,
  bundle: true,
  format: "esm",
  target: "es2022",
  minify: true,
  legalComments: "eof",
  logLevel: "warning",
  // The generated contract's package.json lists its export conditions in an
  // order esbuild warns about. The dashboard imports its source files by path,
  // not through that manifest, and generated code is not edited by hand.
  logOverride: { "package.json": "silent" },
});

await build({
  entryPoints: { dashboard: join(packageRoot, "static", "dashboard.css") },
  outdir: out,
  bundle: true,
  target: ["chrome123", "firefox120", "safari17.5"],
  minify: true,
  logLevel: "warning",
});

copyFileSync(join(packageRoot, "static", "owl.svg"), join(out, "owl.svg"));
