import logging
import re
from pathlib import Path

import config

logger = logging.getLogger("audio_downloader")

TEMP_DIR = Path(config.TEMP_WORKSPACE_DIR)
ARCHIVE_DIR = Path(config.SONGS_ARCHIVE_DIR)
DOWNLOADS_DIR = Path(config.DOWNLOAD_DIR)


def extract_identifier(filename):
    match = re.search(r"\[([A-Za-z0-9_-]{11})\]", filename)
    return match.group(1) if match else Path(filename).stem


def get_all_identifiers(directory):
    ids = set()
    if directory.exists():
        for path in directory.rglob("*"):
            if path.is_file():
                ids.add(extract_identifier(path.name))
    return ids


def cleanup_workspace() -> int:
    """Remove leftover files in the temp workspace that have already made
    it into the archive (or the legacy downloads folder). Safe to call any
    time -- returns the number of files removed."""
    if not TEMP_DIR.exists():
        logger.info("cleanup_workspace: %s not found, nothing to do", TEMP_DIR)
        return 0

    archived_ids = get_all_identifiers(ARCHIVE_DIR)
    downloaded_ids = get_all_identifiers(DOWNLOADS_DIR)
    all_saved = archived_ids | downloaded_ids

    removed_count = 0
    for file_path in TEMP_DIR.iterdir():
        if file_path.is_file():
            file_id = extract_identifier(file_path.name)
            if file_id in all_saved:
                file_path.unlink()
                logger.info("cleanup_workspace: removed %s", file_path.name)
                removed_count += 1

    logger.info(
        "cleanup_workspace: removed %d processed file(s) from %s",
        removed_count,
        TEMP_DIR,
    )
    return removed_count


if __name__ == "__main__":
    n = cleanup_workspace()
    print(f"Cleanup complete. Removed {n} processed files from {TEMP_DIR}.")
