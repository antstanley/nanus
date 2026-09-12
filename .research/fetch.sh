#!/bin/bash
UA="nanus-research/1.0 (crate version survey)"
OUT=/Volumes/Delorean/code/nanus/.research/data
mkdir -p "$OUT"
for c in "$@"; do
  code=$(curl -sS --max-time 25 -A "$UA" -o "$OUT/$c.json" -w "%{http_code}" "https://crates.io/api/v1/crates/$c")
  echo "$c -> $code"
  sleep 0.4
done
