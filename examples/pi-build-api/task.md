# Task

Build a dependency-free Python HTTP API.

Requirements:

- Create `app.py` using only the Python standard library.
- `GET /health` must return JSON `{"status": "ok"}` with HTTP 200.
- Keep the request handler testable without starting a permanent server.
- Create `test_app.py` with meaningful `unittest` coverage.
- Run `python3 -m unittest -v` before finishing.

