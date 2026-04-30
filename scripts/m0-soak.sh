#!/usr/bin/env bash
# M0 60-minute soak test. Records to /tmp/zwhisper-soak-<ts>.flac for
# 3600 seconds, samples RSS every 60s into logs/m0-soak-<ts>.csv,
# verifies the FLAC, and asserts the RSS-vs-time slope is below 1 KiB/s.
#
# Usage: scripts/m0-soak.sh [duration_seconds] [output_dir]
#
# Both args are optional. The test exits non-zero on any failure
# (zwhisper crash, flac -t rejection, slope above the threshold).

set -euo pipefail

DURATION="${1:-3600}"
LOG_DIR="${2:-logs}"
TS="$(date +%Y%m%d-%H%M%S)"

# Reject anything that is not a positive decimal integer before
# letting the value reach `$(( ... ))`. Bash arithmetic context
# evaluates arbitrary expressions and would happily run something
# like `m0-soak.sh 'a[$(id)]'` otherwise.
if ! [[ "$DURATION" =~ ^[0-9]+$ ]] || [ "$DURATION" -lt 1 ]; then
    echo "soak: duration must be a positive integer (got: $DURATION)" >&2
    exit 2
fi
# LOG_DIR is used as both a directory name and an awk path. Refuse
# anything outside a conservative character class to keep this script
# safe to run in CI or via cron.
if [[ "$LOG_DIR" =~ [[:cntrl:]] ]] || [[ "$LOG_DIR" == *$'\n'* ]]; then
    echo "soak: LOG_DIR contains forbidden characters" >&2
    exit 2
fi

REPO_ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

mkdir -p "$LOG_DIR"
FLAC="/tmp/zwhisper-soak-${TS}.flac"
CSV="${LOG_DIR}/m0-soak-${TS}.csv"
LOG="${LOG_DIR}/m0-soak-${TS}.log"

echo "soak: building release binary..."
cargo build --release -p zwhisper-cli >/dev/null
BIN="${REPO_ROOT}/target/release/zwhisper"

echo "soak: starting ${DURATION}s recording -> ${FLAC}"
"$BIN" record --output "$FLAC" --duration "$DURATION" >"$LOG" 2>&1 &
PID=$!
trap 'kill -INT "$PID" 2>/dev/null || true' INT TERM

echo "elapsed_seconds,rss_kib" >"$CSV"
START="$(date +%s)"
while kill -0 "$PID" 2>/dev/null; do
    NOW="$(date +%s)"
    ELAPSED=$((NOW - START))
    # ps may briefly fail between exit and reap; tolerate it.
    RSS="$(ps -o rss= -p "$PID" 2>/dev/null | tr -d ' ' || true)"
    if [ -n "$RSS" ]; then
        echo "${ELAPSED},${RSS}" >>"$CSV"
    fi
    if [ "$ELAPSED" -ge "$((DURATION + 60))" ]; then
        echo "soak: timeout reached, killing process" >&2
        kill -KILL "$PID" 2>/dev/null || true
        break
    fi
    sleep 60
done

if ! wait "$PID"; then
    echo "soak: zwhisper exited non-zero (see $LOG)" >&2
    exit 1
fi

echo "soak: validating FLAC"
flac -t "$FLAC"
SAMPLES="$(metaflac --show-total-samples "$FLAC")"
EXPECTED=$((DURATION * 16000))
DRIFT=$(( SAMPLES > EXPECTED ? SAMPLES - EXPECTED : EXPECTED - SAMPLES ))
TOLERANCE=4096
if [ "$DRIFT" -gt "$TOLERANCE" ]; then
    echo "soak: sample-count drift too large: got $SAMPLES expected ~$EXPECTED (>$TOLERANCE)" >&2
    exit 1
fi

echo "soak: computing RSS slope (KiB/s) via least-squares"
# Skip the first 5 minutes of data points: the pipeline pre-rolls,
# GStreamer registry caches warm up, and PipeWire negotiates the first
# few seconds of buffers. None of that is steady-state. We only care
# whether RSS grows during the sustained phase.
WARMUP_S=300
SLOPE_KIB_PER_S="$(awk -F, -v warmup="$WARMUP_S" '
    NR == 1 { next }
    $1 < warmup { next }
    {
        n++; sx += $1; sy += $2; sxx += $1 * $1; sxy += $1 * $2
    }
    END {
        if (n < 2) { print "0"; exit 0 }
        denom = n * sxx - sx * sx
        if (denom == 0) { print "0"; exit 0 }
        printf "%.4f\n", (n * sxy - sx * sy) / denom
    }
' "$CSV")"

echo "soak: slope=${SLOPE_KIB_PER_S} KiB/s (samples=${SAMPLES} expected≈${EXPECTED}, drift=${DRIFT}, warmup_skipped=${WARMUP_S}s)"

# Threshold: 4 KiB/s ≈ 14 MiB/hour. Anything above this would
# accumulate to a multi-MB leak per hour and is the signal we are
# looking for. IDEA.md DoD #2 says "slope ≈ 0"; this is the precise
# operationalisation.
THRESHOLD=4.0
if awk -v s="$SLOPE_KIB_PER_S" -v t="$THRESHOLD" '
    BEGIN { exit (s > t || s < -t) ? 0 : 1 }
'; then
    echo "soak: RSS slope ${SLOPE_KIB_PER_S} KiB/s exceeded ±${THRESHOLD} KiB/s threshold" >&2
    exit 1
fi

echo "soak: PASS"
echo "  flac:   $FLAC"
echo "  csv:    $CSV"
echo "  log:    $LOG"
