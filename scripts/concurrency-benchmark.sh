#!/usr/bin/env bash
# Run complete test suites concurrently and sample host/disk/PostgreSQL metrics.
# Override SUITE_COMMAND, DISK_DEVICE, PG_CONNINFO, and SAMPLE_SECONDS as needed.
set -uo pipefail

json=false
if [[ "${1:-}" == "--json" ]]; then json=true; shift; fi
if [[ $# -gt 1 ]]; then echo "usage: $0 [--json] [1|2|4|8]" >&2; exit 2; fi
n="${1:-2}"
case "$n" in 1|2|4|8) ;; *) echo "suite count must be one of 1, 2, 4, 8" >&2; exit 2;; esac
if ! command -v setsid >/dev/null 2>&1; then
  echo "setsid is required to isolate and clean up benchmark suite process groups" >&2
  exit 1
fi
suite_command="${SUITE_COMMAND:-npm test}"
sample_seconds="${SAMPLE_SECONDS:-1}"
if [[ ! "$sample_seconds" =~ ^([0-9]+([.][0-9]*)?|[.][0-9]+)$ ]] ||
   ! LC_ALL=C awk -v interval="$sample_seconds" 'BEGIN { exit !(interval > 0) }'; then
  echo "SAMPLE_SECONDS must be a positive number" >&2
  exit 2
fi
if $json && ! command -v python3 >/dev/null 2>&1; then
  echo "python3 is required for --json output" >&2
  exit 1
fi
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
stop_pg_poller() {
  [[ -z "$pg_poller" ]] && return
  local p="$pg_poller"
  touch "$work/stop-pg"
  for _ in {1..10}; do
    kill -0 -- -"$p" 2>/dev/null || break
    sleep 0.1
  done
  if kill -0 -- -"$p" 2>/dev/null; then
    kill -TERM -- -"$p" 2>/dev/null || true
    for _ in {1..20}; do
      kill -0 -- -"$p" 2>/dev/null || break
      sleep 0.1
    done
  fi
  if kill -0 -- -"$p" 2>/dev/null; then
    kill -KILL -- -"$p" 2>/dev/null || true
    for _ in {1..20}; do
      kill -0 -- -"$p" 2>/dev/null || break
      sleep 0.1
    done
  fi
  if kill -0 -- -"$p" 2>/dev/null; then
    echo "warning: PostgreSQL poller process group $p still has members after SIGKILL" >&2
  fi
  wait "$p" 2>/dev/null || true
  pg_poller=
}
cleanup() {
  trap - EXIT INT TERM
  local p alive=0
  for p in "${pids[@]}"; do
    kill -TERM -- -"$p" 2>/dev/null || true
    kill -0 -- -"$p" 2>/dev/null && alive=1 || true
  done
  if (( alive )); then sleep 2; fi
  for p in "${pids[@]}"; do kill -KILL -- -"$p" 2>/dev/null || true; done
  for _ in {1..20}; do
    alive=0
    for p in "${pids[@]}"; do kill -0 -- -"$p" 2>/dev/null && alive=1 || true; done
    (( alive )) || break
    sleep 0.1
  done
  for p in "${pids[@]}"; do
    if kill -0 -- -"$p" 2>/dev/null; then
      echo "warning: process group $p still has members after SIGKILL" >&2
    fi
  done
  [[ -z "$sampler" ]] || kill "$sampler" 2>/dev/null || true
  stop_pg_poller
  rm -rf "$work"
}
trap cleanup EXIT
trap 'exit 130' INT TERM
start_s=$(date +%s)
for i in $(seq 1 "$n"); do
  status_file="$work/suite-$i.status"
  setsid bash -c 'set +e; bash -lc "$1"; rc=$?; printf "%s\n" "$rc" > "$2"; exit 0' _ "$suite_command" "$status_file" >"$work/suite-$i.log" 2>&1 &
  pids+=("$!")
done

# iostat's first report is cumulative since boot; later reports cover intervals.
if command -v iostat >/dev/null && [[ -n "$device" ]]; then
  iostat -x "$device" "$sample_seconds" >"$work/iostat.txt" 2>"$work/iostat.err" & sampler=$!
else
  printf 'unavailable: iostat missing or disk device could not be identified\n' >"$work/iostat.err"
fi
if [[ -n "${PG_CONNINFO:-}" ]] && command -v psql >/dev/null; then
  setsid bash -c '
    work=$1 sample_seconds=$2 conninfo=$3 connect_timeout=$4
    while [[ ! -e "$work/stop-pg" ]]; do
      if value=$(PGCONNECT_TIMEOUT="$connect_timeout" psql "$conninfo" -Atqc "select count(*) from pg_stat_activity" 2>>"$work/pg.err"); then
        printf "%s\\n" "$value" >>"$work/pg.samples"
      else
        printf "unavailable\\n" >>"$work/pg.samples"
      fi
      sleep "$sample_seconds"
    done
  ' _ "$work" "$sample_seconds" "$PG_CONNINFO" "${PG_CONNECT_TIMEOUT:-2}" & pg_poller=$!
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
end_s=$(date +%s)
[[ -z "$sampler" ]] || { kill "$sampler" 2>/dev/null || true; wait "$sampler" 2>/dev/null || true; }
stop_pg_poller
sampler=

disk_util=unavailable write_iops=unavailable write_latency=unavailable
disk_reason='no complete in-run iostat sample (suite may be shorter than SAMPLE_SECONDS)'
if [[ ! -s "$work/iostat.txt" && -s "$work/iostat.err" ]]; then
  disk_reason=$(cat "$work/iostat.err")
fi
if [[ -s "$work/iostat.txt" ]]; then
  read -r disk_util write_iops write_latency disk_reason < <(awk -v target="$device" '
    /^Device[[:space:]]/ {
      report++
      delete col
      for (i=1; i<=NF; i++) col[$i]=i
      next
    }
    report>=2 && $1==target {
      u=col["%util"]; w=col["w/s"]; a=col["w_await"]
      if (u && w && a) { util_sum += $u; iops_sum += $w; latency_sum += $a; count++ }
    }
    END {
      if (count) printf "%.2f %.2f %.2f sampled\n", util_sum/count, iops_sum/count, latency_sum/count
      else print "unavailable unavailable unavailable no-complete-in-run-sample"
    }
  ' "$work/iostat.txt") || true
fi
pg_connections=unavailable
if [[ -s "$work/pg.samples" ]]; then
  pg_connections=$(awk '$1!="unavailable" { if ($1+0>max) max=$1+0; seen=1 } END { if(seen) print max; else print "unavailable" }' "$work/pg.samples")
fi
pg_errors=$(cat "$work/pg.err" 2>/dev/null || true)
duration_ms=$(( (end_s - start_s) * 1000 ))
if $json; then
  python3 - "$n" "$duration_ms" "$device" "$disk_util" "$write_iops" "$write_latency" "$pg_connections" "$pg_errors" "$disk_reason" "${statuses[@]}" <<'PY'
import json, sys
n, duration, device, util, iops, latency, pg, pg_errors, disk_reason, *codes = sys.argv[1:]
print(json.dumps({"suites": int(n), "duration_ms": int(duration), "disk_device": device or None,
 "disk_util_percent": util, "write_iops": iops, "write_latency_ms": latency,
 "postgres_connections": pg, "postgres_errors": pg_errors, "disk_sample_status": disk_reason,
 "suite_exit_codes": [int(c) if c.isdigit() else c for c in codes]}))
PY
else
  printf 'Concurrency benchmark report\nSuites: %s\nDuration: %s ms\nDisk device: %s\nDisk util: %s%%\nWrite IOPS: %s\nWrite latency: %s ms\nPostgreSQL connections: %s\nPostgreSQL errors: %s\nDisk sample status: %s\nSuite exit codes: %s\n' \
    "$n" "$duration_ms" "${device:-unavailable}" "$disk_util" "$write_iops" "$write_latency" "$pg_connections" "${pg_errors:-none}" "$disk_reason" "${statuses[*]}"
fi
(( failures == 0 ))
