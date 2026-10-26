# gbench

Benchmark protocol and runner for gardend. Full text: [PROTOCOL.md](PROTOCOL.md). Schema: `plans/2026-10-25-perf/SCHEMA.md` in the hub.

    node bench/gbench.mjs env
    node bench/gbench.mjs fixture --tier s|m|l --seed 1
    node bench/gbench.mjs run --suite smoke|standard --tier s|m --label <l> --out <dir> [--bin <gardend>] [--measured]
    node bench/gbench.mjs run --attach <url> --key-file <path>      # a running instance, graph `bench`
    node bench/gbench.mjs compare <runA> <runB>
    node bench/gbench.mjs ab --a <bin> --b <bin> --rounds 3
    node bench/gbench.mjs publish <runDir> --url <url> --key-file <path> [--baseline]

Node 22, no cargo. `pnpm install` in `bench/` only for the ws cases (yjs).
