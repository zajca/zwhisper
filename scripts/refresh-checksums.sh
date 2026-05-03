#!/usr/bin/env bash
#
# Refresh the SHA-256 entries in
# `crates/zwhisper-settings/checksums.toml` against the upstream
# HuggingFace `ggerganov/whisper.cpp` model bucket.
#
# Exit codes:
#   0 — every entry matched the upstream byte stream (no drift)
#   1 — at least one entry drifted (upstream re-encoded; manifest
#       must be regenerated before the next release; details on
#       stderr)
#   2 — environment error (curl missing, manifest absent, etc.)
#
# Documented in docs/RELEASE.md step 4 and docs/M8-plan.md DoD #17.

set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "$SCRIPT_DIR/.." && pwd)"
MANIFEST="$REPO_ROOT/crates/zwhisper-settings/checksums.toml"

# Optional: callers (and the zwhisper-settings test harness) can
# override the upstream base URL with ZWHISPER_REFRESH_BASE_URL so
# the integration tests can point this script at a wiremock
# fixture without hitting the network.
DEFAULT_BASE_URL="https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-{model}.bin"
BASE_URL="${ZWHISPER_REFRESH_BASE_URL:-$DEFAULT_BASE_URL}"

if [[ ! -f "$MANIFEST" ]]; then
    echo "error: manifest not found at $MANIFEST" >&2
    exit 2
fi
if ! command -v curl >/dev/null 2>&1; then
    echo "error: curl not on PATH" >&2
    exit 2
fi
if ! command -v sha256sum >/dev/null 2>&1; then
    echo "error: sha256sum not on PATH" >&2
    exit 2
fi

WORKDIR="$(mktemp -d)"
trap 'rm -rf "$WORKDIR"' EXIT

# Extract every `[<name>] ... sha256 = "..." ... size_bytes = ...`
# triple from the manifest. The format is the M7 layout — a flat
# table per model, no nested tables, no quoted whitespace.
parse_manifest() {
    awk '
        function flush() {
            if (name != "" && sha != "" && size != "") {
                print name, sha, size
            }
            name = ""; sha = ""; size = ""
        }
        /^[[:space:]]*#/ { next }
        /^\[[^]]+\]/ {
            flush()
            name = gensub(/^\[([^\]]+)\].*$/, "\\1", "g", $0)
            next
        }
        /^[[:space:]]*sha256[[:space:]]*=/ {
            sha = gensub(/^[^"]*"([^"]+)".*$/, "\\1", "g", $0)
        }
        /^[[:space:]]*size_bytes[[:space:]]*=/ {
            size = gensub(/^[^=]+=[[:space:]]*([0-9]+).*$/, "\\1", "g", $0)
        }
        END { flush() }
    ' "$MANIFEST"
}

drift=0
total=0
while read -r model expected_sha expected_size; do
    [[ -z "${model:-}" ]] && continue
    total=$((total + 1))
    url="${BASE_URL//\{model\}/$model}"
    out="$WORKDIR/$model.bin"

    echo ">> $model (expected sha256: $expected_sha, size: $expected_size)" >&2
    if ! curl --fail --location --silent --show-error \
                 --output "$out" "$url"; then
        echo "    download failed for $url" >&2
        drift=$((drift + 1))
        continue
    fi

    got_size="$(stat -c %s "$out")"
    if [[ "$got_size" != "$expected_size" ]]; then
        echo "    size drift: got $got_size, expected $expected_size" >&2
        drift=$((drift + 1))
        continue
    fi

    got_sha="$(sha256sum "$out" | awk '{print $1}')"
    if [[ "$got_sha" != "$expected_sha" ]]; then
        echo "    sha256 drift: got $got_sha, expected $expected_sha" >&2
        drift=$((drift + 1))
        continue
    fi

    echo "    ok" >&2
done < <(parse_manifest)

if [[ "$total" -eq 0 ]]; then
    echo "error: parsed zero model entries from $MANIFEST" >&2
    exit 2
fi

if [[ "$drift" -gt 0 ]]; then
    echo "$drift of $total entries drifted; manifest must be regenerated" >&2
    exit 1
fi

echo "all $total ggml entries match the upstream manifest" >&2
exit 0
