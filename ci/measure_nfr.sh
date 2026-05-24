#!/usr/bin/env bash
#
# CI measurement script for the cheap NFRs:
#   NFR-003  startup-to-ready  ≤ 500 ms   (enforced)
#   NFR-004  idle RSS          ≤ 30 MiB   (enforced)
#   NFR-005  binary size       ≤ 15 MiB   (informational only)
#
# Numbers are emitted as GitHub workflow ::notice:: lines so the
# values are visible in the Actions summary even when the job
# passes. Enforced NFRs use ::error:: + non-zero exit on miss.
#
# Run from the repo root after `cargo build --release`.

set -euo pipefail

BIN=target/release/remo-broker
SANDBOX=$(mktemp -d)
trap 'rm -rf "$SANDBOX"' EXIT

mkdir -p "$SANDBOX/run" "$SANDBOX/log"
echo "ci-token" > "$SANDBOX/bootstrap-token"
cat > "$SANDBOX/fnox.toml" <<'EOF'
[providers]
plain = { type = "plain" }

[secrets]
X = { provider = "plain", value = "y" }
EOF

# ---- NFR-005 binary size ---------------------------------------------
SIZE_BYTES=$(stat -c '%s' "$BIN")
SIZE_MIB=$(( SIZE_BYTES / 1024 / 1024 ))
echo "::notice title=NFR-005 binary size::${SIZE_MIB} MiB (${SIZE_BYTES} bytes) — target ≤15 MiB"
if [ "$SIZE_BYTES" -gt $((15 * 1024 * 1024)) ]; then
  echo "::warning title=NFR-005 over target::${SIZE_MIB} MiB exceeds 15 MiB target. Tracked as a known gap (AWS SDK + ctap-hid-fido2 dep chain); not failing the job today."
fi

# ---- NFR-003 startup -------------------------------------------------
T0=$(date +%s%N)
"$BIN" \
  --bootstrap-token-path "$SANDBOX/bootstrap-token" \
  --fnox-config         "$SANDBOX/fnox.toml" \
  --socket-dir          "$SANDBOX/run" \
  --audit-log-path      "$SANDBOX/log/audit.log" \
  >"$SANDBOX/log/stdout" 2>"$SANDBOX/log/stderr" &
PID=$!
trap 'kill "$PID" 2>/dev/null || true; rm -rf "$SANDBOX"' EXIT

READY=0
for _ in $(seq 1 500); do
  if echo '{"op":"status"}' | socat - UNIX-CONNECT:"$SANDBOX/run/admin.sock" 2>/dev/null | grep -q '"ok":true'; then
    READY=1
    break
  fi
  sleep 0.01
done
T1=$(date +%s%N)
if [ "$READY" -ne 1 ]; then
  echo "::error title=NFR-003 daemon never became ready::see $SANDBOX/log/stderr"
  cat "$SANDBOX/log/stderr" || true
  exit 1
fi
STARTUP_MS=$(( (T1 - T0) / 1000000 ))
echo "::notice title=NFR-003 startup::${STARTUP_MS} ms — target ≤500 ms"

# ---- NFR-004 idle RSS ------------------------------------------------
sleep 2
RSS_KB=$(awk '/^VmRSS:/ { print $2 }' "/proc/$PID/status")
RSS_MIB=$(( RSS_KB / 1024 ))
echo "::notice title=NFR-004 idle RSS::${RSS_KB} KiB = ${RSS_MIB} MiB — target ≤30 MiB"

# ---- Clean shutdown for hygiene --------------------------------------
kill -TERM "$PID"
wait "$PID" 2>/dev/null || true

# ---- Enforce ---------------------------------------------------------
FAIL=0
if [ "$STARTUP_MS" -gt 500 ]; then
  echo "::error title=NFR-003 violation::startup ${STARTUP_MS} ms exceeds 500 ms target"
  FAIL=1
fi
if [ "$RSS_KB" -gt $(( 30 * 1024 )) ]; then
  echo "::error title=NFR-004 violation::idle RSS ${RSS_MIB} MiB exceeds 30 MiB target"
  FAIL=1
fi
exit "$FAIL"
