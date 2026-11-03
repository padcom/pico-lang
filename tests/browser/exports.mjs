// Drives exports.ppl's `@export("js")` functions through the generated glue.
import init from "./exports.js";

const app = await init();
console.log(app.add(2, 40), app.greet("wörld"), JSON.stringify(app.mid({ x: 1, y: 2 }, { x: 3, y: 6 })));
console.log(JSON.stringify(app.describe({ label: "a", at: { x: 1.5, y: -1 }, count: 21, ok: true })));
const c = app.newCounter();
app.bump(c);
console.log(app.bump(c), c instanceof app.Counter);
c.free();
try {
  app.bump(c);
} catch (e) {
  console.log(e.message);
}
try {
  app.greet(42);
} catch (e) {
  console.log(e.message);
}
console.log("exit", await app.done);
app.later(10);
await new Promise((r) => setTimeout(r, 50));
console.log("end");
