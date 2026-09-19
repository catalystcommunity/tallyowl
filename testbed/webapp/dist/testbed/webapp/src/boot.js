// What the seedstore document loads.
//
// It exists so that `index.ts` stays importable by a test without starting
// anything, and so the document needs no inline script.
import { start } from "./index.js";
start();
