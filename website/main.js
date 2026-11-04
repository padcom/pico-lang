// Pico website: syntax highlighting for the code samples and the scroll-driven feature tour.
"use strict";

// --- Highlighting -------------------------------------------------------------------------------

const KEYWORDS = new Set(`
  abstract as async await break case catch class comptime const constructor continue decorator
  default delete do else enum export extends false final finally for from function get if immutable
  implements import in instanceof interface is let loop match new null of override private protected
  public readonly return set static struct super switch this throw throws true try type typeof using
  virtual void while yield
`.split(/\s+/).filter(Boolean));

const TYPES = new Set(`
  i8 i16 i32 i64 isize u8 u16 u32 u64 usize f32 f64 int number bool char string unknown never
`.split(/\s+/).filter(Boolean));

const escape = (s) => s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
const span = (cls, s) => `<span class="t-${cls}">${escape(s)}</span>`;

/** Highlights pico source (plain text) into HTML. */
function highlight(src) {
  let out = "";
  let i = 0;
  const n = src.length;
  while (i < n) {
    const rest = src.slice(i);
    let m;
    if ((m = /^\/\/[^\n]*/.exec(rest)) || (m = /^\/\*[\s\S]*?\*\//.exec(rest))) {
      out += span("com", m[0]);
      i += m[0].length;
    } else if (src[i] === "`") {
      // A template literal: the text is a string, `${…}` holes are highlighted as code.
      let j = i + 1;
      let text = "`";
      while (j < n && src[j] !== "`") {
        if (src[j] === "\\") { text += src.slice(j, j + 2); j += 2; continue; }
        if (src[j] === "$" && src[j + 1] === "{") {
          out += span("str", text);
          text = "";
          let depth = 1;
          let k = j + 2;
          while (k < n && depth > 0) {
            if (src[k] === "{") depth++;
            else if (src[k] === "}") depth--;
            if (depth > 0) k++;
          }
          out += span("kw", "${") + highlight(src.slice(j + 2, k)) + span("kw", "}");
          j = k + 1;
          continue;
        }
        text += src[j++];
      }
      out += span("str", text + (j < n ? "`" : ""));
      i = j + 1;
    } else if ((m = /^"(?:[^"\\\n]|\\.)*"|^'(?:[^'\\\n]|\\.)*'/.exec(rest))) {
      out += span("str", m[0]);
      i += m[0].length;
    } else if ((m = /^@[A-Za-z_][\w.]*/.exec(rest))) {
      out += span("dec", m[0]);
      i += m[0].length;
    } else if ((m = /^(?:0x[0-9A-Fa-f_]+|\d[\d_]*(?:\.\d[\d_]*)?(?:[eE][+-]?\d+)?)(?:[iu](?:8|16|32|64|size)|f32|f64)?/.exec(rest))) {
      out += span("num", m[0]);
      i += m[0].length;
    } else if ((m = /^[A-Za-z_$][\w$]*/.exec(rest))) {
      const w = m[0];
      const after = src.slice(i + w.length);
      if (KEYWORDS.has(w)) out += span("kw", w);
      else if (TYPES.has(w) || /^[A-Z]/.test(w)) out += span("type", w);
      else if (/^\s*(?:<[^>()]*>)?\(/.test(after)) out += span("fn", w);
      else out += escape(w);
      i += w.length;
    } else {
      out += escape(src[i]);
      i++;
    }
  }
  return out;
}

for (const code of document.querySelectorAll("pre.code:not(.term) code")) {
  code.innerHTML = highlight(code.textContent);
}
for (const code of document.querySelectorAll("pre.term code")) {
  code.innerHTML = code.textContent
    .split("\n")
    .map((line) => {
      const [cmd, comment] = line.split(/(?=#)/);
      return `<span class="t-kw">$</span> ${escape(cmd)}${comment ? span("com", comment) : ""}`;
    })
    .join("\n");
}

// --- Feature tour ---------------------------------------------------------------------------------

const steps = [...document.querySelectorAll(".step")];
const rail = document.querySelector(".tour-rail");
const panels = document.querySelector(".stage-panels");
const stageTitle = document.querySelector(".stage-title");

const links = steps.map((step) => {
  const li = document.createElement("li");
  const a = document.createElement("a");
  a.href = `#${step.id}`;
  a.textContent = step.dataset.title;
  li.append(a);
  rail.append(li);
  return a;
});

const stagePanels = steps.map((step) => {
  const pre = step.querySelector("pre.code").cloneNode(true);
  panels.append(pre);
  return pre;
});

// The stage is as tall as its longest sample, so switching samples never moves the page.
function sizeStage() {
  const tallest = Math.max(...stagePanels.map((p) => p.scrollHeight));
  panels.style.minHeight = `${tallest}px`;
  panels.parentElement.parentElement.style.setProperty("--stage-h", `${tallest + 40}px`);
}
sizeStage();
document.fonts?.ready.then(sizeStage);
addEventListener("resize", sizeStage);

let current = -1;
function activate(index) {
  if (index === current) return;
  current = index;
  steps.forEach((s, i) => s.classList.toggle("active", i === index));
  links.forEach((a, i) => a.classList.toggle("active", i === index));
  stagePanels.forEach((p, i) => p.classList.toggle("active", i === index));
  stageTitle.textContent = `${steps[index].id.slice(2)}.pi`;
}

// The active step is the last one whose top has passed the middle of the viewport. It's
// recomputed from the layout on every scroll, so fast jumps (the rail, the scrollbar thumb, a
// link with a #hash) always land on the right sample.
let pending = false;
function update() {
  pending = false;
  const middle = innerHeight / 2;
  let index = 0;
  steps.forEach((s, i) => {
    if (s.getBoundingClientRect().top <= middle) index = i;
  });
  activate(index);
}
function schedule() {
  if (!pending) {
    pending = true;
    requestAnimationFrame(update);
  }
}
addEventListener("scroll", schedule, { passive: true, capture: true });
addEventListener("resize", schedule);
update();
