# Firefox extension (dev scaffold)

One-click overlay so you never copy-paste video links manually.

## What it does

`content.js` is injected into `youtube.com` / `music.youtube.com` watch
pages (see `manifest.json` → `content_scripts`, MDN: *user_interface*).
It adds a floating **⬇ Send to Downloader** button that POSTs the
current page URL to `http://localhost:8000/api/add` (your FastAPI
service). CORS on the service already allows this.

No `cookies` / `webRequest` API is used — the extension never touches
your login cookies. All Premium/auth handling stays server-side via
yt-dlp's `--cookies-from-browser firefox` (see `/api/cookies/status`).

## Install temporarily (Firefox)

1. Start the service: `uv run uvicorn main:app --port 8000`
   (or install the systemd user service in `../systemd/`).
2. Open `about:debugging#/runtime/this-firefox` → **Load Temporary Add-on…**
3. Select this folder's `manifest.json`.
4. Open any YouTube / YT Music video → click **⬇ Send to Downloader**.

## Next steps (not yet done)

- `cookies` API + `browser.cookies.getAll({domain})` export button, if you
  ever want a one-click `cookies.txt` refresh instead of
  `--cookies-from-browser`.
- Page-action popup listing `/api/formats?url=<current>` so you can pick
  the exact audio format once before queueing.
- `web-ext run` / `web-ext lint` wiring for automated testing with a
  real browser (say the word and I'll add it — happy to test against a
  browser you set up).
