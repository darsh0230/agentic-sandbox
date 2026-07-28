#!/usr/bin/env python3
"""End-to-end test for HTTP client-disconnect run handling.

Start a master with exactly one slot:

    cargo run -- serve \
      --root /tmp/agentic-sandbox-disconnect-test \
      --slots 1 \
      --socket /tmp/agentic-sandbox-disconnect-test/master.sock \
      --disable-proxy \
      --http 127.0.0.1:8080 \
      --isolation user

Then run:

    python3 scripts/test_disconnect_behavior.py

The script verifies two cases:

1. Omitting ``kill_on_disconnect`` uses the default value (true). After the
   client drops the response stream, a probe run must receive the only slot
   well before the disconnected command could finish naturally.
2. Setting ``kill_on_disconnect`` to false lets the command continue. A probe
   must wait for the only slot and start only after that command finishes.

The target master must be otherwise idle and configured with exactly one slot.
Only Python's standard library is required.
"""

from __future__ import annotations

import argparse
import http.client
import json
import math
import os
import socket
import ssl
import sys
import time
import urllib.parse
from dataclasses import dataclass
from typing import Any


JsonObject = dict[str, Any]


@dataclass(frozen=True)
class HttpTarget:
    scheme: str
    host: str
    port: int
    run_path: str
    health_path: str


@dataclass(frozen=True)
class ObservedLine:
    timestamp: float
    message: JsonObject


def print_line(message: str) -> None:
    print(message, flush=True)


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


def request_headers(token: str | None) -> dict[str, str]:
    result = {
        "Accept": "application/x-ndjson",
        "Content-Type": "application/json",
    }
    if token:
        result["Authorization"] = f"Bearer {token}"
    return result


def check_health(target: HttpTarget, token: str | None, timeout: float) -> None:
    conn = connection(target, timeout)
    try:
        conn.request("GET", target.health_path, headers=request_headers(token))
        response = conn.getresponse()
        body = response.read()
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


def build_run_request(
    run_id: str,
    run_seconds: float,
    kill_on_disconnect: bool | None,
) -> JsonObject:
    request: JsonObject = {
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
    if kill_on_disconnect is not None:
        request["kill_on_disconnect"] = kill_on_disconnect
    return request


def open_run(
    target: HttpTarget,
    token: str | None,
    request: JsonObject,
    timeout: float,
) -> tuple[http.client.HTTPConnection, http.client.HTTPResponse]:
    body = json.dumps(request).encode("utf-8")
    conn = connection(target, timeout)
    try:
        conn.request(
            "POST",
            target.run_path,
            body=body,
            headers={
                **request_headers(token),
                "Content-Length": str(len(body)),
            },
        )
        response = conn.getresponse()
    except BaseException:
        conn.close()
        raise
    if response.status != 200:
        response_body = response.read().decode("utf-8", errors="replace")
        conn.close()
        raise RuntimeError(
            f"{request['run_id']}: HTTP {response.status}: {response_body}"
        )
    return conn, response


def read_json_line(
    run_id: str,
    response: http.client.HTTPResponse,
    show_api_responses: bool,
) -> ObservedLine:
    raw_line = response.readline()
    timestamp = time.monotonic()
    if not raw_line:
        raise RuntimeError(f"{run_id}: response stream ended unexpectedly")

    decoded = raw_line.decode("utf-8", errors="replace").rstrip("\r\n")
    try:
        message = json.loads(decoded)
    except json.JSONDecodeError as error:
        raise RuntimeError(f"{run_id}: invalid NDJSON line: {decoded!r}") from error
    if not isinstance(message, dict):
        raise RuntimeError(f"{run_id}: expected a JSON object, got {message!r}")
    if show_api_responses:
        print_line(f"api response [{run_id}] {decoded}")
    return ObservedLine(timestamp=timestamp, message=message)


def is_event(message: JsonObject, event_type: str) -> bool:
    event = message.get("event")
    return (
        message.get("type") == "event"
        and isinstance(event, dict)
        and event.get("type") == event_type
    )


def force_disconnect(
    conn: http.client.HTTPConnection,
    response: http.client.HTTPResponse,
) -> None:
    active_socket = conn.sock
    if active_socket is not None:
        try:
            active_socket.shutdown(socket.SHUT_RDWR)
        except OSError:
            pass
    response.close()
    conn.close()


def start_then_disconnect(
    target: HttpTarget,
    token: str | None,
    request: JsonObject,
    timeout: float,
    show_api_responses: bool,
) -> tuple[float, JsonObject]:
    run_id = str(request["run_id"])
    conn, response = open_run(target, token, request, timeout)
    try:
        while True:
            observed = read_json_line(run_id, response, show_api_responses)
            message = observed.message
            if is_event(message, "started"):
                force_disconnect(conn, response)
                return time.monotonic(), message
            if message.get("type") in {"result", "error"}:
                raise AssertionError(
                    f"{run_id}: run ended before disconnect test: {message!r}"
                )
    except BaseException:
        force_disconnect(conn, response)
        raise


def run_to_completion(
    target: HttpTarget,
    token: str | None,
    request: JsonObject,
    timeout: float,
    show_api_responses: bool,
) -> list[ObservedLine]:
    run_id = str(request["run_id"])
    conn, response = open_run(target, token, request, timeout)
    observed_lines: list[ObservedLine] = []
    try:
        while True:
            observed = read_json_line(run_id, response, show_api_responses)
            observed_lines.append(observed)
            message = observed.message
            if message.get("type") == "error":
                raise AssertionError(f"{run_id}: server returned error: {message!r}")
            if message.get("type") == "result":
                result = message.get("result")
                if not isinstance(result, dict):
                    raise AssertionError(f"{run_id}: malformed result: {message!r}")
                if result.get("exit_code") != 0:
                    raise AssertionError(
                        f"{run_id}: expected exit code 0, got {result!r}"
                    )
                return observed_lines
    finally:
        response.close()
        conn.close()


def first_event(
    run_id: str,
    observed_lines: list[ObservedLine],
    event_type: str,
) -> ObservedLine:
    for observed in observed_lines:
        if is_event(observed.message, event_type):
            return observed
    raise AssertionError(
        f"{run_id}: did not receive {event_type!r}; "
        f"observed={[item.message for item in observed_lines]!r}"
    )


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--url", default="http://127.0.0.1:8080")
    parser.add_argument(
        "--token",
        default=os.environ.get("AGENTIC_SANDBOX_HTTP_TOKEN"),
        help=(
            "Bearer token (default: AGENTIC_SANDBOX_HTTP_TOKEN); omit when the "
            "HTTP API has no token configured"
        ),
    )
    parser.add_argument(
        "--kill-run-seconds",
        type=float,
        default=12.0,
        help="Natural runtime of the default-disconnect run (default: 12)",
    )
    parser.add_argument(
        "--continue-run-seconds",
        type=float,
        default=3.0,
        help="Runtime of the kill_on_disconnect=false run (default: 3)",
    )
    parser.add_argument(
        "--kill-release-timeout",
        type=float,
        default=5.0,
        help=(
            "Maximum time for a default-disconnected run to release its slot "
            "(default: 5)"
        ),
    )
    parser.add_argument(
        "--timing-tolerance",
        type=float,
        default=0.5,
        help=(
            "Allowed early-start timing variance for the opt-out test "
            "(default: 0.5)"
        ),
    )
    parser.add_argument(
        "--socket-timeout",
        type=float,
        default=30.0,
        help="Per-read HTTP timeout (default: 30)",
    )
    parser.add_argument(
        "--no-api-responses",
        action="store_false",
        dest="show_api_responses",
        help="Hide raw API response lines and show only the verification summary",
    )
    parser.set_defaults(show_api_responses=True)
    args = parser.parse_args()

    if args.kill_run_seconds <= 0 or args.continue_run_seconds <= 0:
        parser.error("run durations must be positive")
    if args.kill_release_timeout <= 0:
        parser.error("--kill-release-timeout must be positive")
    if args.kill_release_timeout >= args.kill_run_seconds:
        parser.error(
            "--kill-release-timeout must be less than --kill-run-seconds so "
            "the test can distinguish killing from natural completion"
        )
    if args.timing_tolerance < 0:
        parser.error("--timing-tolerance cannot be negative")
    if args.timing_tolerance >= args.continue_run_seconds:
        parser.error(
            "--timing-tolerance must be less than --continue-run-seconds"
        )
    if args.socket_timeout <= args.continue_run_seconds:
        parser.error(
            "--socket-timeout must exceed --continue-run-seconds"
        )
    return args


def main() -> int:
    args = parse_args()
    target = parse_target(args.url)
    unique = f"{int(time.time())}-{time.monotonic_ns() % 1_000_000}"

    print_line(f"Checking {args.url} ...")
    check_health(target, args.token, args.socket_timeout)

    default_run_id = f"disconnect-test-{unique}-default"
    default_probe_id = f"disconnect-test-{unique}-default-probe"
    default_request = build_run_request(
        default_run_id,
        args.kill_run_seconds,
        kill_on_disconnect=None,
    )
    disconnected_at, started = start_then_disconnect(
        target,
        args.token,
        default_request,
        args.socket_timeout,
        args.show_api_responses,
    )
    print_line(
        "Dropped default run after started event "
        f"(pid={started['event'].get('pid')})."
    )
    default_probe = build_run_request(default_probe_id, 0.0, True)
    default_probe_lines = run_to_completion(
        target,
        args.token,
        default_probe,
        args.socket_timeout,
        args.show_api_responses,
    )
    default_probe_started = first_event(
        default_probe_id,
        default_probe_lines,
        "started",
    )
    default_release_elapsed = default_probe_started.timestamp - disconnected_at
    if default_release_elapsed > args.kill_release_timeout:
        raise AssertionError(
            f"default-disconnected run took {default_release_elapsed:.3f}s to "
            f"release the slot; expected at most {args.kill_release_timeout:.3f}s"
        )
    print_line(
        "Default behavior verified: slot released after disconnect in "
        f"{default_release_elapsed:.3f}s."
    )

    continue_run_id = f"disconnect-test-{unique}-continue"
    continue_probe_id = f"disconnect-test-{unique}-continue-probe"
    continue_request = build_run_request(
        continue_run_id,
        args.continue_run_seconds,
        kill_on_disconnect=False,
    )
    continue_disconnected_at, started = start_then_disconnect(
        target,
        args.token,
        continue_request,
        args.socket_timeout,
        args.show_api_responses,
    )
    print_line(
        "Dropped opted-out run after started event "
        f"(pid={started['event'].get('pid')})."
    )
    continue_probe = build_run_request(continue_probe_id, 0.0, True)
    continue_probe_lines = run_to_completion(
        target,
        args.token,
        continue_probe,
        args.socket_timeout,
        args.show_api_responses,
    )
    first_event(continue_probe_id, continue_probe_lines, "waiting_for_slot")
    continue_probe_started = first_event(
        continue_probe_id,
        continue_probe_lines,
        "started",
    )
    continue_elapsed = continue_probe_started.timestamp - continue_disconnected_at
    minimum_expected_wait = args.continue_run_seconds - args.timing_tolerance
    if continue_elapsed < minimum_expected_wait:
        raise AssertionError(
            "kill_on_disconnect=false run released the slot too early: "
            f"probe started after {continue_elapsed:.3f}s; expected at least "
            f"{minimum_expected_wait:.3f}s"
        )
    maximum_expected_wait = (
        args.continue_run_seconds + args.kill_release_timeout
    )
    if continue_elapsed > maximum_expected_wait:
        raise AssertionError(
            "kill_on_disconnect=false run did not release the slot promptly "
            f"after natural completion: probe started after "
            f"{continue_elapsed:.3f}s; expected at most "
            f"{maximum_expected_wait:.3f}s"
        )
    print_line(
        "Opt-out behavior verified: probe remained queued for "
        f"{continue_elapsed:.3f}s while the disconnected run continued."
    )

    print_line(
        "PASS: default disconnect killing and kill_on_disconnect=false "
        "continuation are correct."
    )
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (AssertionError, RuntimeError, OSError, ValueError) as error:
        print(f"FAIL: {error}", file=sys.stderr, flush=True)
        raise SystemExit(1) from error
