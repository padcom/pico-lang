// Pico website: the code samples (highlighted by highlight.js) and the scroll-driven feature tour.
"use strict";

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
