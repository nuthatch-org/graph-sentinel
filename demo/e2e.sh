#!/bin/sh
# Break the demo stack one incident at a time and time how long graph-sentinel takes to raise
# the matching finding, then to clear it. Run after `docker compose up -d`.
S=http://127.0.0.1:9477/api/snapshot
has() { curl -s $S | python3 -c "import json,sys; d=json.load(sys.stdin); sys.exit(0 if any(f['kind']=='$1' for f in d['findings']) else 1)"; }
wait_for() { # kind want(1 present / 0 absent) timeout
  t0=$(date +%s)
  while :; do
    if has "$1"; then [ "$2" = 1 ] && break; else [ "$2" = 0 ] && break; fi
    [ $(( $(date +%s) - t0 )) -ge "$3" ] && { echo "TIMEOUT"; return 1; }
    sleep 3
  done
  echo "$(( $(date +%s) - t0 ))s"
}
run() { # incident revert kind timeout
  ./incident.sh "$1" >/dev/null
  up=$(wait_for "$3" 1 "$4")
  revert "$2"
  down=$(wait_for "$3" 0 "$4")
  printf "%-16s %-22s raised %-8s cleared %s\n" "$1" "$3" "$up" "$down"
}
# A rotated sender's receipts stay unpaid until the operator configures it, so stopping them is
# not a recovery: adding the sender to indexer-rs's config is. Written in place, since the file
# is bind-mounted and a new inode would not reach the container.
ROTATED='0x4b1d0000000000000000000000000000000c0ffe = "http://sim:8080/aggregator"'
revert() {
  case "$1" in
    configure-sender)
      ./incident.sh rotate-done >/dev/null
      cp indexer-rs.toml /tmp/indexer-rs.toml.e2e && echo "$ROTATED" >> indexer-rs.toml ;;
    *) ./incident.sh "$1" >/dev/null ;;
  esac
}
restore() { [ -f /tmp/indexer-rs.toml.e2e ] && cat /tmp/indexer-rs.toml.e2e > indexer-rs.toml && rm /tmp/indexer-rs.toml.e2e; }
trap restore EXIT
run freeze          unfreeze         frozen               120
run fail            unfail           failed               120
run chain-stall     chain-resume     chain_stalled        150
run rotate-sender   configure-sender unknown_sender       120
run rav-stall       rav-resume       rav_stalled          240
run dispute         dispute-clear    dispute_against      150
run poi-due         poi-ok           poi_due              150
run ineligible      eligible         reo_ineligible       240
run escrow-low      escrow-ok        escrow_low           150
run aggregator-down aggregator-up    aggregator_down      120
./incident.sh clear-denylist >/dev/null
