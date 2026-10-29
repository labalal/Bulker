import os

try:
    from dotenv import load_dotenv

    load_dotenv()
except ImportError:
    # python-dotenv isn't installed -- falls back to whatever's already in
    # the environment plus the defaults below. `pip install python-dotenv`
    # to enable loading a .env file.
    pass

COOKIES_FILE = os.getenv("COOKIES_FILE", "cookies.txt")
ARCHIVE_FILE = os.getenv("ARCHIVE_FILE", "archive.txt")  # yt-dlp's own dedupe file

# Legacy folder some old downloads may still be sitting in (rescued into
# SONGS_ARCHIVE_DIR by cleanup_existing_mess() at startup).
DOWNLOAD_DIR = os.getenv("DOWNLOAD_DIR", "downloads")

# Active/in-progress downloads live here: .part files, aria2 control files,
# raw extracted audio before tagging, temp thumbnails. Nothing here is
# "real" until it's been moved into SONGS_ARCHIVE_DIR.
TEMP_WORKSPACE_DIR = os.getenv("TEMP_WORKSPACE_DIR", "temp_workspace")

# The finished library -- one flat folder, since the offline player reads
# Vorbis-comment tags on the .opus files rather than folder structure.
SONGS_ARCHIVE_DIR = os.getenv("SONGS_ARCHIVE_DIR", "Songs_archive")

# Per-song catalog of everything downloaded (what + quality + where).
# JSON file for the Python app; the Rust build keeps identical data in SQLite.
SONG_LIBRARY_FILE = os.getenv("SONG_LIBRARY_FILE", "library.json")

QUEUE_STORE_PATH = os.getenv("QUEUE_STORE_PATH", "queue_store.json")
LOG_FILE = os.getenv("LOG_FILE", "app.log")

ARIA2C_CONNECTIONS = int(os.getenv("ARIA2C_CONNECTIONS", "16"))
DEFAULT_CONCURRENCY = int(os.getenv("DEFAULT_CONCURRENCY", "3"))
MAX_CONCURRENCY = int(os.getenv("MAX_CONCURRENCY", "6"))

# --- Download resilience -------------------------------------------------
#
# YouTube throttles aggressively, especially when a downloader opens many
# parallel ranged connections at once. Without retries a single throttle
# storm kills a queue item with "aria2c exited with code 2" (aria2c's
# timeout/abort exit code) after minutes of apparently-stuck progress.
# Two layers of retry protect against that:
#
#   * DOWNLOAD_RETRIES / SOCKET_TIMEOUT are passed to yt-dlp itself, and
#     max-tries/retry-wait/connect-timeout go to aria2c -- so individual
#     connections get re-established instead of aborting the download.
#   * AUTO_RETRY_ATTEMPTS is the queue-level backstop in main.py: even if
#     an entire download attempt still dies, the worker retries the item
#     with increasing backoff before marking it as a permanent Error.
DOWNLOAD_RETRIES = int(os.getenv("DOWNLOAD_RETRIES", "10"))
SOCKET_TIMEOUT = int(os.getenv("SOCKET_TIMEOUT", "30"))
AUTO_RETRY_ATTEMPTS = int(os.getenv("AUTO_RETRY_ATTEMPTS", "3"))
AUTO_RETRY_BACKOFF_SECONDS = int(os.getenv("AUTO_RETRY_BACKOFF_SECONDS", "15"))

# Per-download speed cap in KiB/s (0 = unlimited), changeable live from the
# Settings tab. A modest cap makes the parallel connections look like normal
# playback traffic to YouTube, which reduces the throttling that used to
# show up as "aria2c exited with code 2" timeouts. Note this is implemented
# with aria2c's own --max-overall-download-limit, because yt-dlp's native
# `ratelimit` option is ignored when an external downloader is in use.
DEFAULT_RATE_LIMIT_KIBS = int(os.getenv("RATE_LIMIT_KIBS", "0"))

SEARCH_RESULT_LIMIT = int(os.getenv("SEARCH_RESULT_LIMIT", "8"))
AUTO_REMOVE_DELAY_SECONDS = int(os.getenv("AUTO_REMOVE_DELAY_SECONDS", "3"))
HISTORY_LIMIT = int(os.getenv("HISTORY_LIMIT", "200"))

# --- Audio quality -----------------------------------------------------
#
# How the quality actually gets decided:
#
#   1. yt-dlp asks YouTube for the list of formats available for the
#      given cookies. A free/anonymous session only gets offered the
#      ~128-160kbps Opus stream (itag 251). A YouTube Premium session
#      (valid Premium cookies in COOKIES_FILE) additionally gets offered
#      a ~256kbps Opus stream (itag 774) on YouTube Music tracks.
#   2. AUDIO_FORMAT_SELECTOR below tells yt-dlp to grab the best *native
#      Opus* stream from whatever list it was offered. It doesn't -- and
#      can't -- upgrade a free account to 256kbps; it just makes sure
#      you always get the highest-bitrate Opus stream your account is
#      actually entitled to, whatever that is.
#   3. Because the target codec (opus) matches the source codec, yt-dlp
#      does a lossless container remux (stream copy) instead of
#      re-encoding. This is the important bit for quality: the old
#      pipeline re-encoded everything down to 96kbps AAC, which threw
#      away real quality even on a Premium 256kbps source. Now nothing
#      gets re-encoded (and so no quality is lost) as long as YouTube
#      served Opus, which it does for effectively all music tracks.
#
# FALLBACK_TRANSCODE_BITRATE only matters on the rare video where
# YouTube didn't offer a native Opus stream at all (so yt-dlp has to
# transcode from whatever it got, e.g. AAC/M4A) -- it will never make a
# 128kbps source sound like 256kbps, it just caps how hard we push the
# opus encoder on that fallback path.
AUDIO_CODEC = os.getenv("AUDIO_CODEC", "opus")
AUDIO_FORMAT_SELECTOR = os.getenv(
    "AUDIO_FORMAT_SELECTOR", "bestaudio[acodec^=opus]/bestaudio/best"
)
FALLBACK_TRANSCODE_BITRATE = os.getenv("FALLBACK_TRANSCODE_BITRATE", "256")  # kbps