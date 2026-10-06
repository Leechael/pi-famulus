#!/usr/bin/env bash
# Run complete test suites concurrently and sample host/disk/PostgreSQL metrics.
# Override SUITE_COMMAND, DISK_DEVICE, PG_CONNINFO, and SAMPLE_SECONDS as needed.
set -uo pipefail

json=false
if [[ "${1:-}" == "--json" ]]; then json=true; shift; fi
if [[ $# -gt 1 ]]; then echo "usage: $0 [--json] [1|2|4|8]" >&2; exit 2; fi
n="${1:-2}"
case "$n" in 1|2|4|8) ;; *) echo "suite count must be one of 1, 2, 4, 8" >&2; exit 2;; esac
suite_command="${SUITE_COMMAND:-npm test}"
sample_seconds="${SAMPLE_SECONDS:-1}"
device="${DISK_DEVICE:-}"
if [[ -z "$device" ]]; then
  source_device=$(df -P . | awk 'NR==2 {print $1}')
  device=${source_device##*/}
  if [[ "$device" == overlay || "$device" == rootfs || -z "$device" ]]; then
    device=$(lsblk -ndo NAME,TYPE 2>/dev/null | awk '$2=="disk" {print $1; exit}')
  fi
fi
device=${device#/dev/}
work=$(mktemp -d)
pids=() sampler= pg_poller=
cleanup() {
  for p in "${pids[@]}"; do kill -- -"$p" 2>/dev/null || true; done
  [[ -z "$sampler" ]] || kill "$sampler" 2>/dev/null || true
  [[ -z "$pg_poller" ]] || kill "$pg_poller" 2>/dev/null || true
  rm -rf "$work"
}
trap cleanup EXIT INT TERM
start_ns=$(date +%s%N)
for i in $(seq 1 "$n"); do
  status_file="$work/suite-$i.status"
  if command -v setsid >/dev/null; then
    setsid bash -c 'set +e; bash -lc "$1"; rc=$?; printf "%s\\n" "$rc" > "$2"; exit 0' _ "$suite_command" "$status_file" >"$work/suite-$i.log" 2>&1 &
  else
    bash -c 'set +e; bash -lc "$1"; rc=$?; printf "%s\\n" "$rc" > "$2"; exit 0' _ "$suite_command" "$status_file" >"$work/suite-$i.log" 2>&1 &
  fi
  pids+=("$!")
done

# Keep sampling until every suite has ended. iostat emits repeated reports;
# the parser below uses named header columns, not positional assumptions.
if command -v iostat >/dev/null && [[ -n "$device" ]]; then
  iostat -x "$device" "$sample_seconds" >"$work/iostat.txt" 2>"$work/iostat.err" & sampler=$!
else
  printf 'unavailable: iostat missing or disk device could not be identified\n' >"$work/iostat.err"
fi
if [[ -n "${PG_CONNINFO:-}" ]] && command -v psql >/dev/null; then
  (
    while :; do
      if value=$(psql "$PG_CONNINFO" -Atqc 'select count(*) from pg_stat_activity' 2>>"$work/pg.err"); then
        printf '%s\n' "$value" >>"$work/pg.samples"
      else
        printf 'unavailable\n' >>"$work/pg.samples"
      fi
      sleep "$sample_seconds"
    done
  ) & pg_poller=$!
else
  printf 'unavailable: psql or PG_CONNINFO unavailable\n' >"$work/pg.err"
fi

failures=0
statuses=()
for i in "${!pids[@]}"; do
  wait "${pids[$i]}" || true
  status_file="$work/suite-$((i+1)).status"
  code=unknown
  [[ -s "$status_file" ]] && code=$(<"$status_file")
  statuses+=("$code")
  [[ "$code" == 0 ]] || ((failures+=1))
done
end_ns=$(date +%s%N)
[[ -z "$sampler" ]] || { kill "$sampler" 2>/dev/null || true; wait "$sampler" 2>/dev/null || true; }
[[ -z "$pg_poller" ]] || { kill "$pg_poller" 2>/dev/null || true; wait "$pg_poller" 2>/dev/null || true; }
sampler= pg_poller=

disk_util=unavailable write_iops=unavailable write_latency=unavailable
if [[ -s "$work/iostat.txt" ]]; then
  read -r disk_util write_iops write_latency < <(awk -v target="$device" '
    /^Device[[:space:]]/ {
      for (i=1; i<=NF; i++) col[$i]=i
      next
    }
    $1==target {
      u=col["%util"]; w=col["w/s"]; a=col["w_await"]
      if (u && w && a) { util_sum += $u; iops_sum += $w; latency_sum += $a; count++ }
    }
    END {
      if (count) printf "%.2f %.2f %.2f\n", util_sum/count, iops_sum/count, latency_sum/count
      else print "unavailable unavailable unavailable"
    }
  ' "$work/iostat.txt") || true
fi
pg_connections=unavailable
if [[ -s "$work/pg.samples" ]]; then
  pg_connections=$(awk '$1!="unavailable" { if ($1+0>max) max=$1+0; seen=1 } END { if(seen) print max; else print "unavailable" }' "$work/pg.samples")
fi
pg_errors=$(<"$work/pg.err" 2>/dev/null || true)
duration_ms=$(( (end_ns - start_ns) / 1000000 ))
if $json; then
  python3 - "$n" "$duration_ms" "$device" "$disk_util" "$write_iops" "$write_latency" "$pg_connections" "$pg_errors" "${statuses[@]}" <<'PY'
import json, sys
n, duration, device, util, iops, latency, pg, pg_errors, *codes = sys.argv[1:]
print(json.dumps({"suites": int(n), "duration_ms": int(duration), "disk_device": device or None,
 "disk_util_percent": util, "write_iops": iops, "write_latency_ms": latency,
 "postgres_connections": pg, "postgres_errors": pg_errors,
 "suite_exit_codes": [int(c) if c.isdigit() else c for c in codes]}))
PY
else
  printf 'Concurrency benchmark report\nSuites: %s\nDuration: %s ms\nDisk device: %s\nDisk util: %s%%\nWrite IOPS: %s\nWrite latency: %s ms\nPostgreSQL connections: %s\nPostgreSQL errors: %s\nSuite exit codes: %s\n' \
    "$n" "$duration_ms" "${device:-unavailable}" "$disk_util" "$write_iops" "$write_latency" "$pg_connections" "${pg_errors:-none}" "${statuses[*]}"
fi
(( failures == 0 ))
