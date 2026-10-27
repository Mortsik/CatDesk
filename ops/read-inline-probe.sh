#!/usr/bin/env bash
# Probe files for measuring where a host (ChatGPT, Codex, ...) starts cutting
# a `read` reply on its own, independently of CatDesk's inline read budget.
#
# Every line carries its own number, so the last line the model reports is
# exactly how far the reply reached it; the final line is an END marker with a
# per-run token the model cannot guess.
#
# Usage:
#   ops/read-inline-probe.sh <dir> [size_kib ...]     (default: 64 128 192 256 384 512)
#
# Then:
#   1. Restart CatDesk with a budget above the largest probe, so the server
#      itself never cuts:  CATDESK_READ_INLINE_BYTES=1048576 catdesk ...
#   2. In a fresh host conversation, for each file ask:
#        "Read <dir>/probe-<N>k.txt with the CatDesk read tool in ONE call.
#         Report verbatim: the last line you can see, whether you see a line
#         starting with END-OF-PROBE (and its token), and files[0].budgetTruncated."
#   3. Compare against the expected last line printed below. The host cut the
#      reply if the reported last line is earlier than expected while
#      budgetTruncated is false.
set -euo pipefail

if [[ $# -lt 1 ]]; then
    sed -n '2,22p' "$0"
    exit 2
fi
dir=$1
shift
sizes=("$@")
[[ ${#sizes[@]} -gt 0 ]] || sizes=(64 128 192 256 384 512)

mkdir -p "$dir"
token=$(od -An -N6 -tx1 /dev/urandom | tr -d ' \n')
printf '%-22s %10s %12s  %s\n' file bytes last-line end-marker
for kib in "${sizes[@]}"; do
    file="$dir/probe-${kib}k.txt"
    # 64-byte lines: "P<kib>k-L<line> " padded with filler to 63 chars + \n.
    awk -v kib="$kib" -v token="$token" 'BEGIN {
        target = kib * 1024
        lines = int((target - 64) / 64)
        for (i = 1; i <= lines; i++) {
            head = sprintf("P%dk-L%06d ", kib, i)
            pad = 63 - length(head)
            printf "%s%s\n", head, substr("abcdefghijklmnopqrstuvwxyz0123456789abcdefghijklmnopqrstuvwxyz", 1, pad)
        }
        printf "END-OF-PROBE %dk %s L%06d\n", kib, token, lines + 1
    }' >"$file"
    last=$(($(wc -l <"$file")))
    printf '%-22s %10s %12s  END-OF-PROBE %sk %s\n' \
        "probe-${kib}k.txt" "$(wc -c <"$file")" "L$(printf '%06d' "$last")" "$kib" "$token"
done
