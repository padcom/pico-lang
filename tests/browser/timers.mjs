import init from "./timers.js";

const out = [];
const app = await init({ stdout: (b) => out.push(new TextDecoder().decode(b)) });
console.log("init returned");
console.log("exit", await app.done);
process.stdout.write(out.join(""));
