#!/bin/sh
# The child behind synthetic-modes.rec. scripts/capture-rec.py runs it under
# gutter and releases each `read` with Enter once the stage before has gone
# quiet. Fixed strings only: the recording holds everything printed here.
stty -echo
next() { read -r _; }

printf 'synthetic modes\n'
printf '\033[1mbold\033[m \033[4munderline\033[m \033[31mred\033[m\n'
printf 'printed before the early resize\n'
next # the window is made narrower while history is still above the band

printf 'printed after the early resize\n'
printf 'slot 1: waiting\nslot 2: waiting\nslot 3: waiting\n'
next

# A repaint inside a synchronized update, with a pause so it spans frames.
printf '\033[?2026h\033[3A\r\033[2K\033[32mslot 1: done\033[m\n'
sleep 0.2
printf '\033[2K\033[32mslot 2: done\033[m\n\033[2K\033[33mslot 3: skipped\033[m\n\033[?2026l'
next # resize mode is entered, stepped and left while this read waits

printf '\033[?1049h\033[2J\033[1;1H\033[7m alternate screen \033[m'
printf '\033[3;5H\033[1;34mbold blue\033[m'
printf '\033[4;5H\033[4munderlined\033[m and \033[3mitalic\033[m'
printf '\033[5;5H\033[38;5;220;48;5;52mcolour on colour\033[m'
printf '\033[7;1Hwaiting on the alternate screen'
next # the window is resized while the alternate screen is up

printf '\033[?1049l'
printf 'back on the primary screen\n'
next
