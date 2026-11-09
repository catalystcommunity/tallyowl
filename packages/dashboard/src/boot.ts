// What the document loads.
//
// It exists so that `index.ts` stays importable by a test without starting
// anything, and so that the document needs no inline script: the content
// security policy the head sends is `default-src 'self'`, which refuses one.
// The build bundles this file, the dashboard, and the Zest components it
// uses into one script.

import { start } from "./index.ts";
import { defineDashboardElements } from "./zest.ts";

defineDashboardElements();
void start();
