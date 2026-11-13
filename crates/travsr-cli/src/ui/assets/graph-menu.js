// travsr ui (#1082 A5): a right-click menu on the Graph page's nodes.
//
// graph.js (the extension's, run unchanged) declares the Cytoscape instance
// as a top-level `cy`, which a later classic script can reach. This file is
// loaded after it and only adds a menu: it never changes what graph.js draws.
// The menu listens for the DOM `contextmenu` event and finds the node under
// the pointer itself, because graph.js takes over the left button's pointer
// events for dragging and panning.
"use strict";

(function () {
  const ui = window.TravsrUi;
  const container = document.getElementById("cy");
  if (!container || typeof cy === "undefined") return;

  // A path's start, picked with "Path from here" and kept for this tab, so it
  // survives the reload a menu pick causes.
  const FROM_KEY = "travsr.ui.pathFrom";
  function pathFrom() { try { return sessionStorage.getItem(FROM_KEY) || ""; } catch (_) { return ""; } }
  function setPathFrom(v) {
    try { if (v) sessionStorage.setItem(FROM_KEY, v); else sessionStorage.removeItem(FROM_KEY); } catch (_) { /* storage off */ }
  }

  // Kinds that are not a symbol: the repo map's packages and regions, the
  // file groups, and the stand-ins for packages outside the repository.
  const NOT_SYMBOL = new Set(["pkg", "group", "ghost", "region", "dir", "file"]);

  function nodeAt(clientX, clientY) {
    const r = container.getBoundingClientRect();
    const x = clientX - r.left;
    const y = clientY - r.top;
    let hit = null;
    cy.nodes().forEach((n) => {
      if (n.isParent() || !n.visible()) return;
      const b = n.renderedBoundingBox({ includeLabels: false });
      if (x >= b.x1 && x <= b.x2 && y >= b.y1 && y <= b.y2) hit = n; // topmost drawn last
    });
    return hit;
  }

  /** The bare name a lookup box takes: the label's first line (a duplicated
   *  name gets its folder as a second line), without the `method:` style kind
   *  prefix the execution-path view keeps (`a::b` is a path, not a prefix). */
  function nameOf(n) {
    return String(n.data("label") || "").split("\n")[0].trim().replace(/^[A-Za-z_]+:(?!:)/, "");
  }

  function itemsFor(n) {
    const kind = n.data("kind") || "";
    const path = n.data("path") || "";
    const line = Number(n.data("line")) || null;
    const out = [];
    const go = (label, href) => out.push({ label, href });
    if (kind === "file" || (NOT_SYMBOL.has(kind) && path)) {
      if (!path) return out;
      go("Dependencies", ui.withRepo("/dependencies", { file: path }));
      go("Blast radius", ui.withRepo("/blast", { file: path }));
      go("File graph", ui.withRepo("/graph", { file: path }));
    } else if (!NOT_SYMBOL.has(kind)) {
      const name = nameOf(n);
      if (!name) return out;
      go("Callers", ui.withRepo("/callers", { symbol: name }));
      go("References", ui.withRepo("/terminal", { cmd: "references", q: name }));
      go("Context", ui.withRepo("/context", { q: name }));
      if (path) go("Blast radius of its file", ui.withRepo("/blast", { file: path }));
      out.push({ sep: true });
      const from = pathFrom();
      out.push({ label: "Path from here", act: () => { setPathFrom(name); toast("Path start: " + name + ". Right-click the end symbol and pick Path to here."); } });
      if (from && from !== name) go("Path to here (from " + from + ")", ui.withRepo("/graph", { from, to: name }));
      else go("Path to here", ui.withRepo("/path", { to: name }));
      if (from) out.push({ label: "Clear path start (" + from + ")", act: () => setPathFrom("") });
    }
    if (path) {
      const link = ui.editorLink(path, line);
      if (link || out.length) out.push({ sep: true });
      if (link) go("Open in editor", link);
    }
    return out;
  }

  let menu = null;
  function close() {
    if (menu) { menu.remove(); menu = null; }
  }
  function toast(text) {
    const t = document.createElement("div");
    t.className = "tsr-gm-toast";
    t.setAttribute("role", "status");
    t.textContent = text;
    document.body.append(t);
    setTimeout(() => t.remove(), 4000);
  }

  function open(n, clientX, clientY) {
    close();
    const items = itemsFor(n);
    if (!items.length) return false;
    menu = document.createElement("div");
    menu.className = "tsr-gm";
    menu.setAttribute("role", "menu");
    const head = document.createElement("div");
    head.className = "tsr-gm-head";
    head.textContent = nameOf(n) || n.data("path") || "";
    menu.append(head);
    const buttons = [];
    for (const it of items) {
      if (it.sep) { menu.append(Object.assign(document.createElement("div"), { className: "tsr-gm-sep" })); continue; }
      const b = document.createElement(it.href ? "a" : "button");
      b.className = "tsr-gm-item";
      b.setAttribute("role", "menuitem");
      b.textContent = it.label;
      if (it.href) b.href = it.href;
      else { b.type = "button"; b.addEventListener("click", () => { close(); it.act(); }); }
      if (it.href) b.addEventListener("click", close);
      menu.append(b);
      buttons.push(b);
    }
    document.body.append(menu);
    // Keep it on screen.
    const w = menu.offsetWidth;
    const h = menu.offsetHeight;
    menu.style.left = Math.max(4, Math.min(clientX, window.innerWidth - w - 4)) + "px";
    menu.style.top = Math.max(4, Math.min(clientY, window.innerHeight - h - 4)) + "px";
    menu.addEventListener("keydown", (e) => {
      const i = buttons.indexOf(document.activeElement);
      if (e.key === "ArrowDown") { e.preventDefault(); buttons[(i + 1) % buttons.length].focus(); }
      else if (e.key === "ArrowUp") { e.preventDefault(); buttons[(i - 1 + buttons.length) % buttons.length].focus(); }
      else if (e.key === "Escape") { e.preventDefault(); close(); }
    });
    buttons[0].focus();
    return true;
  }

  container.addEventListener("contextmenu", (e) => {
    const n = nodeAt(e.clientX, e.clientY);
    if (!n) { close(); return; }
    if (open(n, e.clientX, e.clientY)) e.preventDefault();
  });
  document.addEventListener("mousedown", (e) => { if (menu && !menu.contains(e.target)) close(); }, true);
  document.addEventListener("keydown", (e) => { if (e.key === "Escape") close(); });
  window.addEventListener("blur", close);
  // The user's own zoom closes it. Not Cytoscape's pan/zoom events: layouts
  // and the effects animate the viewport by themselves and would shut the
  // menu the moment it opened.
  container.addEventListener("wheel", close, { passive: true });

  window.TravsrGraphMenu = { open: (id, x, y) => { const n = cy.getElementById(id); return n.nonempty() && open(n, x, y); }, close };
})();
