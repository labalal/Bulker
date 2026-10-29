# Bulker

YouTube **Music** bulk audio downloader. Two interchangeable implementations
that share the same web UI and philosophy (best native Opus, lossless
stream-copy, no re-encoding):

- **Python**: FastAPI + yt-dlp Python API + mutagen + SQLite-free JSON store
- **Rust** (`rust/`): single-binary axum + SQLite + lofty (smaller, faster,
  no runtime)

Both download through the `yt-dlp` + `aria2c` + `ffmpeg` pipeline with the
same retry-hardened settings, and both are only YouTube-Music-facing:
quality is decided by the surface (playlists, singles, YouTube-Music search),
see `core/downloader.py` / `rust/src/ytdlp.rs`.

## Structure

```
core/     shared engine (Python): config, downloader pipeline, cleaning
server/   the FastAPI app -- run me (uvicorn server.main:app)
web/      the web UI (shared by both implementations; rust embeds it)
cli/      command-line client tools + `serve` daemon mode (Python)
rust/     the Rust implementation (server + daemon + `check` CLI)
```

## Setup (Python)

```bash
mise install          # python + uv
uv sync               # .venv
sudo pacman -S ffmpeg aria2 atomicparsley   # or apt equivalent
```

Run:

```bash
uv run python -m cli serve            # daemon mode: http://127.0.0.1:8000
# or: uv run uvicorn server.main:app --reload
```

## Setup (Rust)

```bash
cd rust && cargo build --release       # -> rust/target/release/bulker-rs
```

Run (from the repo root so config paths + cookies.txt line up):
```bash
cd .. && ./rust/target/release/bulker-rs          # same UI at 127.0.0.1:8000
```

## Verify Premium actually delivers 256kbps Opus

Premium cookies unlock the ~256kbps Opus itag (774) on YouTube Music tracks;
free tietops out around 128-160kbps (itag 251). To *prove* it:

```bash
uv run python -m cli check <music.youtube.com URL>    # Python
./rust/target/release/bulker-rs check <url>           # Rust (no server needed)
curl '127.0.0.1:8000/api/quality?url=<url>'           # while the server runs
```

`premium_opus_available: true` means downloads will use the Premium stream.
Every completed download also logs a **quality receipt** with its delivered
kbps in app.log.

## Downloaded-music library (your 10 GB budget)

Every finished download is catalogued — title, artist, YouTube itag,
delivered kbps, file path, download time, and total GiB spent. Re-downloading
a track at a different quality appends a new row, so you can see quality
changes over time:

```bash
uv run python -m cli songs        # Python (reads library.json, works offline)
./rust/target/release/bulker-rs songs   # Rust (reads queue.db SQLite)
curl '127.0.0.1:8000/api/songs'            # JSON from a running server
```

Python keeps `library.json`; the Rust build stores identical data in SQLite
(`queue.db`, table `songs`).

## CLI

```bash
uv run python -m cli --help
uv run python -m cli add <url>...   status   retry <id>   delete <id>
uv run python -m cli clear          cleanup  songs   logs
```

## Cookies

Export your Premium cookies to `cookies.txt` (Netscape format). Never
commit it. When yt-dlp reports them expired, the web UI shows a red banner.
Copy `.env.example` to `.env` to override paths/limits; defaults work
out of the box.
