// the second and last piece of hand-written javascript: instantiate the same
// wasm module inside the worker and hand control to rust. three lines.
import init, { boot_worker } from "./pkg/vanish.js";
await init();
boot_worker();
