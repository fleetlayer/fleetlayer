// fleetlayer-pages: live updates (server-sent events) and the image lightbox. The page works without it.
"use strict";

(() => {
  const content = () => document.getElementById("content");

  // ---------------------------------------------------------------- galleries of phone screenshots

  function markTall(root) {
    for (const g of root.querySelectorAll(".gallery")) {
      const imgs = [...g.querySelectorAll("img")];
      const check = () => {
        const sized = imgs.filter((i) => i.naturalWidth > 0);
        if (sized.length < imgs.length) return;
        const tall = sized.filter((i) => i.naturalHeight > i.naturalWidth * 1.4).length;
        g.classList.toggle("tall", tall * 2 > sized.length);
      };
      imgs.forEach((i) => (i.complete ? null : i.addEventListener("load", check, { once: true })));
      check();
    }
  }

  // ---------------------------------------------------------------- lightbox

  const box = document.getElementById("lightbox");
  let items = [];
  let at = 0;

  function el(tag, cls, text) {
    const e = document.createElement(tag);
    if (cls) e.className = cls;
    if (text) e.textContent = text;
    return e;
  }

  function buildBox() {
    box.replaceChildren();
    const bar = el("div", "lb-bar");
    const count = el("span", "lb-count");
    const close = el("button", "lb-btn lb-close", "✕");
    close.type = "button";
    close.setAttribute("aria-label", "Close");
    close.addEventListener("click", () => box.close());
    bar.append(count, close);
    const stage = el("div", "lb-stage");
    const img = el("img");
    img.alt = "";
    const prev = el("button", "lb-btn lb-prev", "‹");
    prev.type = "button";
    prev.setAttribute("aria-label", "Previous image");
    prev.addEventListener("click", () => show(at - 1));
    const next = el("button", "lb-btn lb-next", "›");
    next.type = "button";
    next.setAttribute("aria-label", "Next image");
    next.addEventListener("click", () => show(at + 1));
    stage.append(img, prev, next);
    stage.addEventListener("click", (e) => { if (e.target === stage) box.close(); });
    let x0 = null;
    stage.addEventListener("touchstart", (e) => { x0 = e.touches[0].clientX; }, { passive: true });
    stage.addEventListener("touchend", (e) => {
      if (x0 === null) return;
      const dx = e.changedTouches[0].clientX - x0;
      if (Math.abs(dx) > 48) show(at + (dx < 0 ? 1 : -1));
      x0 = null;
    });
    const caption = el("p", "lb-caption");
    box.append(bar, stage, caption);
    return { count, img, prev, next, caption };
  }

  let parts = null;

  function show(i) {
    if (i < 0 || i >= items.length) return;
    at = i;
    const a = items[i];
    const img = a.querySelector("img");
    parts.img.src = a.getAttribute("href");
    parts.img.alt = img ? img.alt : "";
    parts.caption.textContent = img ? img.alt : "";
    parts.caption.hidden = !parts.caption.textContent;
    parts.count.textContent = items.length > 1 ? `${i + 1} of ${items.length}` : "";
    parts.prev.disabled = i === 0;
    parts.next.disabled = i === items.length - 1;
    parts.prev.hidden = parts.next.hidden = items.length < 2;
  }

  if (box && typeof box.showModal === "function") {
    parts = buildBox();
    document.addEventListener("click", (e) => {
      const a = e.target.closest("a.lb");
      if (!a || e.metaKey || e.ctrlKey || e.shiftKey || e.button !== 0) return;
      e.preventDefault();
      items = [...document.querySelectorAll("#content a.lb")];
      if (!box.open) box.showModal();
      show(items.indexOf(a));
    });
    box.addEventListener("keydown", (e) => {
      if (e.key === "ArrowLeft") show(at - 1);
      else if (e.key === "ArrowRight") show(at + 1);
    });
  }

  // ---------------------------------------------------------------- live

  const live = document.body.dataset.live;
  const badge = document.getElementById("live");
  const setState = (s, title) => {
    if (!badge) return;
    badge.dataset.state = s;
    if (title) badge.title = title;
  };

  async function refresh() {
    const res = await fetch(`${location.pathname}?fragment=1`, { credentials: "same-origin", cache: "no-store" });
    if (res.status === 404) {
      setState("off", "This page was removed");
      return;
    }
    if (!res.ok) return;
    const html = await res.text();
    const c = content();
    if (!c) return;
    const frame = c.querySelector("iframe.mock-frame");
    if (frame) {
      // A mock-up: keep the frame (and its scroll), refresh the header, reload the frame.
      const tmp = document.createElement("div");
      tmp.innerHTML = html;
      const head = tmp.querySelector(".doc-head");
      const problems = tmp.querySelector("#problems");
      c.querySelector(".doc-head")?.replaceWith(head);
      c.querySelector("#problems")?.remove();
      if (problems) head.after(problems);
      frame.contentWindow?.location.reload();
    } else {
      const y = window.scrollY;
      c.innerHTML = html;
      window.scrollTo(0, y);
      markTall(c);
    }
    const t = c.querySelector("h1");
    if (t && !document.title.startsWith(t.textContent)) {
      document.title = `${t.textContent} · Pages`;
      const here = document.querySelector(".crumb-here");
      if (here && location.pathname.split("/").filter(Boolean).length === 2) here.textContent = t.textContent;
    }
    setState("updated", "Updated just now");
    setTimeout(() => setState("on", "Updates as the files change"), 1500);
  }

  if (live && typeof EventSource === "function") {
    let es;
    let backoff = 1000;
    const connect = () => {
      es = new EventSource(`/_/events?path=${encodeURIComponent(live)}`);
      es.addEventListener("ready", () => { backoff = 1000; setState("on", "Updates as the files change"); });
      es.addEventListener("change", () => { refresh().catch(() => {}); });
      es.onerror = () => {
        setState("off", "Reconnecting…");
        es.close();
        setTimeout(connect, backoff);
        backoff = Math.min(backoff * 2, 30000);
      };
    };
    connect();
  }

  markTall(document);
})();
