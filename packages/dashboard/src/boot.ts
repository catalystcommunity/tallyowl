// What the document loads.
//
// It exists so that `index.ts` stays importable by a test without starting
// anything, and so that the document needs no inline script: the content
// security policy the head sends is `default-src 'self'`, which refuses one.

import { start } from "./index.ts";

void start();
