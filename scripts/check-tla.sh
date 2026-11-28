#!/usr/bin/env bash
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Model-check cladefs's TLA+ specs with TLC (tlaplus/tlaplus, MIT): each directory of specs/ holds
# one spec (its .tla) and its models (TASK-44):
#
# - specs/dax-cow: the guest Fxfs's DAX, copying on write: the window mapping sealed extents and
#   zeros, a write's page private on the host, written back and rebound (guest/linux/fs/cladefs/dax.c,
#   doc-13, TASK-164.4). Writable DAX's specs (dax, dax-region, extent-open) went with it
#   (dead-ends/writable-dax.md);
# - specs/bucket: cells in object storage with the host's WAL (doc-7, doc-14, TASK-165.2), each
#   one log, forked only from its versions, into new cells, compacted by their writers (doc-27):
#   its mutations, one per rule of bucket_cell's Rules (two of newest_checkpoint) and of the WAL;
# - specs/journal: Fxfs's journal, objects' flushes, device flushes and the super-block
#   (crates/fxfs's write_super_block, TASK-149.11): a super-block claims only what its journal
#   flushed, which the objects' offsets read after the device's flush broke (TASK-149.5);
# - specs/buckets: forks across buckets and their mirrors (doc-23, TASK-633.2): children in one
#   bucket of a parent's versions in another, the automatic mirror a host may die in, both buckets'
#   collectors, buckets going down;
# - specs/ephemeral: ephemeral cells (doc-25, TASK-698.1): forks referring by incarnation, daemons
#   retiring their references on start, decommissioning, and live migration's hand-over;
# - specs/part-slabs: the extent cache in part slabs (doc-26, board 6124): slabs owned by a part's
#   slot, an eviction's token or the quarantine, which waits for the VM processes a slab was
#   staged to alone; copy on write in the VM process; frozen and killed VM processes;
# - specs/fast-wal: fast-WAL commits on an appendable object store (S3 Express One Zone / GCS
#   Rapid Storage, doc-29): offset-checked appends, adaptive group-commit batching, large-write
#   direct-to-cold upload + collector protection, segment rolling, tombstone reclamation, and
#   stateless compute-host takeovers;
#
#   check-tla.sh [MODEL...|deep]
#   check-tla.sh cover MODEL [NAME=VALUE...]
#
# checks each model, by its configuration's name, or SPEC/NAME (specs/SPEC/NAME.cfg, or
# specs/SPEC/mutations/NAME.cfg): by default all of them but the deep ones, each in minutes on a
# laptop (TASK-405: under ten). A model of a spec as designed must hold: TLC finds no violation.
# A mutation, the spec with one of its rules removed, must not: TLC must find the violation of the
# invariant its configuration names (`\* expect: NAME`), or of the temporal property it names and
# checks alone (`\* expect: property NAME`), or a deadlock (`\* expect: deadlock`), or the check
# fails, as a check that never fails proves nothing.
#
# A model too large to check at once is split (TASK-405): its configuration's `\* part:` lines,
# each constants set on top of the others (`\* part: Takers = 121; TakePhases = {11, 21, 32}`),
# are models of their own, checked one after another, `MODEL` all of them, `MODEL#K` the K-th,
# `MODEL#whole` the whole. Each part fixes choices every behaviour makes at most once, so that its
# parts check between them every behaviour the whole does. `cover MODEL [NAME=VALUE...]` shows it, on
# the model made small by the constants given or its `\* cover:` lines': every state the whole
# reaches, one of its parts does (states that a symmetry, of Hosts, relates counted as one), and,
# without a symmetry, they reach no other. A full run checks every split model's covers.
#
# A deep model, its configuration tagged `\* deep`, an occasional check of hours in all, runs only
# by name, or with any others as `deep`, its covers with it.
#
# TLC runs on Java: $JAVA, or java if it runs, or Homebrew's openjdk. Its jar, the release pinned
# below, is fetched once into ~/.cache/cladefs/tla and checked against its SHA-256. v1.8.0 is a
# pre-release TLA+ re-cuts in place (2026-11-28's, pinned 2026-11-28), and 1.7.4, the last that
# stays put, lacks -noGenerateSpecTE: a jar re-cut since fails the check until the pinned version
# follows it.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
specs="$root/specs"
tla_version=1.8.0
tla_sha256=df7cef5d37bcca879817424c001696e07fd094d68a72690449af3045fd972469
cache="$HOME/.cache/cladefs/tla"
jar="$cache/tla2tools-$tla_version-${tla_sha256:0:12}.jar"

if [[ ! -f $jar ]]; then
    mkdir -p "$cache"
    curl -sSfL -o "$jar.part" \
        "https://github.com/tlaplus/tlaplus/releases/download/v$tla_version/tla2tools.jar"
    mv "$jar.part" "$jar"
fi
echo "$tla_sha256  $jar" | shasum -a 256 -c --status ||
    { echo "$jar does not match its SHA-256: if TLA+ re-cut v$tla_version, check the release's" \
          "date and digest on GitHub, then pin the new jar here" >&2; exit 1; }

java=${JAVA:-}
if [[ -z $java ]]; then
    for candidate in java /opt/homebrew/opt/openjdk/bin/java /usr/local/opt/openjdk/bin/java; do
        if "$candidate" -version > /dev/null 2>&1; then
            java=$candidate
            break
        fi
    done
fi
[[ -n $java ]] || { echo "No Java runs: set JAVA" >&2; exit 2; }

# Every model's configuration, as specs/SPEC/[mutations/]NAME.cfg.
configs=()
for cfg in "$specs"/*/*.cfg "$specs"/*/mutations/*.cfg; do
    [[ -f $cfg ]] && configs+=("$cfg")
done
# A configuration's parts, a line each: its `\* part:` lines' overrides.
parts() { sed -n 's/^\\\* part: //p' "$1"; }
# Whether a configuration is a deep model's.
deep() { grep -q '^\\\* deep$' "$1"; }
# The runs chosen: each a configuration, and the part of it (a line of its parts), or none for
# the whole.
run_cfg=()
run_part=()
# The deep models whose covers are checked too.
covering=()
# choose CFG [K|whole]: its K-th part, or the whole; with neither, every part, or the whole if it
# has none.
choose() {
    local cfg=$1 which=${2:-} part k=0
    if [[ $which == whole ]] || [[ -z $which && -z $(parts "$cfg") ]]; then
        run_cfg+=("$cfg")
        run_part+=("")
        return
    fi
    while IFS= read -r part; do
        k=$((k + 1))
        if [[ -z $which || $which == "$k" ]]; then
            run_cfg+=("$cfg")
            run_part+=("$part")
        fi
    done < <(parts "$cfg")
    [[ -z $which ]] || (( which >= 1 && which <= k )) ||
        { echo "$cfg has no part $which" >&2; exit 2; }
}
# The configuration of the model named NAME or SPEC/NAME.
find_cfg() {
    local found=() cfg rel name
    for cfg in "${configs[@]}"; do
        rel=${cfg#"$specs"/}
        name=$(basename "$cfg" .cfg)
        if [[ $1 == "$name" || $1 == "${rel%%/*}/$name" ]]; then
            found+=("$cfg")
        fi
    done
    (( ${#found[@]} == 1 )) ||
        { echo "${#found[@]} models match $1: name it SPEC/NAME" >&2; exit 2; }
    echo "${found[0]}"
}
# with_part CFG PART: CFG's text, PART's overrides among its constants.
with_part() {
    printf '%s\n' "$2" | tr ';' '\n' | sed 's/^ */    /' | sed "/^CONSTANTS\$/r /dev/stdin" "$1"
}
if [[ ${1:-} == cover ]]; then
    (( $# >= 2 )) || { echo "cover names a model" >&2; exit 2; }
    cover=$(find_cfg "$2")
    shift 2
    [[ -n $(parts "$cover") ]] || { echo "$cover has no parts" >&2; exit 2; }
elif (( $# == 0 )); then
    for cfg in "${configs[@]}"; do
        deep "$cfg" || choose "$cfg"
    done
else
    for model in "$@"; do
        if [[ $model == deep ]]; then
            for cfg in "${configs[@]}"; do
                if deep "$cfg"; then
                    choose "$cfg"
                    covering+=("$cfg")
                fi
            done
            continue
        fi
        which=
        if [[ $model == *#* ]]; then
            which=${model#*#}
            model=${model%%#*}
        fi
        choose "$(find_cfg "$model")" "$which"
    done
fi

work=$(mktemp -d "${TMPDIR:-/tmp}/cladefs-tla.XXXXXX")
trap 'rm -r "$work"' EXIT

# reached NAME CFG TLA: the states CFG's model reaches, each a digest of its variables' values
# but the splits' history's (takers), sorted, into $work/NAME.reached. Under a symmetry, of Hosts
# (TLC lists one state of each orbit, whichever it found first), a state's digest is its orbit's:
# of the least of its texts, its hosts permuted.
reached() {
    local name=$1 cfg=$2 hosts=
    if grep -q '^SYMMETRY' "$cfg"; then
        hosts=$(sed -n 's/^    Hosts = {\(.*\)}$/\1/p' "$cfg" | tr -d ' ')
        [[ -n $hosts ]] || { echo "$cfg's symmetry is of no Hosts" >&2; exit 2; }
    fi
    "$java" -XX:+UseParallelGC -cp "$jar" tlc2.TLC -workers auto -noGenerateSpecTE \
        -metadir "$work/$name.states" -dump "$work/$name" -config "$cfg" "$3" \
        > "$work/$name.log" 2>&1 ||
        { echo "TLC failed on $name:"; sed -n '/^Error:/,$p' "$work/$name.log"; return 1; }
    python3 -I -c '
import hashlib, itertools, re, sys
hosts = [h for h in sys.argv[2].split(",") if h]
word = re.compile(r"\b(" + "|".join(map(re.escape, hosts)) + r")\b") if hosts else None
perms = [dict(zip(hosts, p)) for p in itertools.permutations(hosts)]
states, state, skip = set(), [], False
def done():
    if state:
        text = "".join(state)
        if word:
            text = min(word.sub(lambda m: p[m.group(1)], text) for p in perms)
        states.add(hashlib.sha1(text.encode()).hexdigest())
for line in open(sys.argv[1]):
    if line.startswith("State "):
        done()
        state, skip = [], False
        continue
    if line.startswith("/\\ "):
        skip = line.startswith("/\\ takers =")
    if not skip and line.strip():
        state.append(line)
done()
print("\n".join(sorted(states)))' "$work/$name.dump" "$hosts" > "$work/$name.reached"
    rm -r "$work/$name.dump" "$work/$name.states"
}

# covers CFG SETS: whether the parts of CFG's model, its constants set by SETS (as a part's
# overrides: small enough for TLC to list what they reach), reach between them every state the
# whole does: that its split is exact, its parts checking every behaviour it does.
covers() {
    local cfg=$1 sets=${2:-} name k=0 part whole missed extra start=$SECONDS tla
    tla=("$(dirname "$cfg")"/*.tla)
    local small="$work/cover.cfg" model
    model="${cfg#"$specs"/}"
    model="${model%%/*}/$(basename "$cfg" .cfg)"
    cp "$cfg" "$small"
    while IFS= read -r set; do
        name=$(echo "${set%%=*}" | tr -d ' ')
        [[ -n $name ]] || continue
        grep -qE "^    $name (=|<-) " "$small" || { echo "$cfg sets no $name" >&2; exit 2; }
        sed -i.bak -E "s/^    $name (=|<-) .*/    $name = $(echo "${set#*=}" | sed 's/^ *//')/" \
            "$small"
    done < <(printf '%s\n' "$sets" | tr ';' '\n')
    rm -f "$work"/cover-*.reached
    reached cover-whole "$small" "${tla[0]}" || return 1
    while IFS= read -r part; do
        k=$((k + 1))
        with_part "$small" "$part" > "$work/cover-$k.cfg"
        reached "cover-$k" "$work/cover-$k.cfg" "${tla[0]}" || return 1
    done < <(parts "$cfg")
    sort -u "$work"/cover-[0-9]*.reached > "$work/cover-parts.reached"
    whole=$(wc -l < "$work/cover-whole.reached")
    missed=$(comm -23 "$work/cover-whole.reached" "$work/cover-parts.reached" | wc -l)
    # Parts only restrict the whole's steps, so they reach no state it does not: that is checked
    # without a symmetry (or with one host), but not under one, as a function of hosts permuted,
    # its text keeping its order, needn't be the text TLC prints for it: a state may have two
    # digests.
    extra=0
    if ! grep -q '^SYMMETRY' "$small" ||
        (( $(sed -n 's/^    Hosts = {\(.*\)}$/\1/p' "$small" | tr ',' '\n' | wc -l) <= 1 )); then
        extra=$(comm -13 "$work/cover-whole.reached" "$work/cover-parts.reached" | wc -l)
    fi
    if (( missed == 0 && extra == 0 )); then
        echo "PASS $model covered: its $k parts reach the $((whole)) states the whole does" \
            "[$sets], $((SECONDS - start))s"
        return 0
    fi
    echo "FAIL $model covered: of the whole's $((whole)) states [$sets], its $k parts miss" \
        "$((missed)) and reach $((extra)) more"
    return 1
}

# covered CFG: whether CFG's model's parts cover it, on each model its `\* cover:` lines make.
covered() {
    local sets ok=0
    while IFS= read -r sets; do
        covers "$1" "$sets" || ok=1
    done < <(sed -n 's/^\\\* cover: //p' "$1")
    return $ok
}

if [[ -n ${cover:-} ]]; then
    if (( $# > 0 )); then covers "$cover" "$(IFS=';'; echo "$*")"; else covered "$cover"; fi
    exit
fi
status=0
for (( r = 0; r < ${#run_cfg[@]}; r++ )); do
    cfg=${run_cfg[r]}
    part=${run_part[r]}
    rel=${cfg#"$specs"/}
    spec_dir="$specs/${rel%%/*}"
    tla=("$spec_dir"/*.tla)
    (( ${#tla[@]} == 1 )) || { echo "$spec_dir holds no single spec" >&2; exit 2; }
    model="${rel%%/*}/$(basename "$cfg" .cfg)"
    key=$model
    if [[ -n $part ]]; then
        # The part's configuration: the whole's, its overrides among the constants.
        key="$model-$(printf '%s' "$part" | tr -c 'A-Za-z0-9' '_')"
        model="$model [$part]"
        with_part "$cfg" "$part" > "$work/${key//\//-}.cfg"
        cfg="$work/${key//\//-}.cfg"
    fi
    expect=
    if [[ $cfg == */mutations/* ]]; then
        expect=$(sed -n 's/^\\\* expect: //p' "$cfg")
        [[ -n $expect ]] || { echo "$cfg names no expected violation" >&2; exit 2; }
    fi
    log="$work/${key//\//-}.log"
    start=$SECONDS
    # TLC exits nonzero when it finds a violation, whose trace it prints rather than writing it
    # beside the spec (-noGenerateSpecTE).
    "$java" -XX:+UseParallelGC -cp "$jar" tlc2.TLC -workers auto -noGenerateSpecTE \
        -metadir "$work/${key//\//-}.states" -config "$cfg" "${tla[0]}" > "$log" 2>&1 &&
        found=0 || found=$?
    took=$((SECONDS - start))
    states=$(sed -n 's/.* \([0-9,]*\) distinct states found.*/\1/p' "$log" | sed -n '$p')
    if [[ -z $expect ]]; then
        if (( found == 0 )) && grep -q '^Model checking completed. No error has been found' "$log"
        then
            echo "PASS $model: holds, $states distinct states, ${took}s"
        else
            echo "FAIL $model: TLC found a violation, or failed (exit $found):"
            sed -n '/^Error:/,$p' "$log" | sed -n 1,200p
            status=1
        fi
    elif [[ $expect == deadlock ]] && grep -q '^Error: Deadlock reached' "$log"; then
        echo "PASS $model: deadlocks as expected, ${took}s"
    elif [[ $expect == property\ * ]] &&
        grep -q "^Error: Temporal property ${expect#property } was violated" "$log"; then
        echo "PASS $model: violates ${expect#property } as expected, ${took}s"
    elif grep -q "^Error: Invariant $expect is violated" "$log"; then
        echo "PASS $model: violates $expect as expected, ${took}s"
    else
        echo "FAIL $model: expected a violation of $expect, but (exit $found):"
        grep -E '^Error|is violated|No error has been found' "$log" | sed -n 1,20p
        status=1
    fi
done

# Each split model's cover, in a full run, and each deep one's with them.
if (( $# == 0 )); then
    for cfg in "${configs[@]}"; do
        deep "$cfg" || covered "$cfg" || status=1
    done
fi
for cfg in ${covering[@]+"${covering[@]}"}; do
    covered "$cfg" || status=1
done

exit "$status"
