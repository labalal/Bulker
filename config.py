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

QUEUE_STORE_PATH = os.getenv("QUEUE_STORE_PATH", "queue_store.json")
LOG_FILE = os.getenv("LOG_FILE", "app.log")

ARIA2C_CONNECTIONS = int(os.getenv("ARIA2C_CONNECTIONS", "16"))
DEFAULT_CONCURRENCY = int(os.getenv("DEFAULT_CONCURRENCY", "3"))
MAX_CONCURRENCY = int(os.getenv("MAX_CONCURRENCY", "6"))

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