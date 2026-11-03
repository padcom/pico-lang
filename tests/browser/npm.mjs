import init from "./npm.js";

const app = await init();
console.log("exit", await app.done);
