#!/bin/sh
# Break the demo stack on cue: ./incident.sh freeze   (no argument lists them)
set -e
if [ -z "$1" ]; then
  curl -s http://127.0.0.1:8080/control | python3 -c 'import json,sys; [print(f"{k:18} {v}") for k,v in json.load(sys.stdin)["incidents"].items()]'
  exit 0
fi
curl -s -X POST "http://127.0.0.1:8080/control/$1" && echo
