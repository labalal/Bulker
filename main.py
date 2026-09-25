import asyncio
import json
import logging
import os
import threading
import uuid
from datetime import UTC, datetime

from fastapi import FastAPI, HTTPException
from fastapi.middleware.cors import CORSMiddleware
from fastapi.responses import FileResponse
from fastapi.staticfiles import StaticFiles
from pydantic import BaseModel

import config
from cleaning import cleanup_workspace
from downloader import (
    STEP_NAMES,
    DownloadCancelled,
    cleanup_existing_mess,
    download_audio,
    list_entries,
    parse_input_urls,
    search_videos,
)

logging.basicConfig(
    filename=config.LOG_FILE,
    level=logging.INFO,
    format="%(asctime)s [%(levelname)s] %(message)s",
)
logger = logging.getLogger("audio_downloader")

app = FastAPI()

app.add_middleware(
    CORSMiddleware,
    allow_origins=["*"],
    allow_credentials=True,
    allow_methods=["*"],
    allow_headers=["*"],
)

app.mount("/static", StaticFiles(directory="static"), name="static")

items: dict[str, dict] = {}
order: list[str] = []
history: list[dict] = []
queue_lock = asyncio.Lock()


def fresh_steps() -> dict:
    return {name: "pending" for name in STEP_NAMES}


def is_active_status(status: str, steps: dict | None = None) -> bool:
    if status in ("Starting...", "Cancelling..."):
        return True
    if steps and any(s == "active" for s in steps.values()):
        return True
    # Fallback for status strings saved by older versions of this app.
    return status.startswith(("Downloading", "Processing"))


class RuntimeSettings:
    def __init__(self):
        self.aria2_connections = config.ARIA2C_CONNECTIONS


runtime_settings = RuntimeSettings()


class ConcurrencyLimiter:
    def __init__(self, limit: int):
        self.limit = limit
        self._active = 0
        self._lock = asyncio.Lock()

    async def acquire(self):
        while True:
            async with self._lock:
                if self._active < self.limit:
                    self._active += 1
                    return
            await asyncio.sleep(0.2)

    async def release(self):
        async with self._lock:
            self._active = max(0, self._active - 1)


limiter = ConcurrencyLimiter(limit=config.DEFAULT_CONCURRENCY)


class BulkURLRequest(BaseModel):
    raw_input: str


class SearchQuery(BaseModel):
    query: str


class ReorderRequest(BaseModel):
    ids: list[str]


class SettingsRequest(BaseModel):
    concurrency: int | None = None
    aria2_connections: int | None = None


def make_item(url: str, title: str, from_playlist: bool = False) -> dict:
    return {
        "id": str(uuid.uuid4())[:8],
        "url": url,
        "title": title,
        "from_playlist": from_playlist,
        "status": "Waiting in queue...",
        "steps": fresh_steps(),
        "cancel_event": threading.Event(),
    }


def public_item(item: dict) -> dict:
    return {k: v for k, v in item.items() if k != "cancel_event"}


def record_history(item: dict):
    history.append(
        {
            "id": item["id"],
            "title": item["title"],
            "status": item["status"],
            "from_playlist": item.get("from_playlist", False),
            "finished_at": datetime.now(UTC).isoformat(timespec="seconds"),
        }
    )
    if len(history) > config.HISTORY_LIMIT:
        del history[: len(history) - config.HISTORY_LIMIT]


def save_state():
    snapshot = {
        "order": list(order),
        "items": [
            {
                "id": item["id"],
                "url": item["url"],
                "title": item["title"],
                "from_playlist": item.get("from_playlist", False),
                "status": item["status"],
            }
            for item in items.values()
        ],
        "history": history[-config.HISTORY_LIMIT :],
    }
    try:
        tmp_path = config.QUEUE_STORE_PATH + ".tmp"
        with open(tmp_path, "w") as f:
            json.dump(snapshot, f)
        os.replace(tmp_path, config.QUEUE_STORE_PATH)
    except Exception:
        logger.exception("Failed to save queue state")


def load_state():
    if not os.path.exists(config.QUEUE_STORE_PATH):
        return
    try:
        with open(config.QUEUE_STORE_PATH) as f:
            data = json.load(f)
    except Exception:
        logger.exception("Failed to load queue , starting with an empty queue")
        return

    for stored in data.get("items", []):
        status = stored.get("status", "Waiting in queue...")
        if is_active_status(status):
            status = "Waiting in queue..."
        item_id = stored["id"]
        items[item_id] = {
            "id": item_id,
            "url": stored["url"],
            "title": stored["title"],
            "from_playlist": stored.get("from_playlist", True),
            "status": status,
            "steps": fresh_steps(),
            "cancel_event": threading.Event(),
        }

    order.extend([i for i in data.get("order", []) if i in items])
    history.extend(data.get("history", []))
    logger.info("Loaded %d queued item(s) from %s", len(items), config.QUEUE_STORE_PATH)


async def persist():
    await asyncio.to_thread(save_state)


async def periodic_saver():
    while True:
        await asyncio.sleep(2)
        await persist()


async def claim_next_item():
    async with queue_lock:
        for item_id in order:
            item = items.get(item_id)
            if item and item["status"] == "Waiting in queue...":
                item["status"] = "Starting..."
                return item
    return None


async def auto_remove_after_delay(item_id: str, delay: float):
    await asyncio.sleep(delay)
    async with queue_lock:
        it = items.get(item_id)
        if it and it["status"] == "Completed":
            items.pop(item_id, None)
            if item_id in order:
                order.remove(item_id)
    await persist()


async def worker():
    while True:
        item = await claim_next_item()
        if item is None:
            await asyncio.sleep(0.5)
            continue

        if item["cancel_event"].is_set():
            item["status"] = "Cancelled"
            record_history(item)
            continue

        await limiter.acquire()
        try:

            def on_progress(step: str, state: str, detail: str, item=item):
                if item["cancel_event"].is_set() and state != "failed":
                    return
                item["steps"][step] = state
                item["status"] = detail

            logger.info("Starting download: %s (%s)", item["title"], item["url"])
            try:
                await asyncio.to_thread(
                    download_audio,
                    item["url"],
                    on_progress,
                    item["cancel_event"],
                    runtime_settings.aria2_connections,
                )
                if item["cancel_event"].is_set():
                    item["status"] = "Cancelled"
                    logger.info("Cancelled: %s", item["title"])
                else:
                    item["status"] = "Completed"
                    logger.info("Completed: %s", item["title"])
            except DownloadCancelled:
                item["status"] = "Cancelled"
                logger.info("Cancelled: %s", item["title"])
            except Exception as e:  # noqa: BLE001
                item["status"] = f"Error: {e!s}"
                logger.error("Failed: %s -- %s", item["title"], str(e))
        finally:
            await limiter.release()

        record_history(item)
        if item["status"] == "Completed":
            asyncio.create_task(
                auto_remove_after_delay(item["id"], config.AUTO_REMOVE_DELAY_SECONDS)
            )
        else:
            await persist()


@app.on_event("startup")
async def startup_event():
    cleanup_existing_mess()
    os.makedirs(config.SONGS_ARCHIVE_DIR, exist_ok=True)
    os.makedirs(config.TEMP_WORKSPACE_DIR, exist_ok=True)
    load_state()
    for _ in range(config.MAX_CONCURRENCY):
        asyncio.create_task(worker())
    asyncio.create_task(periodic_saver())


@app.get("/")
def read_root():
    return FileResponse("static/index.html")


@app.post("/api/add")
async def add_to_queue(req: BulkURLRequest):
    raw_urls = parse_input_urls(req.raw_input)
    added_ids = []

    for raw_url in raw_urls:
        try:
            entries = await asyncio.to_thread(list_entries, raw_url)
        except Exception:  # noqa: BLE001
            entries = [{"title": raw_url, "url": raw_url, "from_playlist": False}]

        async with queue_lock:
            for entry in entries:
                item = make_item(entry["url"], entry["title"], entry.get("from_playlist", False))
                items[item["id"]] = item
                order.append(item["id"])
                added_ids.append(item["id"])

    logger.info("Added %d item(s) to queue", len(added_ids))
    await persist()
    return {"message": f"Added {len(added_ids)} item(s)", "ids": added_ids}


@app.get("/api/search")
async def search(q: str, limit: int | None = None):
    if not q.strip():
        return {"results": []}
    results = await asyncio.to_thread(search_videos, q, limit or config.SEARCH_RESULT_LIMIT)
    return {"results": results}


@app.get("/api/queue")
def get_queue():
    return {"queue": [public_item(items[i]) for i in order if i in items]}


@app.get("/api/history")
def get_history():
    return {"history": list(reversed(history[-50:]))}


@app.patch("/api/queue/reorder")
async def reorder_queue(req: ReorderRequest):
    async with queue_lock:
        new_order = [i for i in req.ids if i in items]
        missing = [i for i in order if i not in new_order]
        order.clear()
        order.extend(new_order + missing)
    await persist()
    return {"message": "Reordered"}


@app.delete("/api/queue/{item_id}")
async def delete_item(item_id: str):
    async with queue_lock:
        item = items.get(item_id)
        if item is None:
            raise HTTPException(status_code=404, detail="Item not found")

        if not is_active_status(item["status"], item.get("steps")):
            items.pop(item_id, None)
            if item_id in order:
                order.remove(item_id)
            await persist()
            return {"message": "Removed"}

        item["cancel_event"].set()
        item["status"] = "Cancelling..."
    return {"message": "Cancel requested"}


@app.post("/api/queue/{item_id}/retry")
async def retry_item(item_id: str):
    async with queue_lock:
        item = items.get(item_id)
        if item is None:
            raise HTTPException(status_code=404, detail="Item not found")
        if not (item["status"].startswith("Error") or item["status"] == "Cancelled"):
            raise HTTPException(
                status_code=400, detail="Only failed or cancelled items can be retried"
            )
        item["cancel_event"] = threading.Event()
        item["status"] = "Waiting in queue..."
        item["steps"] = fresh_steps()
    logger.info("Retrying: %s", item["title"])
    await persist()
    return {"message": "Retrying"}


@app.delete("/api/queue")
async def clear_finished():
    removable = {"Completed", "Cancelled"}
    async with queue_lock:
        to_remove = [
            i
            for i in order
            if i in items
            and (items[i]["status"] in removable or items[i]["status"].startswith("Error"))
        ]
        for i in to_remove:
            items.pop(i, None)
        order[:] = [i for i in order if i not in to_remove]
    await persist()
    return {"message": f"Cleared {len(to_remove)} item(s)"}


@app.post("/api/cleanup")
async def run_cleanup():
    removed = await asyncio.to_thread(cleanup_workspace)
    logger.info("Manual cleanup run via API: removed %d file(s)", removed)
    return {"removed": removed}


@app.get("/api/settings")
def get_settings():
    return {
        "concurrency": limiter.limit,
        "max_concurrency": config.MAX_CONCURRENCY,
        "aria2_connections": runtime_settings.aria2_connections,
        "temp_workspace_dir": config.TEMP_WORKSPACE_DIR,
        "songs_archive_dir": config.SONGS_ARCHIVE_DIR,
        "cookies_file": config.COOKIES_FILE,
        "log_file": config.LOG_FILE,
    }


@app.post("/api/settings")
def update_settings(req: SettingsRequest):
    if req.concurrency is not None:
        limiter.limit = max(1, min(config.MAX_CONCURRENCY, req.concurrency))
    if req.aria2_connections is not None:
        runtime_settings.aria2_connections = max(1, min(32, req.aria2_connections))
    return {"concurrency": limiter.limit, "aria2_connections": runtime_settings.aria2_connections}


@app.get("/api/logs")
def get_logs(lines: int = 200):
    if not os.path.exists(config.LOG_FILE):
        return {"logs": ""}
    with open(config.LOG_FILE) as f:
        content = f.readlines()
    return {"logs": "".join(content[-lines:])}