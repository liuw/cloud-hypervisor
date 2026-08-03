#!/usr/bin/env bash
# Copyright © 2026 The Cloud Hypervisor Authors. All rights reserved.
#
# SPDX-License-Identifier: Apache-2.0
#
# Stages the fuzz targets for OSS-Fuzz.
#
# Builds every target the way OSS-Fuzz builds them and copies the results into
# $OUT, together with the seed corpora, dictionaries and libFuzzer options
# that the disk image targets rely on. OSS-Fuzz sets $OUT and $SANITIZER; both
# fall back to values that make a local dry run work.
#
# The OSS-Fuzz build.sh for this project only has to call this script:
#
#     cd $SRC/cloud-hypervisor
#     ./scripts/oss-fuzz-build.sh

set -euo pipefail

OUT=${OUT:-$PWD/oss-fuzz-out}
SANITIZER=${SANITIZER:-address}
BUILD_DIR=fuzz/target/x86_64-unknown-linux-gnu/release
WORK_DIR=$(mktemp -d)

cleanup() {
    rm -rf "$WORK_DIR"
}
trap cleanup EXIT

# Builds with the sanitizer OSS-Fuzz asked for. A coverage build wants an
# uninstrumented binary, and anything else cargo-fuzz does not know falls back
# to address.
build_targets() {
    local sanitizer=$SANITIZER

    case "$sanitizer" in
    address | memory | thread | none) ;;
    coverage) sanitizer=none ;;
    *) sanitizer=address ;;
    esac

    cargo fuzz build --release --sanitizer "$sanitizer"
}

stage_binaries() {
    local target

    for source in fuzz/fuzz_targets/*.rs; do
        target=$(basename "${source%.rs}")
        cp "$BUILD_DIR/$target" "$OUT/"
    done
}

# Reports the input cap for a seeded target, as the next power of two at or
# above its largest seed.
#
# The cap has to be stated: left to itself libFuzzer never derives one above
# 1 MiB and truncates larger corpus entries to it, which turns a disk image
# into a rejected image and the target into a fuzzer of its own error path.
# Deriving it from the seeds keeps the two in step as formats are added.
max_len_for() {
    local dir=$1
    local largest

    largest=$(find "$dir" -type f -printf '%s\n' | sort -n | tail -1)
    python3 -c 'import sys; print(max(1 << 20, 1 << (int(sys.argv[1]) - 1).bit_length()))' \
        "$largest"
}

# Seeds every disk image target that the seed generator knows about, and caps
# the input size for it. Targets that build their own image, such as the
# operation program targets, need neither.
stage_disk_corpora() {
    local target

    if ! command -v qemu-img >/dev/null; then
        echo "warning: qemu-img not found, disk image targets get no seeds" >&2
        return 0
    fi

    scripts/generate-fuzz-seeds.sh "$WORK_DIR/corpus" >/dev/null

    for dir in "$WORK_DIR"/corpus/*/; do
        target=$(basename "$dir")
        # python3 rather than zip: the OSS-Fuzz base image is guaranteed to
        # have the former.
        python3 -c 'import shutil, sys; shutil.make_archive(sys.argv[1], "zip", sys.argv[2])' \
            "$OUT/${target}_seed_corpus" "$dir"
        printf '[libfuzzer]\nmax_len = %s\n' "$(max_len_for "$dir")" \
            >"$OUT/$target.options"
    done
}

stage_dictionaries() {
    local format

    for dict in fuzz/dictionaries/*.dict; do
        [ -e "$dict" ] || continue
        format=$(basename "${dict%.dict}")
        if [ -e "$BUILD_DIR/disk_$format" ]; then
            cp "$dict" "$OUT/disk_$format.dict"
        fi
    done
}

main() {
    mkdir -p "$OUT"

    build_targets
    stage_binaries
    stage_disk_corpora
    stage_dictionaries

    echo "staged for OSS-Fuzz in $OUT:"
    find "$OUT" -maxdepth 1 -mindepth 1 -printf '  %f\n' | sort
}

main
