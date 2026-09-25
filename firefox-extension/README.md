# Firefox extension — setup & how it all works together

Two pieces that talk to each other:

```
YouTube page ──overlay button / toolbar popup──▶ http://localhost:8000/api/* ──▶ queue ──▶ Songs_archive/
   (content.js / popup.js)            (FastAPI in main.py)          (downloader.py + yt-dlp)
```

No cookies ever leave Firefox: auth stays server-side via
yt-dlp `--cookies-from-browser firefox` (see `GET /api/cookies/status`).

## 1. Start the server (pick one)

**Terminal (simplest):**
```
uv sync
uv run uvicorn main:app --host 127.0.0.1 --port 8000
```
Open http://localhost:8000 — the full web UI.

**As a background service (recommended):**
```
mkdir -p ~/.config/systemd/user
cp systemd/music-downloader.service ~/.config/systemd/user/
# edit WorkingDirectory inside the file if your checkout lives elsewhere
systemctl --user daemon-reload
systemctl --user enable --now music-downloader
systemctl --user status music-downloader
curl localhost:8000/api/cookies/status
```
Needs `ffmpeg`, `aria2c` on PATH (`sudo pacman -S ffmpeg aria2`).

## 2. Install the extension (temporary, dev mode)

1. Open `about:debugging#/runtime/this-firefox`.
2. **Load Temporary Add-on…** → select this folder's `manifest.json`.
3. A 🎵 icon appears in the toolbar. It stays until you restart Firefox
   (re-load after restart; permanent signing comes later if you want it).

## 3. Use it

**Overlay (zero clicks of copy-paste):** open any YouTube / YT Music
video → floating **⬇ Send to Downloader** button → toast confirms.
Uses the default best-Opus selector.

**Popup app (toolbar icon):**
- Shows server + cookie status, the current tab URL.
- Loads that video's **audio formats once** into a dropdown
  (`GET /api/formats`), Auto = best Opus by default.
- **Send to queue** posts `{raw_input, format_selector}` to `/api/add`.
- Mini queue view (last 6) + **Open UI** button for the full page.

## Troubleshooting

| Symptom | Fix |
|---|---|
| Popup: "Server not reachable" | Start the server / service on `:8000` first |
| "Open a YouTube video…" | Popup only sends youtube.com / music.youtube.com tabs |
| Format list fails but Send works | Video blocked the probe; it queues with Auto selector |
| Overlay button missing | Only on `/watch` and `/shorts/` pages; reload the tab (SPA nav) |
| Age-restricted / Premium 256k missing | Firefox must be running, same user, logged in; check `/api/cookies/status` |
