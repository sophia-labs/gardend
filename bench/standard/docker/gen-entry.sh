#!/bin/bash
# gen-entry bsbm <products> <seed-dir> | sp2b <triples> | watdiv <scale> <queries-per-template>
# Outputs under /out. Deterministic: BSBM generator is seeded by its own fixed seed, SP2B and WatDiv are fixed.
set -euo pipefail
kind=$1; shift
case $kind in
  bsbm)
    pc=$1; cd /opt/bsbm
    # explore dataset: everything in the base file (no -ud). explore-update dataset: official -ud split, kept under /out/upd
    ./generate -fc -pc "$pc" -s nt -fn /out/dataset -dir /out/td_data
    mkdir -p /out/upd
    ./generate -fc -pc "$pc" -s nt -fn /out/upd/dataset -ud -ufn /out/upd/dataset_update -dir /out/upd/td_data
    ;;
  sp2b)
    t=$1; cd /opt/sp2b/bin
    ./sp2b_gen_native -t "$t" /out/dataset.n3
    cp -r /opt/sp2b/queries /out/queries
    ;;
  watdiv)
    sf=$1; q=${2:-20}; cd /opt/watdiv/bin/Release
    MODEL=/opt/watdiv/model/wsdbm-data-model.txt
    ./watdiv -d $MODEL "$sf" > /out/dataset.nt
    mkdir -p /out/queries
    for f in /opt/watdiv/testsuite/*.txt; do b=$(basename "$f"); ./watdiv -q $MODEL "$f" "$q" 1 > /out/queries/$b; done
    ;;
  version) java -version 2>&1 | head -1; g++ --version | head -1 ;;
  *) echo "unknown $kind" >&2; exit 2 ;;
esac
