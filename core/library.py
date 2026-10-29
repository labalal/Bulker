"""Per-song library catalog for the Python implementation.

Every completed download gets one entry appended to a plain JSON file
(the Python app deliberately stays JSON-store-based; the Rust build keeps
the same catalog in SQLite instead). Used by /api/songs and `cli songs`
so you can see what you have, at what quality, and how much of your
10 GB budget is spent.
"""

import json
import os
import threading
from datetime import UTC, datetime

from core import config

_lock = threading.Lock()


def record_song(
    title: str,
    artist: str,
    video_id: str,
    itag: str,
    abr: float | None,
    delivered_kbps: float | None,
    size_bytes: int,
    path: str,
) -> None:
    entry = {
        "video_id": video_id,
        "title": title,
        "artist": artist,
        "itag": itag,
        "abr": abr,
        "delivered_kbps": round(delivered_kbps, 1) if delivered_kbps else None,
        "size_bytes": size_bytes,
        "path": path,
        "downloaded_at": datetime.now(UTC).replace(microsecond=0).isoformat(),
    }
    with _lock:
        catalog = read_songs()
        catalog.append(entry)
        tmp = config.SONG_LIBRARY_FILE + ".tmp"
        try:
            with open(tmp, "w") as f:
                json.dump(catalog, f, indent=1)
            os.replace(tmp, config.SONG_LIBRARY_FILE)
        except OSError:
            import logging

            logging.getLogger("audio_downloader").exception(
                "Failed to write library catalog"
            )


def read_songs() -> list[dict]:
    try:
        with open(config.SONG_LIBRARY_FILE) as f:
            catalog: list[dict] = json.load(f)
            return catalog
    except (OSError, json.JSONDecodeError):
        return []
