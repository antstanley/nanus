#!/bin/bash
# Live check of `read_video` through the real agent, for the models and plans that have stored
# credentials. It spends API credit: one short run per model. Credentials are read by the agent from
# the same store chain a normal run uses and are never printed.
#
# usage: scripts/live-read-video.sh [path-to-nanus] [label provider plan model mode-hint]...
#   With no case arguments, runs the default matrix below.
#
# A clip of four colours, two seconds each, with one to four white squares, is generated with
# ffmpeg, so the expected answer is known: 1040 red 1, 3040 green 2, 5040 blue 3, 7040 yellow 4
# (give or take the frame interval). Each run's agent output is checked against it.
set -u
nanus=${1:-target/debug/nanus}
[ $# -gt 0 ] && shift
root=$(mktemp -d /tmp/nanus-live-video.XXXXXX)
ws=$root/ws; mkdir -p "$ws"

args=(); for c in red green blue yellow; do args+=(-f lavfi -i "color=c=${c}:s=640x360:r=25:d=2"); done
box() { echo -n "drawbox=x=$1:y=140:w=80:h=80:color=white@1:t=fill"; }
ffmpeg -hide_banner -loglevel error -y "${args[@]}" -filter_complex \
  "[0]$(box 280)[a];[1]$(box 190),$(box 370)[b];[2]$(box 100),$(box 280),$(box 460)[c];[3]$(box 10),$(box 190),$(box 370),$(box 550)[d];[a][b][c][d]concat=n=4:v=1:a=0[v]" \
  -map "[v]" -c:v libx264 -pix_fmt yuv420p "$ws/repro.mp4" || exit 2

default_cases() {
  cat <<'LIST'
a-sonnet55 anthropic api claude-sonnet-5-5 default mode
a-opus55 anthropic api claude-opus-5-5 default mode
a-haiku45 anthropic api claude-haiku-4-5-20251001 default mode
a-sonnet55-analyze anthropic api claude-sonnet-5-5 mode analyze
o-luna6 openai api gpt-6-luna default mode
o-astra openai api gpt-6-astra default mode
o-luna6-analyze openai api gpt-6-luna mode analyze
s-luna6 openai subscription gpt-6-luna default mode
s-luna6-analyze openai subscription gpt-6-luna mode analyze
LIST
}
if [ $# -ge 5 ]; then cases=$(echo "$*"); else cases=$(default_cases); fi

pass=0; fail=0
while read -r label provider plan model hint; do
  [ -z "$label" ] && continue
  dir=$root/$label; mkdir -p "$dir/home"
  cat > "$dir/config.toml" <<TOML
provider = "$provider"
plan = "$plan"
model = "$model"
read_video = true
workspace_root = "$ws"
max_steps_per_turn = 6
TOML
  prompt="Use the read_video tool on repro.mp4 ($hint). For each sampled frame give its timestamp, the background colour and how many white squares it shows, as a short list. Do not use any other tool."
  NANUS_HOME="$dir/home" NANUS_CONFIG="$dir/config.toml" "$nanus" --verbose --approval permitted \
    run "$prompt" >"$dir/out.txt" 2>"$dir/err.txt"
  code=$?
  text=$(tr 'A-Z' 'a-z' < "$dir/out.txt")
  ok=1
  [ $code -eq 0 ] || ok=0
  for word in red green blue yellow; do echo "$text" | grep -q "$word" || ok=0; done
  grep -q "read_video finished" "$dir/err.txt" || ok=0
  if [ $ok -eq 1 ]; then pass=$((pass+1)); echo "PASS $label ($provider/$plan/$model)"
  else fail=$((fail+1)); echo "FAIL $label ($provider/$plan/$model) exit=$code; see $dir"; fi
done <<< "$cases"
echo "$pass passed, $fail failed; artifacts in $root"
[ $fail -eq 0 ]
