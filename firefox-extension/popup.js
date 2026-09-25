/* Toolbar popup: pick the exact audio format once, send the tab, watch the queue.
 * Talks to the local FastAPI service on :8000 (host permission in manifest).
 */
(function () {
  const BASE = "http://localhost:8000";
  const $ = (id) => document.getElementById(id);

  const statusEl = $("status");
  const tabUrlEl = $("tabUrl");
  const formatSelect = $("formatSelect");
  const sendBtn = $("sendBtn");
  const sendMsg = $("sendMsg");
  const queueList = $("queueList");
  const queueCount = $("queueCount");

  let currentUrl = "";
  let defaultSelector = "";

  function isSupportedPage(url) {
    try {
      const u = new URL(url);
      return /(^|\.)youtube\.com$|(^|\.)music\.youtube\.com$/.test(u.hostname);
    } catch {
      return false;
    }
  }

  async function api(path, opts) {
    const res = await fetch(BASE + path, opts);
    if (!res.ok) throw new Error(`HTTP ${res.status}`);
    return res.json();
  }

  async function init() {
    // 1. server alive?
    try {
      const s = await api("/api/cookies/status");
      statusEl.textContent = `Server OK • cookies: ${s.source}`;
      statusEl.className = "";
    } catch {
      statusEl.textContent = "Server not reachable — start it on :8000 first.";
      tabUrlEl.textContent = "—";
      queueList.textContent = "—";
      return;
    }

    // 2. current tab URL
    const [tab] = await browser.tabs.query({ active: true, currentWindow: true });
    currentUrl = (tab && tab.url) || "";
    tabUrlEl.textContent = currentUrl || "—";
    tabUrlEl.title = currentUrl;
    if (!isSupportedPage(currentUrl)) {
      sendMsg.textContent = "Open a YouTube / YT Music video to send it.";
      refreshQueue();
      return;
    }

    // 3. formats, loaded once per video
    formatSelect.innerHTML = "<option>Loading formats…</option>";
    try {
      const data = await api(`/api/formats?url=${encodeURIComponent(currentUrl)}`);
      defaultSelector = data.default || "";
      formatSelect.innerHTML = "";
      const auto = document.createElement("option");
      auto.value = "";
      auto.textContent = `Auto (best Opus — ${defaultSelector})`;
      formatSelect.appendChild(auto);
      for (const f of data.formats) {
        const o = document.createElement("option");
        o.value = f.id;
        const bitrate = f.abr ? `${Math.round(f.abr)}kbps` : "audio";
        o.textContent = `${bitrate} • ${f.acodec} • ${f.ext}${f.note ? ` • ${f.note}` : ""} (itag ${f.id})`;
        formatSelect.appendChild(o);
      }
      formatSelect.disabled = false;
      sendBtn.disabled = false;
    } catch (e) {
      formatSelect.innerHTML = "<option>Format list failed — send with Auto</option>";
      sendBtn.disabled = false; // still allow queueing with the default selector
    }

    refreshQueue();
  }

  async function send() {
    sendBtn.disabled = true;
    sendMsg.textContent = "Sending…";
    try {
      const body = { raw_input: currentUrl };
      if (formatSelect.value) body.format_selector = formatSelect.value;
      const data = await api("/api/add", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(body),
      });
      sendMsg.textContent = data.message || "Added to queue";
      refreshQueue();
    } catch (e) {
      sendMsg.textContent = `Failed: ${e.message}`;
    } finally {
      sendBtn.disabled = false;
    }
  }

  async function refreshQueue() {
    try {
      const data = await api("/api/queue");
      const q = data.queue || [];
      queueCount.textContent = `(${q.length})`;
      queueList.innerHTML = "";
      if (!q.length) {
        queueList.textContent = "Queue is empty";
        return;
      }
      for (const item of q.slice(-6).reverse()) {
        const div = document.createElement("div");
        div.className = "q-item";
        div.textContent = `${item.title} — ${item.status}`;
        div.title = `${item.title}\n${item.status}`;
        queueList.appendChild(div);
      }
    } catch {
      queueList.textContent = "Could not load queue";
    }
  }

  $("sendBtn").addEventListener("click", send);
  $("openUiBtn").addEventListener("click", () => browser.tabs.create({ url: `${BASE}/` }));
  init();
})();
