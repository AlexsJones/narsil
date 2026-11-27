#!/bin/bash
# Remake the screenshots in docs/img.
#
# A scripted session is walked through the TUI in tmux, and each screen is
# drawn to a PNG from what the terminal holds: the same characters in the
# same colours. No model is called: the stand-in model (the `mock` provider)
# follows script.py, working in a copy of the small project in tideline/.
#
# Needs tmux, git, python3 with Pillow (PYTHON names another python), and
# fontconfig. The font is Adwaita Mono where it is installed.
#
#   cargo build && docs/screenshots/make.sh
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../.." && pwd)
bin=${NARSIL_BIN:-$repo/target/debug}
python=${PYTHON:-python3}
out=$repo/docs/img
# A place of its own, with a plain path: paths show in the trace and the log.
demo=/tmp/narsil-demo
T=narsil-shots COLS=118 ROWS=36

export HOME=$demo/home NARSIL_HOME=$demo/nh NARSIL_RUNTIME_DIR=${XDG_RUNTIME_DIR:-/tmp}/narsil-demo NARSIL_MOCK_SCRIPT=$demo/script.txt
tmux kill-session -t $T 2>/dev/null || true
"$bin/narsil" stop >/dev/null 2>&1 || true
rm -rf /tmp/narsil-demo
mkdir -p $demo/home "$out"
cp -r "$here/tideline" $demo/home/tideline
git -C $demo/home/tideline init -q -b main
git -C $demo/home/tideline add -A
git -C $demo/home/tideline -c user.name=demo -c user.email=demo@example.org commit -q -m "tideline 0.3.0"
"$python" "$here/script.py" $demo/script.txt >/dev/null
"$bin/narsil" setup --yes --provider mock --name demo --model scripted >/dev/null
sed -i -e 's/^theme = .*/theme = "gruvbox"/' \
  -e '/^\[permissions\]/,/^\[/ s/^mode = .*/mode = "auto"/' \
  -e '/^\[llmfit\]/,/^\[/ s/^command = .*/command = ""/' $NARSIL_HOME/config/config.toml

keys() { tmux send-keys -t $T "$@"; }
say() { tmux send-keys -t $T -l "$1"; keys Enter; }
screen() { tmux capture-pane -p -t $T; }
wait_for() {
  for _ in $(seq 1 $((${2:-30} * 5))); do
    if screen | grep -qE "$1"; then return 0; fi
    sleep 0.2
  done
  echo "timed out waiting for: $1" >&2
  return 1
}
shot() {
  sleep "${2:-0.8}"
  tmux capture-pane -e -p -t $T | "$python" "$here/render.py" "$out/$1.png" --cols $COLS --size 24 --pad 22
}

tmux new-session -d -s $T -x $COLS -y $ROWS -c $demo/home/tideline "bash --norc --noprofile"
sleep 0.5
say "export PS1='\$ ' PATH=$bin:/usr/bin:/bin COLORTERM=truecolor TERM=xterm-256color; clear"
sleep 0.3
say "narsil"
wait_for "enter send" 20

# At work: the explorer, the conversation, the objective and its plan, two subagents.
say "add retry with backoff to the client, and cover it with tests"
wait_for "2 subagents working" 30
say "/files"
shot overview 1.5
keys Escape
wait_for "● ready" 90

# A second request, then the windows.
say "what happens now if the feed is down?"
sleep 1
wait_for "● ready" 30
say "/usage"; shot usage 1.2; keys Escape; sleep 0.4
say "/trace"; shot trace 1.2; keys Escape; sleep 0.4
say "/logs"; shot logs 1.2; keys Escape; sleep 0.4
say "/theme"; shot themes 1; keys Escape; sleep 0.4
say "/keys"; shot keys 1; keys Escape; sleep 0.4

# What narsil did to one file: the explorer again (it opens on the file last changed), then enter.
say "/files"; sleep 0.5
say "/files"; sleep 0.8
keys Enter; shot file-history 1.2
keys Escape; sleep 0.4; keys Escape

# Asking before it runs something.
say "/ask"; sleep 0.5
say "run the tests once more"
wait_for "wants to run" 20
shot approval
keys y
wait_for "● ready" 30

# The same from a shell.
keys C-d; sleep 1
say "clear"
say 'narsil run "what happens now if the feed is down?"'
wait_for "narsil resume" 40
shot run 0.6

tmux kill-session -t $T
"$bin/narsil" stop >/dev/null 2>&1 || true
echo "screenshots are in $out"
