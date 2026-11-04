import init from "./js_values.js";

const app = await init();
const code = await app.done;
// (The JavaScript timer isn't the pico event loop's: it comes after.)
await new Promise((r) => setTimeout(r, 100));
console.log("exit", code);
