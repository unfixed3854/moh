#!/usr/bin/env bash
set -euo pipefail

reset=$'\033[0m'
bold=$'\033[1m'
dim=$'\033[2m'
cyan=$'\033[36m'
green=$'\033[32m'
reverse=$'\033[7m'
reverse_off=$'\033[27m'
cyan_pill=$'\033[1;30;46m'

title() {
    printf '\n%b%s%b\n\n' "$bold$cyan" "$1" "$reset"
}

app_header() {
    printf 'moh — medium reasoning\n'
    printf 'Enter sends · / commands · Ctrl+O help · Ctrl+C exits\n'
}

prompt_and_status() {
    printf '%b❯%b /quit%b %b\n' "$cyan" "$reset" "$reverse" "$reverse_off"
    printf '%b╰─ %b%s%b · %bready%b · %b%s%b\n' \
        "$dim" "$reset$cyan" 'gpt-5.6-luna' "$dim" \
        "$reset$green" "$dim" "$dim" '/home/demo/Projects/moh' "$reset"
}

title 'A — Minimal'
app_header
printf '  %b› /quit%b  %bExit moh%b\n' "$bold$cyan" "$reset" "$dim" "$reset"
prompt_and_status

title 'B — Accent rail'
app_header
printf '%b│%b %b› /quit%b  %bExit moh%b\n' \
    "$dim$cyan" "$reset" "$bold$cyan" "$reset" "$dim" "$reset"
prompt_and_status

title 'C — Small card'
app_header
printf '%b╭─ commands ───────────╮%b\n' "$dim$cyan" "$reset"
printf '%b│%b %b› /quit%b  %bExit moh%b %b│%b\n' \
    "$dim$cyan" "$reset" "$bold$cyan" "$reset" "$dim" "$reset" "$dim$cyan" "$reset"
printf '%b╰──────────────────────╯%b\n' "$dim$cyan" "$reset"
prompt_and_status

title 'A — Intensity comparison'
app_header
printf '%bsubtle%b   %b›%b /quit  %bExit moh%b\n' \
    "$dim" "$reset" "$dim$cyan" "$reset" "$dim" "$reset"
printf '%bmedium%b   %b› /quit%b  %bExit moh%b\n' \
    "$dim" "$reset" "$bold$cyan" "$reset" "$dim" "$reset"
printf '%bstrong%b   %b › /quit %b  %bExit moh%b\n' \
    "$dim" "$reset" "$cyan_pill" "$reset" "$dim" "$reset"
prompt_and_status

printf '\n%bTip:%b judge the menu against your Alacritty background, not the labels above it.\n' \
    "$dim" "$reset"
