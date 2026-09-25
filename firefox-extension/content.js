/* Content script: injects a floating "Send to Downloader" overlay button
 * on YouTube / YouTube Music watch pages. One click POSTs the current
 * page URL to the local FastAPI service -- no manual copy-paste.
 *
 * MDN refs: user_interface (content scripts / page injection) and
 * Browser_support_for_JavaScript_APIs (fetch, notifications etc.).
 * content_scripts.match patterns for youtube.com + music.youtube.com are
 * declared in manifest.json; this file only does DOM injection + fetch.
 */
(function () {
  const API = "http://localhost:8000/api/add";
  const BTN_ID = "md-send-to-queue";

  function toast(msg) {
    let el = document.getElementById("md-toast");
    if (!el) {
      el = document.createElement("div");
      el.id = "md-toast";
      document.documentElement.appendChild(el);
    }
    el.textContent = msg;
    clearTimeout(el._t);
    el._t = setTimeout(() => el.remove(), 3500);
  }

  async function sendCurrentPage() {
    const btn = document.getElementById(BTN_ID);
    if (btn) {
      btn.disabled = true;
      btn.textContent = "Sending…";
    }
    try {
      const res = await fetch(API, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ raw_input: location.href }),
      });
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      const data = await res.json();
      toast(data.message || "Added to queue");
    } catch (e) {
      toast(`Downloader not reachable (${e.message}). Is it running on :8000?`);
    } finally {
      if (btn) {
        btn.disabled = false;
        btn.textContent = "⬇ Send to Downloader";
      }
    }
  }

  function isWatchPage() {
    // youtube.com/watch, /shorts/, youtu.be redirect, music.youtube.com/watch
    return /\/watch|\/shorts\//.test(location.pathname);
  }

  function ensureButton() {
    if (document.getElementById(BTN_ID)) return;
    if (!isWatchPage()) return;
    const btn = document.createElement("button");
    btn.id = BTN_ID;
    btn.textContent = "⬇ Send to Downloader";
    btn.title = "Send this video to your local music downloader queue";
    btn.addEventListener("click", sendCurrentPage);
    document.documentElement.appendChild(btn);
  }

  // YouTube is an SPA -- re-check on navigation.
  new MutationObserver(ensureButton).observe(document.documentElement, {
    childList: true,
    subtree: true,
  });
  ensureButton();
})();
