// Pico website: the standard library reference. reference.json is `pico doc --std --json`
// (checked against the stdlib by crates/pico-cli/tests/doc_examples.rs); this renders it: the
// module tree, a module's page with its items, their members and examples, and search.
"use strict";

const tree = document.getElementById("ref-tree");
const content = document.getElementById("ref-content");
const side = document.getElementById("ref-side");
const browse = document.getElementById("ref-browse");
const q = document.getElementById("ref-q");

const KIND_LETTER = {
  function: "f", method: "m", constructor: "c", class: "C", struct: "S", interface: "I",
  enum: "E", type: "T", constant: "K", field: "F", property: "P", module: "M",
};
const KIND_ORDER = ["class", "struct", "interface", "enum", "type", "function", "constant"];

let ref = null;
/** Every item and member: { module, item, path: ["Map", "set"], hash, text } */
let entries = [];

const esc = (s) => String(s).replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;");
const badge = (kind) => `<span class="k k-${kind}" title="${kind}">${KIND_LETTER[kind] ?? "?"}</span>`;
const hashOf = (module, path) => `#${module.id}${path.length ? "/" + path.map(encodeURIComponent).join(".") : ""}`;
const anchorOf = (module, path) => `i-${module.id}-${path.join("-")}`.replace(/[^\w-]/g, "_");
/** Doc text for readers: the maintainers' references (`(spec §13.4)`, `decision 250`) left out. */
const forReaders = (html) => html
  .replace(/\s*\((?:spec|PLAN|decisions?)\b[^()]*\)/g, "")
  .replace(/[;,]\s*decisions?\s+\d+(?:\s*(?:,|and)\s*\d+)*/g, "")
  .replace(/\s*\(decisions?\s+\d+[^()]*\)/g, "");
const plain = (html) => html.replace(/<[^>]*>/g, " ").replace(/\s+/g, " ").trim();

/** A signature, highlighted, with the documented types linked. */
function signature(sig) {
  return highlight(sig).replace(/<span class="t-type">([A-Za-z_]\w*)<\/span>/g, (all, name) => {
    const target = ref.types[name];
    if (!target) return all;
    const [module, item] = target.split("#");
    return `<a href="#${module}${item ? "/" + item : ""}">${esc(name)}</a>`;
  });
}

function sortedItems(module) {
  // Core and std pages keep their order (a module's own); the tree groups by kind.
  return [...module.items].sort((a, b) =>
    (KIND_ORDER.indexOf(a.kind) - KIND_ORDER.indexOf(b.kind)) || a.name.localeCompare(b.name));
}

// --- Tree ------------------------------------------------------------------------------------------

function buildTree() {
  const html = ref.modules.map((m) => {
    const items = sortedItems(m).map((i) => {
      const link = `<a href="${hashOf(m, [i.name])}" data-hash="${hashOf(m, [i.name])}">${badge(i.kind)}<span class="name">${esc(i.name)}</span></a>`;
      if (!i.members.length) return `<li>${link}</li>`;
      const members = i.members.map((mm) =>
        `<li><a href="${hashOf(m, [i.name, mm.name])}" data-hash="${hashOf(m, [i.name, mm.name])}">${badge(mm.kind)}<span class="name">${esc(mm.name)}</span></a></li>`).join("");
      return `<li><details><summary>${link}</summary><ul>${members}</ul></details></li>`;
    }).join("");
    return `<details data-module="${m.id}"><summary data-hash="#${m.id}"><span class="name">${esc(m.name)}</span><span class="count">${m.items.length}</span></summary><ul>${items}</ul></details>`;
  }).join("");
  tree.innerHTML = html;
  // A module's name opens its page as well as its branch.
  tree.addEventListener("click", (e) => {
    const summary = e.target.closest("summary[data-hash]");
    if (summary && !e.target.closest("a")) {
      const open = summary.parentElement.open;
      if (location.hash !== summary.dataset.hash) {
        location.hash = summary.dataset.hash;
        if (open) e.preventDefault();
      }
    }
    if (e.target.closest("a")) closeSide();
  });
}

function markTree(hash) {
  for (const el of tree.querySelectorAll(".current")) el.classList.remove("current");
  const [moduleId] = hash.slice(1).split("/");
  const branch = tree.querySelector(`details[data-module="${CSS.escape(moduleId)}"]`);
  if (branch) branch.open = true;
  const link = tree.querySelector(`[data-hash="${CSS.escape(hash)}"]`);
  if (!link) {
    branch?.querySelector("summary")?.classList.add("current");
    return;
  }
  link.classList.add("current");
  const parent = link.closest("li")?.parentElement?.closest("details");
  if (parent) parent.open = true;
  link.scrollIntoView({ block: "nearest" });
}

// --- Pages -----------------------------------------------------------------------------------------

/** A link to where `item` is declared in the standard library's sources, on GitHub. */
function sourceLink(item) {
  if (!item.source) return "";
  const [file, line] = item.source.split(":");
  return `<a class="ref-src" href="https://github.com/padcom/pico-lang/blob/main/stdlib/${file}#L${line}" title="${esc(item.source)}">source</a>`;
}

function itemHtml(module, item, path, level) {
  const h = level === 0 ? "h2" : "h3";
  const hash = hashOf(module, path);
  const doc = item.doc.trim()
    ? `<div class="ref-doc">${forReaders(item.doc)}</div>`
    : `<p class="ref-nodoc">No description yet.</p>`;
  const examples = item.examples.map((e) => `
    <div class="ref-example window">
      <div class="window-bar"><span></span><span></span><span></span><b>${esc(e.caption || "Example")}</b>
        <button class="ref-copy" type="button">Copy</button></div>
      <pre class="code"><code>${highlight(e.code)}</code></pre>
    </div>`).join("");
  const members = item.members.length
    ? `<p class="ref-members-title">Members</p><div class="ref-members">${item.members.map((m) => itemHtml(module, m, [...path, m.name], level + 1)).join("")}</div>`
    : "";
  return `
    <section class="ref-item" id="${anchorOf(module, path)}">
      <${h}>${badge(item.kind)}<span>${esc(item.name)}</span><span class="kind-label">${item.kind}</span><a class="anchor" href="${hash}" aria-label="Link to ${esc(item.name)}">#</a>${sourceLink(item)}</${h}>
      <pre class="ref-sig">${signature(item.signature)}</pre>
      ${doc}
      ${examples}
      ${members}
    </section>`;
}

function moduleHtml(module) {
  const isStd = module.name.startsWith("std/");
  const sample = isStd ? sortedItems(module).find((i) => i.kind !== "type") : null;
  const importLine = isStd
    ? `<code class="ref-import">${highlight(`import { ${sample ? sample.name : "…"} } from "${module.name}"`)}</code>`
    : "";
  const toc = sortedItems(module).map((i) =>
    `<a href="${hashOf(module, [i.name])}">${badge(i.kind)}<span class="name">${esc(i.name)}</span></a>`).join("");
  return `
    <p class="ref-crumbs"><a href="#">Standard library</a> / ${esc(module.name)}</p>
    <h1 class="ref-title">${esc(module.name)}</h1>
    <div class="ref-about">${forReaders(module.about)}</div>
    ${importLine}
    <nav class="ref-toc" aria-label="Contents">${toc}</nav>
    ${sortedItems(module).map((i) => itemHtml(module, i, [i.name], 0)).join("")}`;
}

function homeHtml() {
  const std = ref.modules.filter((m) => m.name.startsWith("std/"));
  const builtin = ref.modules.filter((m) => !m.name.startsWith("std/"));
  const card = (m) => `<a href="#${m.id}">${badge("module")}<span class="name">${esc(m.name)}</span></a>`;
  return `
    <h1 class="ref-title">Standard library</h1>
    <div class="ref-about"><p>Every module, type and function of the Pico ${esc(ref.version)} standard library, with usage examples.
      The core library and the built-in types are there without an import; <code>std/…</code> modules are imported by name.
      Search with <kbd>/</kbd>.</p></div>
    <nav class="ref-toc" aria-label="Built in">${builtin.map(card).join("")}</nav>
    <nav class="ref-toc" aria-label="Modules">${std.map(card).join("")}</nav>`;
}

let shownModule = null;

function route() {
  if (!ref) return;
  const hash = decodeURIComponent(location.hash);
  const [moduleId, rest] = hash.slice(1).split(/\/(.*)/s);
  const module = ref.modules.find((m) => m.id === moduleId);
  if (q.value) clearSearch(false);
  if (!module) {
    shownModule = null;
    content.innerHTML = homeHtml();
    document.title = "Pico Standard Library";
    markTree("#");
    scrollTo({ top: 0, behavior: "instant" });
    return;
  }
  if (shownModule !== module) {
    content.innerHTML = moduleHtml(module);
    shownModule = module;
    document.title = `${module.name} · Pico Standard Library`;
  }
  markTree(location.hash);
  if (rest) {
    const el = document.getElementById(anchorOf(module, rest.split(".")));
    if (el) {
      el.scrollIntoView({ block: "start", behavior: "instant" });
      // (Again once the web fonts are in: they change the heights above it.)
      document.fonts?.ready.then(() => el.scrollIntoView({ block: "start", behavior: "instant" }));
      el.classList.remove("flash");
      void el.offsetWidth;
      el.classList.add("flash");
      setTimeout(() => el.classList.remove("flash"), 1600);
    }
  } else {
    scrollTo({ top: 0, behavior: "instant" });
  }
}

// --- Search ----------------------------------------------------------------------------------------

function indexEntries() {
  entries = [];
  const add = (module, item, path) => {
    entries.push({
      module, item, path,
      name: path.join("."),
      lname: item.name.toLowerCase(),
      lpath: path.join(".").toLowerCase(),
      text: `${item.signature} ${plain(item.doc)}`.toLowerCase(),
    });
    for (const m of item.members) add(module, m, [...path, m.name]);
  };
  for (const m of ref.modules) for (const i of m.items) add(m, i, [i.name]);
}

function score(e, words) {
  let s = 0;
  for (const w of words) {
    if (e.lname === w) s += 100;
    else if (e.lname.startsWith(w)) s += 60;
    else if (e.lpath.includes(w)) s += 35;
    else if (e.module.name.toLowerCase().includes(w)) s += 15;
    else if (e.text.includes(w)) s += 5;
    else return 0;
  }
  return s - e.path.length;
}

function mark(name, words) {
  let html = esc(name);
  for (const w of words) {
    const i = name.toLowerCase().indexOf(w);
    if (i >= 0) {
      html = `${esc(name.slice(0, i))}<mark>${esc(name.slice(i, i + w.length))}</mark>${esc(name.slice(i + w.length))}`;
      break;
    }
  }
  return html;
}

let selected = 0;

function search() {
  const words = q.value.toLowerCase().split(/\s+/).filter(Boolean);
  if (!words.length) {
    clearSearch(true);
    return;
  }
  const hits = entries
    .map((e) => [score(e, words), e])
    .filter(([s]) => s > 0)
    .sort((a, b) => b[0] - a[0] || a[1].name.localeCompare(b[1].name))
    .slice(0, 80);
  shownModule = null;
  selected = 0;
  scrollTo({ top: 0, behavior: "instant" });
  content.innerHTML = `
    <p class="ref-crumbs"><a href="#">Standard library</a> / search</p>
    <h1 class="ref-title">${hits.length ? `Results for “${esc(q.value.trim())}”` : `Nothing matches “${esc(q.value.trim())}”`}</h1>
    <ul class="ref-results">${hits.map(([, e], n) => `
      <li><a href="${hashOf(e.module, e.path)}"${n === 0 ? ' class="sel"' : ""}>${badge(e.item.kind)}
        <span class="r-name">${mark(e.name, words)}<span class="r-where">${esc(e.module.name)}</span></span>
        <span class="r-sig">${esc(e.item.signature)}</span></a></li>`).join("")}</ul>`;
}

function clearSearch(rerender) {
  q.value = "";
  if (rerender) {
    shownModule = null;
    route();
  }
}

function moveSelection(by) {
  const links = [...content.querySelectorAll(".ref-results a")];
  if (!links.length) return;
  links[selected]?.classList.remove("sel");
  selected = (selected + by + links.length) % links.length;
  links[selected].classList.add("sel");
  links[selected].scrollIntoView({ block: "nearest" });
}

let timer = 0;
q.addEventListener("input", () => {
  clearTimeout(timer);
  timer = setTimeout(search, 80);
});
q.addEventListener("keydown", (e) => {
  if (e.key === "ArrowDown") { e.preventDefault(); moveSelection(1); }
  else if (e.key === "ArrowUp") { e.preventDefault(); moveSelection(-1); }
  else if (e.key === "Enter") {
    const link = content.querySelectorAll(".ref-results a")[selected];
    if (link) { location.hash = link.getAttribute("href"); q.blur(); closeSide(); }
  } else if (e.key === "Escape") { clearSearch(true); q.blur(); }
});
addEventListener("keydown", (e) => {
  if (e.key === "/" && document.activeElement !== q && !e.metaKey && !e.ctrlKey) {
    e.preventDefault();
    openSide();
    q.focus();
    q.select();
  }
});

// --- Copy buttons, the small-screen drawer ---------------------------------------------------------------

content.addEventListener("click", (e) => {
  const button = e.target.closest(".ref-copy");
  if (!button) return;
  const code = button.closest(".ref-example").querySelector("code").textContent;
  navigator.clipboard?.writeText(code).then(() => {
    button.textContent = "Copied";
    setTimeout(() => { button.textContent = "Copy"; }, 1200);
  });
});

function openSide() {
  // (On small screens the tree is above the content.)
  if (getComputedStyle(browse).display !== "none") scrollTo({ top: 0, behavior: "instant" });
  side.classList.add("open");
  browse.setAttribute("aria-expanded", "true");
}
function closeSide() {
  side.classList.remove("open");
  browse.setAttribute("aria-expanded", "false");
}
browse.addEventListener("click", () => (side.classList.contains("open") ? closeSide() : openSide()));

// --- Start ---------------------------------------------------------------------------------------------------

fetch("reference.json")
  .then((r) => r.json())
  .then((data) => {
    ref = data;
    document.getElementById("ref-version").textContent = `Pico ${ref.version}`;
    buildTree();
    indexEntries();
    route();
    addEventListener("hashchange", route);
  })
  .catch(() => {
    content.innerHTML = `<p class="ref-empty">The reference couldn't be loaded.</p>`;
  });
