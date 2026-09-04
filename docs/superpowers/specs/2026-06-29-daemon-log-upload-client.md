# Daemon Log Upload Client

This document describes the Git AI client-side daemon diagnostics upload path.

## Behavior

- The daemon captures tracing events through `DaemonLogUploadLayer`.
- Events are buffered in memory by the daemon telemetry worker and flushed with
  the existing API client/auth headers.
- A heartbeat event is generated roughly every 15 minutes while the daemon is
  running. Besides `uptime_seconds`/`os`/`arch` it carries the daemon health
  snapshot (the same data as the `status.daemon` control request / `git-ai bg
  status`): pending sequencer work, open, finishing and written trace roots,
  causal-fence releases, loss counters, a `sequencer_stalled` flag, and the two
  families with the oldest pending work as `family_<i>_*` fields. Family keys
  (local repository paths) are replaced by a stable `family_<i>_id` digest.
- Upload is best-effort and fire-and-forget. The telemetry worker dispatches at
  most one daemon-log upload at a time on a detached thread and does not await
  endpoint availability.
- If another daemon-log upload is already in flight, the current batch is
  requeued into the bounded in-memory buffer. If the buffer exceeds its cap, the
  oldest events are dropped.
- Events are dropped without retry when upload is disabled, the current API
  auth/configuration does not permit upload, or a dispatched upload fails.
- `feature_flags.daemon_log_upload` disables capture and upload when set to
  `false`. The flag defaults to enabled in debug and release builds.
- Each event keeps at most 64 structured fields. Field names and string values
  are bounded, secret-redacted, and length-limited before buffering.

## Endpoint

The daemon posts to:

```text
POST /worker/logs/upload
```

Authentication uses the same headers as metrics upload:

- API key mode: `X-API-Key`, `X-Author-Identity`, `X-Distinct-ID`
- OAuth mode: `Authorization: Bearer <token>`, `X-Distinct-ID`

## Payload

```json
{
  "version": 1,
  "git_ai_version": "1.6.5",
  "daemon_id": "daemon-run-uuid",
  "install_id": "install-distinct-id",
  "events": [
    {
      "id": "event-uuid",
      "kind": "heartbeat",
      "timestamp": "2026-06-29T12:00:00Z",
      "level": "info",
      "target": "git_ai::daemon",
      "message": "alive",
      "fields": {
        "uptime_seconds": 900,
        "os": "linux",
        "arch": "x86_64",
        "snapshot_partial": false,
        "sequencer_entries_total": 1,
        "sequencer_fenced_families": 1,
        "sequencer_oldest_entry_age_ms": 420,
        "sequencer_stalled": false,
        "trace_roots_open_mutating": 1,
        "causal_grace_expirations": 0,
        "family_0_id": "3f2a9c1d8e07",
        "family_0_front_kind": "checkpoint",
        "family_0_fenced": true
      }
    }
  ]
}
```

## Verification

Run focused tests:

```bash
task test TEST_FILTER=daemon_log
task test TEST_FILTER=telemetry_buffer_caps_daemon_logs_to_latest_events
```

Run normal pre-PR checks:

```bash
task fmt
task lint
task test
```

## Practical Smoke Test

This checks the installed/debug `git-ai` binary against a local mock HTTP
endpoint. It validates the daemon sends real log events to
`/worker/logs/upload` with the expected auth header and payload shape.

```bash
task build

tmp="$(mktemp -d)"
port_file="$tmp/port"
requests_file="$tmp/requests.jsonl"

python3 - "$port_file" "$requests_file" <<'PY' &
import json
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer

port_file, requests_file = sys.argv[1:3]

class Handler(BaseHTTPRequestHandler):
    def do_POST(self):
        length = int(self.headers.get("content-length", "0"))
        payload = json.loads(self.rfile.read(length).decode("utf-8"))
        events = payload.get("events", [])
        with open(requests_file, "a", encoding="utf-8") as f:
            f.write(json.dumps({
                "path": self.path,
                "api_key": self.headers.get("x-api-key"),
                "version": payload.get("version"),
                "daemon_id_present": bool(payload.get("daemon_id")),
                "install_id_present": bool(payload.get("install_id")),
                "event_count": len(events),
                "kinds": [event.get("kind") for event in events],
                "messages": [event.get("message") for event in events],
            }) + "\n")
        body = json.dumps({
            "accepted": len(events),
            "dropped": 0,
            "enqueued": True,
            "errors": [],
        }).encode("utf-8")
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *_):
        pass

server = HTTPServer(("127.0.0.1", 0), Handler)
with open(port_file, "w", encoding="utf-8") as f:
    f.write(str(server.server_port))
server.serve_forever()
PY

server_pid=$!
trap 'target/debug/git-ai bg shutdown >/dev/null 2>&1 || true; kill "$server_pid" >/dev/null 2>&1 || true; rm -rf "$tmp"' EXIT

while [ ! -s "$port_file" ]; do sleep 0.1; done
api_url="http://127.0.0.1:$(cat "$port_file")"

repo="$tmp/repo"
git init -q "$repo"
git -C "$repo" config user.email smoke@example.com
git -C "$repo" config user.name Smoke
printf 'hello\n' > "$repo/example.txt"

export GIT_AI_API_BASE_URL="$api_url"
export GIT_AI_API_KEY="local-smoke-key"
export GIT_AI_DAEMON_HOME="$tmp/daemon"
export GIT_AI_DAEMON_UPDATE_CHECK_INTERVAL=86400
export GIT_AI_DAEMON_MAX_UPTIME_SECS=86400

target/debug/git-ai bg start
target/debug/git-ai checkpoint human "$repo/example.txt"
sleep 4
target/debug/git-ai bg shutdown

python3 - "$requests_file" <<'PY'
import json
import sys

rows = [json.loads(line) for line in open(sys.argv[1], encoding="utf-8")]
print("requests=", len(rows), sep="")
for row in rows:
    print("path=", row["path"], sep="")
    print("api_key=", row["api_key"], sep="")
    print("version=", row["version"], sep="")
    print("daemon_id_present=", row["daemon_id_present"], sep="")
    print("install_id_present=", row["install_id_present"], sep="")
    print("event_count=", row["event_count"], sep="")
    print("kinds=", ",".join(row["kinds"]), sep="")
    print("messages=", ",".join(row["messages"]), sep="")
PY
```

Expected output has one or more daemon log events sent to the upload endpoint:

```text
requests=1
path=/worker/logs/upload
api_key=local-smoke-key
version=1
daemon_id_present=True
install_id_present=True
event_count=6
kinds=log,log,log,log,log,log
messages=transcript worker spawned,socket health check started,transcript worker started,sweep completed,checkpoint start,checkpoint done
```
