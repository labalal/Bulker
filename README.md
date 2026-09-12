# Music Downloader

FastAPI + yt-dlp bulk audio downloader with an AtomicParsley tagging
pipeline (thumbnail compress -> embed -> move to `Songs_archive`).

## Setup

1. Install [mise](https://mise.jdx.dev) (manages the Python version and uv for you):
   ```
   curl https://mise.run | sh
   ```
2. From the project folder:
   ```
   mise install       # installs the pinned Python version + uv
   uv sync            # creates .venv and installs dependencies from pyproject.toml
   ```
3. Install these system tools separately (not managed by uv/mise):
   - `ffmpeg` — audio extraction + thumbnail resizing
   - `aria2c` — the external downloader yt-dlp shells out to
   - `AtomicParsley` — embeds title/artist/artwork into the .m4a file
   - On Arch: `sudo pacman -S ffmpeg aria2 atomicparsley`
   - On Debian/Ubuntu: `sudo apt install ffmpeg aria2 atomicparsley`
4. Copy `.env.example` to `.env` and adjust if needed (defaults work out of the box).
5. Export your YouTube cookies to `cookies.txt` (needed for anything
   age-restricted or member-only) — **never commit this file**, it's
   personal to your account and already in `.gitignore`.

## Run

```
mise run dev     # http://localhost:8000, auto-reloads on code changes
```
or without mise:
```
uv run uvicorn main:app --reload
```

## Cleanup script

```
mise run cleanup
```
Removes temp-workspace files that have already made it into `Songs_archive`.