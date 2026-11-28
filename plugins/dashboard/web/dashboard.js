// The live view: fetch the page's <main> again every few seconds and swap it in when it changed. Paused while the tab
// is hidden or while someone is pressing a button in it; a lost sign-in reloads the page (to sign in again).
(() => {
  const main = document.getElementById("main");
  const live = document.querySelector(".live");
  const label = live && live.querySelector(".live-label");
  if (!main || !main.dataset.refresh) return;
  const EVERY = 5000;
  let last = main.innerHTML;
  let busy = false;
  let timer = null;

  // A message after an action is shown once: drop ?msg= from the address so a reload doesn't show it again.
  if (location.search.includes("msg=")) {
    const u = new URL(location.href);
    u.searchParams.delete("msg");
    history.replaceState(null, "", u.pathname + u.search);
  }

  const state = (s, text) => {
    if (!live) return;
    live.dataset.state = s;
    if (label) label.textContent = text;
  };

  async function tick() {
    if (document.hidden || busy) return;
    if (main.contains(document.activeElement) && document.activeElement.matches("button, input, a.btn")) return;
    busy = true;
    try {
      const res = await fetch(main.dataset.refresh, { credentials: "same-origin", headers: { Accept: "text/html" } });
      if (res.redirected || res.status === 401 || res.status === 403) {
        location.reload();
        return;
      }
      if (!res.ok) throw new Error(String(res.status));
      const html = await res.text();
      // The callout from an action belongs to the first view only.
      if (html !== last) {
        const y = window.scrollY;
        main.innerHTML = html;
        last = html;
        window.scrollTo(0, y);
        state("updated", "Live");
        setTimeout(() => state("on", "Live"), 900);
      } else {
        state("on", "Live");
      }
    } catch (e) {
      state("off", "Reconnecting");
    } finally {
      busy = false;
    }
  }

  const start = () => { if (!timer) timer = setInterval(tick, EVERY); };
  const stop = () => { clearInterval(timer); timer = null; };
  document.addEventListener("visibilitychange", () => (document.hidden ? stop() : (tick(), start())));
  start();
})();
