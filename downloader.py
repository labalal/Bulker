import logging
import os
import re
import shutil
import subprocess
import threading

import yt_dlp

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


def cleanup_existing_mess():
    """Rescues old m4a files to Songs_archive and deletes the rest."""
    archive_dir = config.SONGS_ARCHIVE_DIR
    os.makedirs(archive_dir, exist_ok=True)
    os.makedirs(config.TEMP_WORKSPACE_DIR, exist_ok=True)

    if not os.path.exists(config.DOWNLOAD_DIR):
        return

    for root, _, files in os.walk(config.DOWNLOAD_DIR):
        for file in files:
            full_path = os.path.join(root, file)
            if file.endswith(".m4a"):
                try:
                    shutil.move(full_path, os.path.join(archive_dir, file))
                except OSError:
                    logger.exception("Could not rescue %s to archive", full_path)
            elif file.endswith((".jpg", ".webp", ".part", ".webm", ".temp")):
                try:
                    os.remove(full_path)
                except OSError:
                    logger.exception("Could not remove leftover %s", full_path)


class AtomicParsleyPP(yt_dlp.postprocessor.PostProcessor):
    """Compresses the thumbnail, embeds it + title/artist with
    AtomicParsley, then moves the finished file into Songs_archive.
    Reports each of those steps through on_progress so the UI can show a
    green/red status per step instead of a black box."""

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

        self._report("Extract Audio", "done", "Audio extracted")

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

        if not filepath.endswith(".m4a"):
            filepath = os.path.splitext(filepath)[0] + ".m4a"

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
        self._report("Metadata", "active", "Embedding metadata (AtomicParsley)...")
        atomicparsley_cmd = [
            "atomicparsley",
            filepath,
            "--title",
            title,
            "--artist",
            artist,
            "--overWrite",
        ]
        if compressed_thumb and os.path.exists(compressed_thumb):
            atomicparsley_cmd[4:4] = ["--artwork", compressed_thumb]

        result = subprocess.run(atomicparsley_cmd, capture_output=True, text=True, check=False)
        if result.returncode == 0:
            logger.info("Metadata embedded for: %s", title)
            self._report("Metadata", "done", "Metadata embedded")
        else:
            logger.error("AtomicParsley failed for %s: %s", title, result.stderr[-800:])
            self._report("Metadata", "failed", "Metadata embed failed")

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


def parse_input_urls(raw_input: str) -> list[str]:
    lines = raw_input.splitlines()
    urls = []
    for line in lines:
        cleaned = line.strip()
        if not cleaned:
            continue
        cleaned = re.sub(r"&(si|ab_channel|pp|feature)=[^&]+", "", cleaned)
        urls.append(cleaned)
    return urls


def _flatten_entry(entry: dict) -> str | None:
    entry_url = entry.get("url") or entry.get("webpage_url") or entry.get("id")
    if not entry_url:
        return None
    if not str(entry_url).startswith("http"):
        entry_url = f"https://www.youtube.com/watch?v={entry_url}"
    return str(entry_url)


def list_entries(url: str) -> list[dict]:
    """Expands a pasted URL into individual entries. Playlists become one
    entry per video, tagged from_playlist=True so the UI can badge them."""
    ydl_opts = {
        "quiet": True,
        "skip_download": True,
        "cookiefile": config.COOKIES_FILE,
        "extract_flat": "in_playlist",
        "extractor_args": {"youtube": {"player-client": ["android", "web"]}},
        "logger": YTDLPLogger(),
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
    """Search YouTube directly instead of needing a pasted link."""
    if not query.strip():
        return []

    ydl_opts = {
        "quiet": True,
        "skip_download": True,
        "cookiefile": config.COOKIES_FILE,
        "extract_flat": "in_playlist",
        "extractor_args": {"youtube": {"player-client": ["android", "web"]}},
        "logger": YTDLPLogger(),
    }
    search_key = f"ytsearch{max(1, limit)}:{query}"

    with yt_dlp.YoutubeDL(ydl_opts) as ydl:
        try:
            info = ydl.extract_info(search_key, download=False)
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
):
    """on_progress(step: str, state: 'active'|'done'|'failed', detail: str)
    is called at every pipeline transition (Download, Extract Audio,
    Thumbnail, Metadata, Archive) so the caller can render a live step
    tracker instead of a single opaque status string."""

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
                on_progress("Extract Audio", "active", "Extracting audio (ffmpeg)...")

    ydl_opts = {
        "format": "bestaudio/best",
        "outtmpl": f"{config.TEMP_WORKSPACE_DIR}/%(title)s [%(id)s].%(ext)s",
        "cookiefile": config.COOKIES_FILE,
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
            {"key": "FFmpegExtractAudio", "preferredcodec": "m4a"},
        ],
        "postprocessor_args": {
            # loudnorm was removed -- it re-analyzes the whole file in a
            # second pass and was burning a lot of CPU/RAM for marginal
            # benefit at this volume of downloads.
            "ffmpeg": ["-c:a", "aac", "-b:a", "96k", "-threads", "0"]
        },
        "extractor_args": {"youtube": {"player-client": ["android", "web"]}},
        "progress_hooks": [hook],
        "logger": YTDLPLogger(),
    }

    with yt_dlp.YoutubeDL(ydl_opts) as ydl:
        ydl.add_post_processor(AtomicParsleyPP(on_progress=on_progress, cancel_event=cancel_event))
        ydl.download([url])
