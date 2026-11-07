# Bulker

YouTube **Music** bulk audio downloader using the Rust implementation and a
shared web UI. It downloads native Opus through the
`yt-dlp` + `aria2c` + `ffmpeg` pipeline with retry-hardened settings.
Quality is decided by the YouTube Music surface (playlists, singles, and
YouTube Music search); see `rust/src/ytdlp.rs`.

## Structure

```
web/      the web UI embedded by the Rust server
rust/     the Rust server, daemon, SQLite database, and CLI
```

## Setup

```bash
mise install          # Rust + rust-analyzer
sudo pacman -S ffmpeg aria2                # or apt equivalent
```

The application runtime is Rust/Axum. Bun 1.3.14 is installed locally through mise
and is used by yt-dlp for YouTube JavaScript challenge solving. Node.js is
not required by this project.

Run the Rust server:

```bash
mise run server                         # http://127.0.0.1:8000
```

Build a release binary:

```bash
mise run build
```

Rust development commands:

```bash
mise run check       # type-check
mise run lint        # clippy
mise run test        # tests
mise run quality     # format, check, lint, and test
```

Relative runtime paths default to `$HOME/Bulker`, regardless of the shell's
current directory. Set `BULKER_BASE` when a different base directory is
needed.

Individual path variables such as `QUEUE_DB_PATH` and `LOG_FILE` may also be
absolute; relative values are resolved under the base directory.

## Verify Premium actually delivers 256kbps Opus

Premium cookies unlock the ~256kbps Opus itag (774) on YouTube Music tracks;
free tietops out around 128-160kbps (itag 251). To *prove* it:

```bash
mise run inspect -- <music.youtube.com URL>           # no server needed
curl '127.0.0.1:8000/api/quality?url=<url>'          # while the server runs
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
./rust/target/release/bulker-rs songs   # reads queue.db SQLite
curl '127.0.0.1:8000/api/songs'            # JSON from a running server
```

## CLI

```bash
mise run inspect -- <music.youtube.com URL>
./rust/target/release/bulker-rs songs
```

## Cookies

Export your Premium cookies to `cookies.txt` (Netscape format). Never
commit it. When yt-dlp reports them expired, the web UI shows a red banner.
Copy `.env.example` to `.env` to override paths/limits; defaults work
out of the box.
