"""Bulker CLI -- the command-line client for the Bulker server.

Roles covered by this package:
  * client tools that talk to a running server (add/status/retry/cleanup/logs)
  * `serve` -- starts the server (the daemon) in-process
  * `check` -- runs yt-dlp directly (no server needed) to prove whether the
    account cookies unlock YouTube Music Premium's ~256kbps Opus (itag 774)

Usage:
  uv run python -m cli serve
  uv run python -m cli check <youtube-music-url>
  uv run python -m cli add <url> [more urls...]
  uv run python -m cli status
  uv run python -m cli retry <item_id> | delete <item_id> | clear | cleanup
  uv run python -m cli logs [--lines 50]
"""

import argparse
import json
import sys
import urllib.error
import urllib.request

BASE = "http://127.0.0.1:8000"


def request(method: str, path: str, body: dict | None = None) -> dict:
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(
        f"{BASE}{path}", data=data, method=method,
        headers={"Content-Type": "application/json"},
    )
    try:
        with urllib.request.urlopen(req) as res:
            parsed: dict = json.loads(res.read())  # type: ignore[assignment]
            return parsed
    except urllib.error.URLError as e:
        print(f"error: cannot reach server at {BASE} ({e})\n"
              "start it first:  uv run python -m cli serve")
        sys.exit(1)


def cmd_check(url: str) -> int:
    # Runs its own yt-dlp pass; works without the server.
    from core.downloader import inspect_opus_quality
    result = inspect_opus_quality(url)
    print(json.dumps(result, indent=2))

    available = result.get("premium_opus_available")
    if available is True:
        print("\n[PREMIUM OK] 256kbps Opus (itag 774) available -- downloads will use it.")
        return 0
    if available is False:
        print("\n[NOT PREMIUM] free-tier Opus only (itag 251, 128-160kbps) -- "
              "re-export Premium cookies.txt.")
        return 1
    print("\n[ERROR] could not determine quality -- see the detail above.")
    return 2


def cmd_add(urls: list[str]) -> int:
    data = request("POST", "/api/add", {"raw_input": "\n".join(urls)})
    print(data["message"])
    return 0


def cmd_status() -> int:
    queue = request("GET", "/api/queue")["queue"]
    if not queue:
        print("queue is empty")
        return 0
    for item in queue:
        print(f"[{item['id']}] {item['title'][:60]:<60} {item['status']}")
    return 0


def cmd_retry(item_id: str) -> int:
    print(request("POST", f"/api/queue/{item_id}/retry")["message"])
    return 0


def cmd_delete(item_id: str) -> int:
    print(request("DELETE", f"/api/queue/{item_id}")["message"])
    return 0


def cmd_clear() -> int:
    print(request("DELETE", "/api/queue")["message"])
    return 0


def cmd_cleanup() -> int:
    removed = request("POST", "/api/cleanup")["removed"]
    print(f"removed {removed} leftover file(s)")
    return 0


def cmd_logs(lines: int) -> int:
    print(request("GET", f"/api/logs?lines={lines}")["logs"], end="")
    return 0


def cmd_serve() -> int:
    # The daemon role: run the FastAPI server in-process.
    import uvicorn

    uvicorn.run("server.main:app", host="127.0.0.1", port=8000)
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(prog="bulker", description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="cmd", required=True)

    check = sub.add_parser("check", help="verify Premium 256kbps Opus availability for a URL")
    check.add_argument("url")
    check.add_argument("--json", action="store_true", help="print raw JSON only")

    add = sub.add_parser("add", help="add URL(s) to the server's queue")
    add.add_argument("urls", nargs="+")

    sub.add_parser("status", help="show the queue")
    retry = sub.add_parser("retry", help="retry a failed item by id")
    retry.add_argument("item_id")
    delete = sub.add_parser("delete", help="remove an item by id")
    delete.add_argument("item_id")
    sub.add_parser("clear", help="remove finished items")
    sub.add_parser("cleanup", help="clean the temp workspace")
    logs = sub.add_parser("logs", help="tail the server log")
    logs.add_argument("--lines", type=int, default=50)
    sub.add_parser("serve", help="start the server (daemon)")

    args = parser.parse_args()
    if args.cmd == "check":
        if args.json:
            from core.downloader import inspect_opus_quality
            print(json.dumps(inspect_opus_quality(args.url), indent=2))
            return 0
        return cmd_check(args.url)
    if args.cmd == "add":
        return cmd_add(args.urls)
    if args.cmd == "status":
        return cmd_status()
    if args.cmd == "retry":
        return cmd_retry(args.item_id)
    if args.cmd == "delete":
        return cmd_delete(args.item_id)
    if args.cmd == "clear":
        return cmd_clear()
    if args.cmd == "cleanup":
        return cmd_cleanup()
    if args.cmd == "logs":
        return cmd_logs(args.lines)
    if args.cmd == "serve":
        return cmd_serve()
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
