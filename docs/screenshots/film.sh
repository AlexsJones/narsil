#!/bin/bash
# Remake assets/demo.gif: the same scripted session as make.sh, filmed.
#
# The terminal is read a few times a second while the session is walked through, and each
# reading is drawn as a frame. Needs what make.sh needs, and ffmpeg.
#
#   cargo build && docs/screenshots/film.sh
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../.." && pwd)
bin=${NARSIL_BIN:-$repo/target/debug}
python=${PYTHON:-python3}
demo=/tmp/narsil-demo
T=narsil-film COLS=104 ROWS=30 FPS=5

export HOME=$demo/home NARSIL_HOME=$demo/nh NARSIL_RUNTIME_DIR=${XDG_RUNTIME_DIR:-/tmp}/narsil-demo NARSIL_MOCK_SCRIPT=$demo/script.txt
tmux kill-session -t $T 2>/dev/null || true
"$bin/narsil" stop >/dev/null 2>&1 || true
rm -rf /tmp/narsil-demo
mkdir -p $demo/home $demo/frames
cp -r "$here/tideline" $demo/home/tideline
git -C $demo/home/tideline init -q -b main
git -C $demo/home/tideline add -A
git -C $demo/home/tideline -c user.name=demo -c user.email=demo@example.org commit -q -m "tideline 0.3.0"
"$python" "$here/script.py" $demo/script.txt 7 >/dev/null
"$bin/narsil" setup --yes --provider mock --name demo --model scripted >/dev/null
sed -i -e 's/^theme = .*/theme = "gruvbox"/' \
  -e '/^\[permissions\]/,/^\[/ s/^mode = .*/mode = "auto"/' \
  -e '/^\[llmfit\]/,/^\[/ s/^command = .*/command = ""/' $NARSIL_HOME/config/config.toml

keys() { tmux send-keys -t $T "$@"; }
# Typed as a person types, so that it can be read as it goes in.
type_in() { local text=$1 i; for ((i = 0; i < ${#text}; i++)); do tmux send-keys -t $T -l "${text:i:1}"; sleep 0.045; done; sleep 0.4; keys Enter; }
wait_for() { for _ in $(seq 1 $((${2:-30} * 5))); do if tmux capture-pane -p -t $T | grep -qE "$1"; then return 0; fi; sleep 0.2; done; echo "timed out waiting for: $1" >&2; return 1; }

tmux new-session -d -s $T -x $COLS -y $ROWS -c $demo/home/tideline "bash --norc --noprofile"
sleep 0.5
tmux send-keys -t $T -l "export PS1='\$ ' PATH=$bin:/usr/bin:/bin COLORTERM=truecolor TERM=xterm-256color; clear"; keys Enter
sleep 0.3
tmux send-keys -t $T -l "narsil"; keys Enter
wait_for "enter send" 20

# The camera: the terminal as it stands, FPS times a second, until told to stop.
( n=0; while [ ! -e $demo/cut ]; do tmux capture-pane -e -p -t $T > $demo/frames/$(printf '%05d' $n).ansi 2>/dev/null || break; n=$((n + 1)); sleep $(awk "BEGIN{print 1/$FPS}"); done ) &
camera=$!
sleep 1

type_in "add retry with backoff to the client, and cover it with tests"
wait_for "2 subagents working" 30
sleep 1.2
type_in "/files"                      # what it has made, changed and read
wait_for "● ready" 60
sleep 2.5
keys Escape
type_in "/trace"; sleep 1.5           # every step, and the whole of the one under the cursor
for _ in 1 2 3 4 5; do keys Down; sleep 0.45; done
sleep 2.2; keys Escape; sleep 0.6
type_in "/usage"; sleep 3.2; keys Escape; sleep 0.6   # what it used
type_in "/theme"; sleep 1                              # and a change of colours
for _ in 1 2 3 4 5; do keys Down; sleep 0.75; done
for _ in 1 2 3 4 5; do keys Up; sleep 0.12; done
sleep 0.6; keys Enter; sleep 1.8

touch $demo/cut; wait $camera 2>/dev/null || true
tmux kill-session -t $T
"$bin/narsil" stop >/dev/null 2>&1 || true

# Each reading drawn as a frame, then the frames put together with one palette for them all.
n=0
for f in $demo/frames/*.ansi; do
  "$python" "$here/render.py" "${f%.ansi}.png" --cols $COLS --rows $ROWS --size 16 --pad 14 --square < "$f" >/dev/null &
  n=$((n + 1)); if [ $((n % 8)) -eq 0 ]; then wait; fi
done
wait
ffmpeg -loglevel error -y -framerate $FPS -pattern_type glob -i "$demo/frames/*.png" \
  -vf "split[a][b];[a]palettegen=max_colors=96:stats_mode=diff[p];[b][p]paletteuse=dither=none:diff_mode=rectangle" \
  -loop 0 "$repo/assets/demo.gif"
echo "$n frames, $(du -h "$repo/assets/demo.gif" | cut -f1): $repo/assets/demo.gif"
