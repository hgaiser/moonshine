#!/bin/sh
# vkcube fullscreen plus a notification stand-in once XWayland is up.
if [ -n "$1" ]; then vkcube --wsi wayland --width "$1" --height "$2" & else vkcube --wsi wayland & fi
game=$!
sleep 2
python3 "$(dirname "$0")/notifier.py" &
notifier=$!
trap 'kill $game $notifier 2>/dev/null' EXIT INT TERM
wait $game
