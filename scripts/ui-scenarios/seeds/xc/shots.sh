#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
# Usage: shots.sh <scenario>  -> copies the latest run's PNGs (non-frame) to build/explore/xc/shots/<scenario>/<capture>-<main|w>.png
R=$(git rev-parse --show-toplevel)
run=$(/bin/ls -d $R/build/ui-snapshots/$1/2* | tail -1); out=$R/build/explore/xc/shots/$1; mkdir -p $out
for d in $run/[0-9]*/; do c=$(basename $d); for f in $d*.png; do b=$(basename $f); case $b in *.frame.png) ;; main.png) cp $f $out/$c-main.png;; *) cp $f $out/$c-w.png;; esac; done; done
echo $run; /bin/ls $out | tr '\n' ' '
