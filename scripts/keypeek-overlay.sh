#!/bin/sh
# Launch the KeyPeek overlay if it is not already running.
#
# Placement is the app's own job: the window is override-redirect (bypasses
# the WM entirely), self-positions from its window hints, is inherently
# visible on every workspace, and unmaps itself when idle. No i3 rules or
# i3-msg placement needed (the old for_window/no_focus rules for KeyPeek in
# ~/.config/i3/config are inert for that same reason).
BIN="$HOME/code/keypeek/target/release/keypeek"
LOG="/tmp/opencode/keypeek.out"

: "${DISPLAY:=:0}"
export DISPLAY

pgrep -x keypeek >/dev/null || setsid "$BIN" </dev/null >>"$LOG" 2>&1 &
