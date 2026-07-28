#!/usr/bin/env python3
"""End-to-end test for the sandbox master's bounded HTTP wait queue.

For a quick test, start the master with a small queue:

    cargo run -- serve \
      --root /tmp/pi-sandbox-queue-test \
      --slots 1 \
      --max-waiting 3 \
      --socket /tmp/pi-sandbox-queue-test/master.sock \
      --disable-proxy \
      --http 127.0.0.1:8080 \
      --isolation user

Then run:

    python3 scripts/test_wait_queue.py --slots 1 --max-waiting 3

The default 18-second slot hold ensures a queued connection receives the
server's 15-second heartbeat. Use --no-require-heartbeat and a shorter
--hold-seconds value for a faster queue-only test. Every API response line is
printed by default with the run ID that owns its HTTP stream.
"""

from __future__ import annotations

import argparse
import concurrent.futures
import http.client
import json
import math
import os
import ssl
import sys
import threading
import time
import urllib.parse
from dataclasses import dataclass, field
from typing import Any, Callable


JsonObject = dict[str, Any]
PRINT_LOCK = threading.Lock()


def print_line(line: str) -> None:
    with PRINT_LOCK:
        print(line, flush=True)


@dataclass
class RunRecord:
    run_id: str
    messages: list[tuple[float, JsonObject]] = field(default_factory=list)


class EventTracker:
    def __init__(self, show_api_responses: bool) -> None:
        self._condition = threading.Condition()
        self._records: dict[str, RunRecord] = {}
        self._show_api_responses = show_api_responses

    def publish(
        self,
        run_id: str,
        message: JsonObject,
        raw_response: str | None = None,
    ) -> None:
        with self._condition:
            record = self._records.setdefault(run_id, RunRecord(run_id))
            record.messages.append((time.monotonic(), message))
            if self._show_api_responses and raw_response is not None:
                print_line(f"api response [{run_id}] {raw_response}")
            self._condition.notify_all()

    def wait_for(
        self,
        run_id: str,
        predicate: Callable[[JsonObject], bool],
        timeout: float,
        description: str,
    ) -> tuple[float, JsonObject]:
        deadline = time.monotonic() + timeout
        with self._condition:
            while True:
                record = self._records.get(run_id)
                if record:
                    for timestamp, message in record.messages:
                        if predicate(message):
                            return timestamp, message

                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    observed = record.messages if record else []
                    raise AssertionError(
                        f"{run_id}: timed out waiting for {description}; "
                        f"observed={observed!r}"
                    )
                self._condition.wait(remaining)

    def messages(self, run_id: str) -> list[tuple[float, JsonObject]]:
        with self._condition:
            record = self._records.get(run_id)
            return list(record.messages) if record else []


@dataclass(frozen=True)
class HttpTarget:
    scheme: str
    host: str
    port: int
    run_path: str
    health_path: str


def parse_target(base_url: str) -> HttpTarget:
    parsed = urllib.parse.urlsplit(base_url)
    if parsed.scheme not in {"http", "https"}:
        raise ValueError("--url must use http:// or https://")
    if not parsed.hostname:
        raise ValueError("--url must include a hostname")

    default_port = 443 if parsed.scheme == "https" else 80
    base_path = parsed.path.rstrip("/")
    return HttpTarget(
        scheme=parsed.scheme,
        host=parsed.hostname,
        port=parsed.port or default_port,
        run_path=f"{base_path}/api/run",
        health_path=f"{base_path}/api/health",
    )


def connection(target: HttpTarget, timeout: float) -> http.client.HTTPConnection:
    if target.scheme == "https":
        return http.client.HTTPSConnection(
            target.host,
            target.port,
            timeout=timeout,
            context=ssl.create_default_context(),
        )
    return http.client.HTTPConnection(target.host, target.port, timeout=timeout)


def headers(token: str | None) -> dict[str, str]:
    result = {
        "Accept": "application/x-ndjson",
        "Content-Type": "application/json",
    }
    if token:
        result["Authorization"] = f"Bearer {token}"
    return result


def check_health(
    target: HttpTarget,
    token: str | None,
    timeout: float,
    show_api_responses: bool,
) -> None:
    conn = connection(target, timeout)
    try:
        conn.request("GET", target.health_path, headers=headers(token))
        response = conn.getresponse()
        body = response.read()
        if show_api_responses:
            print_line(
                f"api response [health] "
                f"{body.decode('utf-8', errors='replace').strip()}"
            )
        if response.status != 200:
            raise RuntimeError(
                f"health check returned HTTP {response.status}: "
                f"{body.decode('utf-8', errors='replace')}"
            )
        payload = json.loads(body)
        if payload != {"status": "ok"}:
            raise RuntimeError(f"unexpected health response: {payload!r}")
    finally:
        conn.close()


def run_request(
    target: HttpTarget,
    token: str | None,
    tracker: EventTracker,
    run_id: str,
    run_seconds: float,
    socket_timeout: float,
) -> JsonObject:
    request = {
        "run_id": run_id,
        "command": [
            "sh",
            "-c",
            f"sleep {run_seconds:.3f}; printf '%s\\n' \"$PI_RUN_ID\"",
        ],
        "env": [["PI_RUN_ID", run_id]],
        "timeout_ms": max(30_000, math.ceil((run_seconds + 10) * 1000)),
        "result_policy": {"collect_paths": [], "result_file": None},
    }
    body = json.dumps(request).encode("utf-8")
    conn = connection(target, socket_timeout)

    try:
        conn.request(
            "POST",
            target.run_path,
            body=body,
            headers={**headers(token), "Content-Length": str(len(body))},
        )
        response = conn.getresponse()
        if response.status != 200:
            response_body = response.read().decode("utf-8", errors="replace")
            raise RuntimeError(
                f"{run_id}: HTTP {response.status}: {response_body}"
            )

        while True:
            raw_line = response.readline()
            if not raw_line:
                raise RuntimeError(f"{run_id}: stream ended without result or error")
            try:
                raw_response = raw_line.decode("utf-8", errors="replace").rstrip(
                    "\r\n"
                )
                message = json.loads(raw_response)
            except json.JSONDecodeError as error:
                raise RuntimeError(
                    f"{run_id}: invalid NDJSON line: {raw_line!r}"
                ) from error
            tracker.publish(run_id, message, raw_response)
            if message.get("type") in {"result", "error"}:
                return message
    except Exception as error:
        tracker.publish(
            run_id,
            {"type": "client_error", "message": f"{type(error).__name__}: {error}"},
        )
        raise
    finally:
        conn.close()


def is_event(message: JsonObject, event_type: str) -> bool:
    return (
        message.get("type") == "event"
        and isinstance(message.get("event"), dict)
        and message["event"].get("type") == event_type
    )


def print_message(prefix: str, message: JsonObject) -> None:
    print_line(f"{prefix:<18} {json.dumps(message, sort_keys=True)}")


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--url", default="http://127.0.0.1:8080")
    parser.add_argument(
        "--token",
        default=os.environ.get("PI_SANDBOX_HTTP_TOKEN"),
        help=(
            "Bearer token (default: PI_SANDBOX_HTTP_TOKEN); omit when the "
            "HTTP API has no token configured"
        ),
    )
    parser.add_argument(
        "--slots",
        type=int,
        default=1,
        help="Must match the master's --slots value (default: 1)",
    )
    parser.add_argument(
        "--max-waiting",
        type=int,
        default=3,
        help="Must match the master's --max-waiting value (default: 3)",
    )
    parser.add_argument(
        "--hold-seconds",
        type=float,
        default=18.0,
        help="How long active runs hold every slot (default: 18)",
    )
    parser.add_argument(
        "--queued-run-seconds",
        type=float,
        default=0.2,
        help="Runtime for each queued request after allocation (default: 0.2)",
    )
    parser.add_argument(
        "--event-timeout",
        type=float,
        default=30.0,
        help="Timeout for expected queue/start events (default: 30)",
    )
    parser.add_argument(
        "--socket-timeout",
        type=float,
        default=30.0,
        help="Per-read socket timeout; keep above heartbeat interval (default: 30)",
    )
    parser.add_argument(
        "--no-require-heartbeat",
        action="store_true",
        help="Do not require a heartbeat while a request is queued",
    )
    parser.add_argument(
        "--no-api-responses",
        action="store_false",
        dest="show_api_responses",
        help="Hide raw API response lines and show only the verification summary",
    )
    parser.set_defaults(show_api_responses=True)
    args = parser.parse_args()
    if args.slots < 1:
        parser.error("--slots must be at least 1")
    if args.max_waiting < 1:
        parser.error("--max-waiting must be at least 1 for this test")
    if args.hold_seconds <= 0 or args.queued_run_seconds < 0:
        parser.error("run durations must be positive")
    if args.socket_timeout <= 15 and not args.no_require_heartbeat:
        parser.error("--socket-timeout must exceed the 15-second heartbeat interval")
    if args.hold_seconds <= 15 and not args.no_require_heartbeat:
        parser.error(
            "--hold-seconds must exceed 15 when heartbeat verification is enabled"
        )
    return args


def main() -> int:
    args = parse_args()
    target = parse_target(args.url)
    tracker = EventTracker(args.show_api_responses)
    unique = f"{int(time.time())}-{time.monotonic_ns() % 1_000_000}"
    active_ids = [f"queue-test-{unique}-active-{index:03d}" for index in range(1, args.slots + 1)]
    waiting_ids = [
        f"queue-test-{unique}-waiting-{index:03d}"
        for index in range(1, args.max_waiting + 1)
    ]
    overflow_id = f"queue-test-{unique}-overflow"
    futures: dict[str, concurrent.futures.Future[JsonObject]] = {}
    accepted_ids = active_ids + waiting_ids

    print_line(f"Checking {args.url} ...")
    check_health(
        target,
        args.token,
        args.socket_timeout,
        args.show_api_responses,
    )
    print_line(
        f"Testing {args.slots} active + {args.max_waiting} queued + 1 "
        "overflow request"
    )

    worker_count = len(accepted_ids) + 1
    with concurrent.futures.ThreadPoolExecutor(max_workers=worker_count) as pool:
        for run_id in active_ids:
            futures[run_id] = pool.submit(
                run_request,
                target,
                args.token,
                tracker,
                run_id,
                args.hold_seconds,
                args.socket_timeout,
            )
            _, message = tracker.wait_for(
                run_id,
                lambda item: is_event(item, "started"),
                args.event_timeout,
                "started event",
            )
            print_message("slot occupied", message)

        for expected_position, run_id in enumerate(waiting_ids, start=1):
            futures[run_id] = pool.submit(
                run_request,
                target,
                args.token,
                tracker,
                run_id,
                args.queued_run_seconds,
                args.socket_timeout,
            )
            _, message = tracker.wait_for(
                run_id,
                lambda item: is_event(item, "waiting_for_slot"),
                args.event_timeout,
                "waiting_for_slot event",
            )
            event = message["event"]
            assert event["position"] == expected_position, (
                f"{run_id}: expected queue position {expected_position}, "
                f"got {event['position']}"
            )
            assert event["max_waiting"] == args.max_waiting, (
                f"{run_id}: server reports max_waiting={event['max_waiting']}; "
                f"the script was configured for {args.max_waiting}"
            )
            print_message("queued", message)

        futures[overflow_id] = pool.submit(
            run_request,
            target,
            args.token,
            tracker,
            overflow_id,
            args.queued_run_seconds,
            args.socket_timeout,
        )
        _, overflow = tracker.wait_for(
            overflow_id,
            lambda item: item.get("type") in {"error", "client_error"},
            args.event_timeout,
            "queue-full error",
        )
        assert overflow["type"] == "error", f"unexpected overflow response: {overflow!r}"
        assert "sandbox wait queue is full" in overflow.get("message", ""), overflow
        print_message("overflow rejected", overflow)

        if not args.no_require_heartbeat:
            heartbeat_run_id = waiting_ids[0]
            _, heartbeat = tracker.wait_for(
                heartbeat_run_id,
                lambda item: item.get("type") == "heartbeat",
                args.hold_seconds + args.event_timeout,
                "heartbeat while queued",
            )
            observed_messages = [
                message for _, message in tracker.messages(heartbeat_run_id)
            ]
            heartbeat_index = next(
                index
                for index, message in enumerate(observed_messages)
                if message.get("type") == "heartbeat"
            )
            started_index = next(
                (
                    index
                    for index, message in enumerate(observed_messages)
                    if is_event(message, "started")
                ),
                None,
            )
            assert started_index is None or heartbeat_index < started_index, (
                f"{heartbeat_run_id}: first heartbeat arrived only after the "
                "request had left the queue; increase --hold-seconds"
            )
            print_message("queue heartbeat", heartbeat)

        completion_timeout = (
            args.hold_seconds
            + math.ceil(args.max_waiting / args.slots) * args.queued_run_seconds
            + args.event_timeout
        )
        for run_id in accepted_ids:
            final = futures[run_id].result(timeout=completion_timeout)
            assert final["type"] == "result", f"{run_id}: unexpected final line {final!r}"
            print_message("completed", {"run_id": run_id, "type": final["type"]})

        overflow_final = futures[overflow_id].result(timeout=args.event_timeout)
        assert overflow_final["type"] == "error"

    if args.slots == 1:
        started_order = sorted(
            waiting_ids,
            key=lambda run_id: tracker.wait_for(
                run_id,
                lambda item: is_event(item, "started"),
                0,
                "started event",
            )[0],
        )
        assert started_order == waiting_ids, (
            f"queued runs did not start in FIFO order: {started_order!r}"
        )
        print_line("FIFO start order verified with one slot.")

    print_line("PASS: wait queue, limit, heartbeat, and completion behavior are correct.")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (AssertionError, RuntimeError, OSError) as error:
        print(f"FAIL: {error}", file=sys.stderr, flush=True)
        raise SystemExit(1) from error
