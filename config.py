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
# ID3/MP4 tags rather than folder structure.
SONGS_ARCHIVE_DIR = os.getenv("SONGS_ARCHIVE_DIR", "Songs_archive")

QUEUE_STORE_PATH = os.getenv("QUEUE_STORE_PATH", "queue_store.json")
LOG_FILE = os.getenv("LOG_FILE", "app.log")

ARIA2C_CONNECTIONS = int(os.getenv("ARIA2C_CONNECTIONS", "16"))
DEFAULT_CONCURRENCY = int(os.getenv("DEFAULT_CONCURRENCY", "3"))
MAX_CONCURRENCY = int(os.getenv("MAX_CONCURRENCY", "6"))

SEARCH_RESULT_LIMIT = int(os.getenv("SEARCH_RESULT_LIMIT", "8"))
AUTO_REMOVE_DELAY_SECONDS = int(os.getenv("AUTO_REMOVE_DELAY_SECONDS", "3"))
HISTORY_LIMIT = int(os.getenv("HISTORY_LIMIT", "200"))
