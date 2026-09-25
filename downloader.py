import base64
import logging
import os
import re
import shutil
import subprocess
import threading
from urllib.parse import parse_qs, quote, urlencode, urlparse

import yt_dlp
from mutagen.flac import Picture
from mutagen.oggopus import OggOpus

import config

logger = logging.getLogger("audio_downloader")

# The pipeline every download goes through, in order. Exposed here so
# main.py can build the initial per-item step map without duplicating
# the list.
STEP_NAMES = ["Download", "Extract Audio", "Thumbnail", "Metadata", "Archive"]


class DownloadCancelled(Exception):
    pass


class YTDLPLogger:
    """Routes yt-dlp's own internal messages into app.log instead of
    dropping them, so the logs panel actually shows what yt-dlp saw."""

    def debug(self, msg):
        if msg.startswith("[debug] "):
            return
        logger.debug(msg)

    def warning(self, msg):
        logger.warning(msg)

    def error(self, msg):
        logger.error(msg)


def _cookie_opts() -> dict:
    """Firefox-first cookie selection.

    Prefers live Firefox profile cookies (COOKIES_FROM_BROWSER, default
    "firefox") so no manual export step is needed. Falls back to the
    cookies.txt file when it exists. Returns {} when neither applies.
    """
    if config.COOKIES_FROM_BROWSER:
        return {"cookiesfrombrowser": (config.COOKIES_FROM_BROWSER,)}
    if config.COOKIES_FILE and os.path.exists(config.COOKIES_FILE):
        return {"cookiefile": config.COOKIES_FILE}
    return {}


def cookie_source_label() -> str:
    """Human-readable label for /api/cookies/status."""
    if config.COOKIES_FROM_BROWSER:
        return f"browser:{config.COOKIES_FROM_BROWSER} (+{config.COOKIES_FILE} fallback)"
    if config.COOKIES_FILE and os.path.exists(config.COOKIES_FILE):
        return f"file:{config.COOKIES_FILE}"
    return "none (anonymous -- age-restricted / Premium streams will fail)"


def cleanup_existing_mess():
    """Rescues old audio files to Songs_archive and deletes the rest."""
    archive_dir = config.SONGS_ARCHIVE_DIR
    os.makedirs(archive_dir, exist_ok=True)
    os.makedirs(config.TEMP_WORKSPACE_DIR, exist_ok=True)

    if not os.path.exists(config.DOWNLOAD_DIR):
        return

    audio_exts = (".opus", ".m4a")  # .m4a kept so old libraries still get rescued
    for root, _, files in os.walk(config.DOWNLOAD_DIR):
        for file in files:
            full_path = os.path.join(root, file)
            if file.endswith(audio_exts):
                try:
                    shutil.move(full_path, os.path.join(archive_dir, file))
                except OSError:
                    logger.exception("Could not rescue %s to archive", full_path)
            elif file.endswith((".jpg", ".webp", ".part", ".webm", ".temp")):
                try:
                    os.remove(full_path)
                except OSError:
                    logger.exception("Could not remove leftover %s", full_path)


class OpusTagPP(yt_dlp.postprocessor.PostProcessor):
    """Compresses the thumbnail, embeds it + title/artist into the Opus
    file's Vorbis comments with mutagen (AtomicParsley only understands
    MP4/M4A, not Ogg/Opus), then moves the finished file into
    Songs_archive. Reports each step through on_progress so the UI can
    show a green/red status per step instead of a black box.
    """

    def __init__(self, on_progress=None, cancel_event: threading.Event | None = None):
        super().__init__()
        self.on_progress = on_progress
        self.cancel_event = cancel_event

    def _report(self, step, state, detail):
        if self.on_progress:
            self.on_progress(step, state, detail)

    def run(self, info):
        filepath = info.get("filepath")
        title = info.get("title", "Unknown Title")
        artist = info.get("artist") or info.get("uploader") or "Unknown Artist"

        self._report("Extract Audio", "done", "Audio extracted (Opus)")

        if self.cancel_event is not None and self.cancel_event.is_set():
            # Cancelled right after extraction -- clean up the orphaned
            # raw file instead of tagging/archiving it.
            if filepath and os.path.exists(filepath):
                try:
                    os.remove(filepath)
                except OSError:
                    logger.warning("Could not remove cancelled file %s", filepath)
            return [], info

        if not filepath or not os.path.exists(filepath):
            self._report("Thumbnail", "failed", "Source audio file missing")
            return [], info

        # FFmpegExtractAudioPP already rewrote filepath/ext to .opus (or
        # left it alone if it was already a native Opus stream copy).
        if not filepath.endswith(".opus"):
            filepath = os.path.splitext(filepath)[0] + ".opus"

        base_path = os.path.splitext(filepath)[0]
        thumb_path = base_path + ".jpg"

        # --- thumbnail ---------------------------------------------------
        self._report("Thumbnail", "active", "Compressing thumbnail (320px)...")
        compressed_thumb = None
        if os.path.exists(thumb_path):
            compressed_thumb = base_path + "_small.jpg"
            result = subprocess.run(
                [
                    "ffmpeg",
                    "-y",
                    "-i",
                    thumb_path,
                    "-vf",
                    "scale=320:-1",
                    "-q:v",
                    "5",
                    compressed_thumb,
                ],
                capture_output=True,
                text=True,
                check=False,
            )
            if result.returncode == 0:
                logger.info("Thumbnail compressed for: %s", title)
                self._report("Thumbnail", "done", "Thumbnail compressed")
            else:
                logger.error(
                    "ffmpeg thumbnail compression failed for %s: %s", title, result.stderr[-800:]
                )
                self._report("Thumbnail", "failed", "Thumbnail compression failed")
                compressed_thumb = None
        else:
            self._report("Thumbnail", "done", "No thumbnail found, skipped")

        # --- metadata ------------------------------------------------------
        self._report("Metadata", "active", "Embedding metadata (mutagen/Opus)...")
        try:
            audio = OggOpus(filepath)
            audio["title"] = title
            audio["artist"] = artist

            if compressed_thumb and os.path.exists(compressed_thumb):
                pic = Picture()
                with open(compressed_thumb, "rb") as f:
                    pic.data = f.read()
                pic.type = 3  # front cover
                pic.mime = "image/jpeg"
                pic.desc = "Cover"
                audio["metadata_block_picture"] = [
                    base64.b64encode(pic.write()).decode("ascii")
                ]

            audio.save()
            logger.info("Metadata embedded for: %s", title)
            self._report("Metadata", "done", "Metadata embedded")
        except Exception as e:  # noqa: BLE001
            logger.error("mutagen tagging failed for %s: %s", title, e)
            self._report("Metadata", "failed", f"Metadata embed failed: {e}")

        # --- archive ------------------------------------------------------
        self._report("Archive", "active", "Moving to archive...")
        archive_dir = config.SONGS_ARCHIVE_DIR
        os.makedirs(archive_dir, exist_ok=True)
        final_filepath = os.path.join(archive_dir, os.path.basename(filepath))

        try:
            shutil.move(filepath, final_filepath)
            self._report("Archive", "done", "Moved to archive")
        except OSError as e:
            logger.error("Failed to move %s to archive: %s", filepath, e)
            self._report("Archive", "failed", f"Move to archive failed: {e}")
            info["filepath"] = filepath
            return [], info

        # Nuke all leftovers in temp_workspace for this specific file
        for ext in [".jpg", "_small.jpg", ".webp", ".part", ".ytdl"]:
            junk_file = base_path + ext
            if os.path.exists(junk_file):
                try:
                    os.remove(junk_file)
                except OSError:
                    logger.warning("Could not remove leftover %s", junk_file)

        info["filepath"] = final_filepath
        return [], info


def to_music_youtube_url(url: str) -> str:
    """Rewrites any youtube.com / youtu.be / m.youtube.com link into the
    equivalent music.youtube.com link.

    This is what actually controls quality, not a yt-dlp option: YouTube
    only exposes the ~256kbps Premium Opus itag (774) through the Music
    surface. The exact same video watched via a plain youtube.com watch
    URL commonly only offers the ~128-160kbps Opus itag (251), even with
    valid Premium cookies. So every URL needs to go in as
    music.youtube.com for the higher-bitrate / lower-compression stream
    to even be on the table.
    """
    had_scheme = "://" in url
    parsed = urlparse(url if had_scheme else f"https://{url}")
    host = parsed.netloc.lower()
    if host == "music.youtube.com":
        return url
    if host not in ("www.youtube.com", "youtube.com", "m.youtube.com", "youtu.be"):
        return url  # not a YouTube URL at all -- leave it alone

    qs = parse_qs(parsed.query)
    video_id = None
    if host == "youtu.be":
        video_id = parsed.path.lstrip("/").split("/")[0] or None
    elif parsed.path.startswith("/shorts/"):
        video_id = parsed.path.split("/shorts/")[1].split("/")[0]
    elif "v" in qs:
        video_id = qs["v"][0]

    if video_id:
        new_qs = {"v": video_id}
        if "list" in qs:
            new_qs["list"] = qs["list"][0]
        return f"https://music.youtube.com/watch?{urlencode(new_qs)}"

    if "list" in qs:
        return f"https://music.youtube.com/playlist?{urlencode({'list': qs['list'][0]})}"

    return url  # couldn't find a video/playlist id -- leave it alone


def parse_input_urls(raw_input: str) -> list[str]:
    lines = raw_input.splitlines()
    urls = []
    for line in lines:
        cleaned = line.strip()
        if not cleaned:
            continue
        cleaned = re.sub(r"&(si|ab_channel|pp|feature)=[^&]+", "", cleaned)
        urls.append(to_music_youtube_url(cleaned))
    return urls


def _flatten_entry(entry: dict) -> str | None:
    entry_url = entry.get("url") or entry.get("webpage_url") or entry.get("id")
    if not entry_url:
        return None
    if not str(entry_url).startswith("http"):
        entry_url = f"https://music.youtube.com/watch?v={entry_url}"
    return to_music_youtube_url(str(entry_url))


def list_entries(url: str) -> list[dict]:
    """Expands a pasted URL into individual entries. Playlists become one
    entry per video, tagged from_playlist=True so the UI can badge them."""
    url = to_music_youtube_url(url)
    ydl_opts = {
        "quiet": True,
        "skip_download": True,
        "extract_flat": "in_playlist",
        "extractor_args": {"youtube": {"player-client": ["android", "web"]}},
        "logger": YTDLPLogger(),
        **_cookie_opts(),
    }
    with yt_dlp.YoutubeDL(ydl_opts) as ydl:
        try:
            info = ydl.extract_info(url, download=False)
        except Exception:
            logger.exception("list_entries failed for %s", url)
            return [{"title": url, "url": url, "from_playlist": False}]

    if not info:
        return [{"title": url, "url": url, "from_playlist": False}]

    if info.get("entries"):
        entries = []
        for entry in info["entries"]:
            if not entry:
                continue
            entry_url = _flatten_entry(entry)
            if not entry_url:
                continue
            entries.append(
                {
                    "title": entry.get("title") or entry_url,
                    "url": entry_url,
                    "from_playlist": True,
                }
            )
        return entries or [{"title": url, "url": url, "from_playlist": False}]

    return [
        {
            "title": info.get("title", url),
            "url": info.get("webpage_url", url),
            "from_playlist": False,
        }
    ]


def search_videos(query: str, limit: int = 8) -> list[dict]:
    """Search directly on music.youtube.com (not a generic ytsearch:),
    so results are songs from the Music catalog/surface and the itags
    yt-dlp sees for them include the high-bitrate Music-only streams."""
    if not query.strip():
        return []

    search_url = f"https://music.youtube.com/search?q={quote(query)}"
    ydl_opts = {
        "quiet": True,
        "skip_download": True,
        "extract_flat": "in_playlist",
        "playlist_items": f"1-{max(1, limit)}",
        "extractor_args": {"youtube": {"player-client": ["android", "web"]}},
        "logger": YTDLPLogger(),
        **_cookie_opts(),
    }

    with yt_dlp.YoutubeDL(ydl_opts) as ydl:
        try:
            info = ydl.extract_info(search_url, download=False)
        except Exception:
            logger.exception("search_videos failed for query %r", query)
            return []

    results = []
    for entry in (info or {}).get("entries") or []:
        if not entry:
            continue
        entry_url = _flatten_entry(entry)
        if not entry_url:
            continue
        results.append(
            {
                "title": entry.get("title") or entry_url,
                "url": entry_url,
                "uploader": entry.get("uploader") or entry.get("channel") or "",
                "duration": entry.get("duration"),
            }
        )
    return results


def list_audio_formats(url: str) -> list[dict]:
    """Returns the audio-only formats yt-dlp sees for one video, so the UI
    (or extension) can let you pick exactly once instead of guessing.

    Each entry: {id, ext, acodec, abr, note}. Sorted best-first.
    """
    url = to_music_youtube_url(url)
    ydl_opts = {
        "quiet": True,
        "skip_download": True,
        "noplaylist": True,
        "extractor_args": {"youtube": {"player-client": ["android", "web"]}},
        "logger": YTDLPLogger(),
        **_cookie_opts(),
    }
    with yt_dlp.YoutubeDL(ydl_opts) as ydl:
        info = ydl.extract_info(url, download=False)
    if not info:
        return []
    formats = []
    for f in info.get("formats") or []:
        # Keep audio-only (or audio+video fallback with audio codec).
        if f.get("vcodec") not in (None, "none"):
            continue
        if f.get("acodec") in (None, "none"):
            continue
        formats.append(
            {
                "id": f.get("format_id"),
                "ext": f.get("ext"),
                "acodec": f.get("acodec"),
                "abr": f.get("abr") or f.get("tbr"),
                "note": f.get("format_note") or "",
            }
        )
    formats.sort(key=lambda f: (f["abr"] or 0), reverse=True)
    return formats


def get_video_title(url: str) -> str:
    """Kept for backwards compatibility; list_entries() is now used for queueing."""
    entries = list_entries(url)
    if len(entries) > 1:
        return f"Playlist ({len(entries)} tracks)"
    return str(entries[0]["title"])


def download_audio(
    url: str,
    on_progress=None,
    cancel_event: threading.Event | None = None,
    aria2_connections: int | None = None,
    format_selector: str | None = None,
):
    """on_progress(step: str, state: 'active'|'done'|'failed', detail: str)
    is called at every pipeline transition (Download, Extract Audio,
    Thumbnail, Metadata, Archive) so the caller can render a live step
    tracker instead of a single opaque status string.

    Quality: AUDIO_FORMAT_SELECTOR asks for the best *native Opus*
    stream your cookies are entitled to (up to 256kbps on YouTube
    Premium, ~128-160kbps on a free account -- see config.py). Because
    the extraction target codec matches that source codec, yt-dlp does
    a lossless stream copy instead of re-encoding, so whatever bitrate
    YouTube served is exactly what ends up in the archive -- no extra
    lossy generation loss on top, unlike transcoding to a fixed-bitrate
    MP3/AAC.
    """

    aria2_connections = aria2_connections or config.ARIA2C_CONNECTIONS

    def hook(d):
        if cancel_event is not None and cancel_event.is_set():
            raise DownloadCancelled("Download cancelled by user")
        if d["status"] == "downloading":
            downloaded = d.get("downloaded_bytes") or 0
            total = d.get("total_bytes") or d.get("total_bytes_estimate")
            if total:
                detail = f"Downloading... {downloaded / total * 100:.1f}%"
            else:
                clean = re.sub(r"\x1b\[[0-9;]*m", "", d.get("_percent_str", "").strip())
                detail = f"Downloading... {clean}" if clean else "Downloading..."
            if on_progress:
                on_progress("Download", "active", detail)
        elif d["status"] == "finished":
            if on_progress:
                on_progress("Download", "done", "Download complete")
                on_progress("Extract Audio", "active", "Extracting audio (Opus)...")

    ydl_opts = {
        "format": format_selector or config.AUDIO_FORMAT_SELECTOR,
        "outtmpl": f"{config.TEMP_WORKSPACE_DIR}/%(title)s [%(id)s].%(ext)s",
        "download_archive": config.ARCHIVE_FILE,
        "noplaylist": True,
        "nooverwrites": True,
        "continuedl": True,
        "writethumbnail": True,
        "external_downloader": "aria2c",
        "external_downloader_args": {
            "aria2c": ["-x", str(aria2_connections), "-s", str(aria2_connections), "-k", "1M"]
        },
        "postprocessors": [
            {"key": "FFmpegThumbnailsConvertor", "format": "jpg"},
            {
                "key": "FFmpegExtractAudio",
                "preferredcodec": config.AUDIO_CODEC,
                # Only used if yt-dlp actually has to transcode (source
                # wasn't already Opus). When source == target codec,
                # yt-dlp stream-copies losslessly and ignores this.
                "preferredquality": config.FALLBACK_TRANSCODE_BITRATE,
            },
        ],
        "postprocessor_args": {
            # Just speeds up the ffmpeg calls; does not force a codec, so
            # it can't clobber the -acodec choice made above (the old
            # pipeline's "-c:a aac -b:a 96k" here was quietly re-encoding
            # every download down to 96kbps AAC regardless of source
            # quality -- that's gone now).
            "ffmpeg": ["-threads", "0"]
        },
        "extractor_args": {"youtube": {"player-client": ["android", "web"]}},
        "progress_hooks": [hook],
        "logger": YTDLPLogger(),
        **_cookie_opts(),
    }

    with yt_dlp.YoutubeDL(ydl_opts) as ydl:
        ydl.add_post_processor(OpusTagPP(on_progress=on_progress, cancel_event=cancel_event))
        ydl.download([url])