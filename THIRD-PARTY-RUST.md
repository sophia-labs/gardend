# Third-Party Rust Dependencies — Headless Engine Closure

This inventory covers the **`src-tauri` crate built for the headless engine** — the exact dependency closure of `cargo build --release --no-default-features --features headless --example gardend`, filtered to the `x86_64-unknown-linux-gnu` target used by `Dockerfile.gardend`. It does **not** cover the desktop (`--features desktop`) build, the `frontend/` npm dependency tree, or any Shrubbery-owned code — those are outside the public source-available boundary (see `EXPORT-MANIFEST.md`).

Generated 578 third-party crate entries from `cargo metadata` via `scripts/gen-third-party-rust.py` (neither `cargo-about` nor `cargo-license` was installed in the generating environment, so this table was built with a small script over `cargo metadata --format-version=1 --no-default-features --features headless --filter-platform x86_64-unknown-linux-gnu`, walking the resolved dependency graph from the `garden` package over normal + build edges and excluding dev-dependencies). Prefer running a real `cargo-about` or `cargo-license` pass instead when either is available, and regenerate whenever `src-tauri/Cargo.lock` changes.

| Crate | Version | License | Source |
|---|---|---|---|
| `adler2` | 2.0.1 | 0BSD OR MIT OR Apache-2.0 | https://github.com/oyvindln/adler2 |
| `aead` | 0.5.2 | MIT OR Apache-2.0 | https://github.com/RustCrypto/traits |
| `aegis` | 0.9.8 | MIT | https://github.com/jedisct1/rust-aegis |
| `aes` | 0.8.4 | MIT OR Apache-2.0 | https://github.com/RustCrypto/block-ciphers |
| `aes` | 0.9.1 | MIT OR Apache-2.0 | https://github.com/RustCrypto/block-ciphers |
| `aes-gcm` | 0.10.3 | Apache-2.0 OR MIT | https://github.com/RustCrypto/AEADs |
| `ahash` | 0.7.8 | MIT OR Apache-2.0 | https://github.com/tkaitchuck/ahash |
| `ahash` | 0.8.12 | MIT OR Apache-2.0 | https://github.com/tkaitchuck/ahash |
| `aho-corasick` | 1.1.4 | Unlicense OR MIT | https://github.com/BurntSushi/aho-corasick |
| `allocator-api2` | 0.2.21 | MIT OR Apache-2.0 | https://github.com/zakarumych/allocator-api2 |
| `ansi-str` | 0.9.0 | MIT | https://github.com/zhiburt/ansi-str |
| `ansitok` | 0.3.0 | MIT | https://gitlab.com/zhiburt/ansitok |
| `anstream` | 1.0.0 | MIT OR Apache-2.0 | https://github.com/rust-cli/anstyle.git |
| `anstyle` | 1.0.14 | MIT OR Apache-2.0 | https://github.com/rust-cli/anstyle.git |
| `anstyle-parse` | 1.0.0 | MIT OR Apache-2.0 | https://github.com/rust-cli/anstyle.git |
| `anstyle-query` | 1.1.5 | MIT OR Apache-2.0 | https://github.com/rust-cli/anstyle.git |
| `anyhow` | 1.0.102 | MIT OR Apache-2.0 | https://github.com/dtolnay/anyhow |
| `arc-swap` | 1.9.1 | MIT OR Apache-2.0 | https://github.com/vorner/arc-swap |
| `arraydeque` | 0.5.1 | MIT/Apache-2.0 | https://github.com/andylokandy/arraydeque |
| `arrayvec` | 0.7.6 | MIT OR Apache-2.0 | https://github.com/bluss/arrayvec |
| `async-lock` | 3.4.2 | Apache-2.0 OR MIT | https://github.com/smol-rs/async-lock |
| `async-trait` | 0.1.89 | MIT OR Apache-2.0 | https://github.com/dtolnay/async-trait |
| `atomic-waker` | 1.1.2 | Apache-2.0 OR MIT | https://github.com/smol-rs/atomic-waker |
| `autocfg` | 1.5.0 | Apache-2.0 OR MIT | https://github.com/cuviper/autocfg |
| `aws-config` | 1.10.1 | Apache-2.0 | https://github.com/smithy-lang/smithy-rs |
| `aws-credential-types` | 1.3.0 | Apache-2.0 | https://github.com/smithy-lang/smithy-rs |
| `aws-runtime` | 1.9.1 | Apache-2.0 | https://github.com/smithy-lang/smithy-rs |
| `aws-sdk-dynamodb` | 1.119.0 | Apache-2.0 | https://github.com/awslabs/aws-sdk-rust |
| `aws-sdk-sts` | 1.110.0 | Apache-2.0 | https://github.com/awslabs/aws-sdk-rust |
| `aws-sigv4` | 1.5.1 | Apache-2.0 | https://github.com/smithy-lang/smithy-rs |
| `aws-smithy-async` | 1.3.0 | Apache-2.0 | https://github.com/smithy-lang/smithy-rs |
| `aws-smithy-http` | 0.64.0 | Apache-2.0 | https://github.com/smithy-lang/smithy-rs |
| `aws-smithy-http-client` | 1.2.0 | Apache-2.0 | https://github.com/smithy-lang/smithy-rs |
| `aws-smithy-json` | 0.63.0 | Apache-2.0 | https://github.com/smithy-lang/smithy-rs |
| `aws-smithy-observability` | 0.3.0 | Apache-2.0 | https://github.com/awslabs/smithy-rs |
| `aws-smithy-query` | 0.62.0 | Apache-2.0 | https://github.com/smithy-lang/smithy-rs |
| `aws-smithy-runtime` | 1.12.1 | Apache-2.0 | https://github.com/smithy-lang/smithy-rs |
| `aws-smithy-runtime-api` | 1.14.0 | Apache-2.0 | https://github.com/smithy-lang/smithy-rs |
| `aws-smithy-runtime-api-macros` | 1.1.0 | Apache-2.0 | https://github.com/smithy-lang/smithy-rs |
| `aws-smithy-schema` | 0.2.0 | Apache-2.0 | https://github.com/smithy-lang/smithy-rs |
| `aws-smithy-types` | 1.6.1 | Apache-2.0 | https://github.com/smithy-lang/smithy-rs |
| `aws-smithy-xml` | 0.62.0 | Apache-2.0 | https://github.com/smithy-lang/smithy-rs |
| `aws-types` | 1.5.0 | Apache-2.0 | https://github.com/smithy-lang/smithy-rs |
| `axum` | 0.8.9 | MIT | https://github.com/tokio-rs/axum |
| `axum-core` | 0.5.6 | MIT | https://github.com/tokio-rs/axum |
| `base64` | 0.13.1 | MIT/Apache-2.0 | https://github.com/marshallpierce/rust-base64 |
| `base64` | 0.22.1 | MIT OR Apache-2.0 | https://github.com/marshallpierce/rust-base64 |
| `base64-simd` | 0.8.0 | MIT | https://github.com/Nugine/simd |
| `base64ct` | 1.8.3 | Apache-2.0 OR MIT | https://github.com/RustCrypto/formats |
| `bigdecimal` | 0.4.10 | MIT/Apache-2.0 | https://github.com/akubera/bigdecimal-rs |
| `bindgen` | 0.69.5 | BSD-3-Clause | https://github.com/rust-lang/rust-bindgen |
| `bindgen` | 0.72.1 | BSD-3-Clause | https://github.com/rust-lang/rust-bindgen |
| `bit-set` | 0.8.0 | Apache-2.0 OR MIT | https://github.com/contain-rs/bit-set |
| `bit-vec` | 0.8.0 | Apache-2.0 OR MIT | https://github.com/contain-rs/bit-vec |
| `bitflags` | 2.11.1 | MIT OR Apache-2.0 | https://github.com/bitflags/bitflags |
| `bitvec` | 1.0.1 | MIT | https://github.com/bitvecto-rs/bitvec |
| `block-buffer` | 0.10.4 | MIT OR Apache-2.0 | https://github.com/RustCrypto/utils |
| `block-buffer` | 0.12.1 | MIT OR Apache-2.0 | https://github.com/RustCrypto/utils |
| `borsh` | 1.6.1 | MIT OR Apache-2.0 | https://github.com/near/borsh-rs |
| `borsh-derive` | 1.6.1 | Apache-2.0 | https://github.com/near/borsh-rs |
| `branches` | 0.4.4 | MIT | https://github.com/fereidani/branches |
| `bumpalo` | 3.20.2 | MIT OR Apache-2.0 | https://github.com/fitzgen/bumpalo |
| `bytecheck` | 0.6.12 | MIT | https://github.com/djkoloski/bytecheck |
| `bytecheck_derive` | 0.6.12 | MIT | https://github.com/djkoloski/bytecheck |
| `bytecount` | 0.6.9 | Apache-2.0/MIT | https://github.com/llogiq/bytecount |
| `bytemuck` | 1.25.0 | Zlib OR Apache-2.0 OR MIT | https://github.com/Lokathor/bytemuck |
| `bytemuck_derive` | 1.10.2 | Zlib OR Apache-2.0 OR MIT | https://github.com/Lokathor/bytemuck |
| `byteorder` | 1.5.0 | Unlicense OR MIT | https://github.com/BurntSushi/byteorder |
| `bytes` | 1.11.1 | MIT | https://github.com/tokio-rs/bytes |
| `bytes-utils` | 0.1.4 | Apache-2.0/MIT | https://github.com/vorner/bytes-utils |
| `bzip2` | 0.6.1 | MIT OR Apache-2.0 | https://github.com/trifectatechfoundation/bzip2-rs |
| `candle-core` | 0.10.2 | MIT OR Apache-2.0 | https://github.com/huggingface/candle |
| `candle-nn` | 0.10.2 | MIT OR Apache-2.0 | https://github.com/huggingface/candle |
| `castaway` | 0.2.4 | MIT | https://github.com/sagebind/castaway |
| `cc` | 1.2.61 | MIT OR Apache-2.0 | https://github.com/rust-lang/cc-rs |
| `cexpr` | 0.6.0 | Apache-2.0/MIT | https://github.com/jethrogb/rust-cexpr |
| `cfg-if` | 1.0.4 | MIT OR Apache-2.0 | https://github.com/rust-lang/cfg-if |
| `cfg_aliases` | 0.2.1 | MIT | https://github.com/katharostech/cfg_aliases |
| `cfg_block` | 0.1.1 | see `LICENSE` in crate source (no SPDX expression declared) | https://github.com/pluots/cfg_block |
| `chrono` | 0.4.44 | MIT OR Apache-2.0 | https://github.com/chronotope/chrono |
| `cipher` | 0.4.4 | MIT OR Apache-2.0 | https://github.com/RustCrypto/traits |
| `cipher` | 0.5.2 | MIT OR Apache-2.0 | https://github.com/RustCrypto/traits |
| `clang-sys` | 1.8.1 | Apache-2.0 | https://github.com/KyleMayes/clang-sys |
| `clap` | 4.6.1 | MIT OR Apache-2.0 | https://github.com/clap-rs/clap |
| `clap_builder` | 4.6.0 | MIT OR Apache-2.0 | https://github.com/clap-rs/clap |
| `clap_derive` | 4.6.1 | MIT OR Apache-2.0 | https://github.com/clap-rs/clap |
| `clap_lex` | 1.1.0 | MIT OR Apache-2.0 | https://github.com/clap-rs/clap |
| `cmov` | 0.5.4 | Apache-2.0 OR MIT | https://github.com/RustCrypto/utils |
| `colorchoice` | 1.0.5 | MIT OR Apache-2.0 | https://github.com/rust-cli/anstyle.git |
| `colored` | 3.1.1 | MPL-2.0 | https://github.com/mackwic/colored |
| `compact_str` | 0.9.0 | MIT | https://github.com/ParkMyCar/compact_str |
| `concurrent-queue` | 2.5.0 | Apache-2.0 OR MIT | https://github.com/smol-rs/concurrent-queue |
| `console` | 0.16.3 | MIT | https://github.com/console-rs/console |
| `const-oid` | 0.10.2 | Apache-2.0 OR MIT | https://github.com/RustCrypto/formats |
| `const_format` | 0.2.36 | Zlib | https://github.com/rodrimati1992/const_format_crates/ |
| `const_format_proc_macros` | 0.2.34 | Zlib | https://github.com/rodrimati1992/const_format_crates/ |
| `constant_time_eq` | 0.4.2 | CC0-1.0 OR MIT-0 OR Apache-2.0 | https://github.com/cesarb/constant_time_eq |
| `cookie` | 0.18.1 | MIT OR Apache-2.0 | https://github.com/SergioBenitez/cookie-rs |
| `cookie_store` | 0.22.1 | MIT OR Apache-2.0 | https://github.com/pfernie/cookie_store |
| `cpubits` | 0.1.1 | MIT OR Apache-2.0 | https://github.com/RustCrypto/utils |
| `cpufeatures` | 0.2.17 | MIT OR Apache-2.0 | https://github.com/RustCrypto/utils |
| `cpufeatures` | 0.3.0 | MIT OR Apache-2.0 | https://github.com/RustCrypto/utils |
| `crc32c` | 0.6.8 | Apache-2.0/MIT | https://github.com/zowens/crc32c |
| `crc32fast` | 1.5.0 | MIT OR Apache-2.0 | https://github.com/srijs/rust-crc32fast |
| `crossbeam-channel` | 0.5.15 | MIT OR Apache-2.0 | https://github.com/crossbeam-rs/crossbeam |
| `crossbeam-deque` | 0.8.6 | MIT OR Apache-2.0 | https://github.com/crossbeam-rs/crossbeam |
| `crossbeam-epoch` | 0.9.18 | MIT OR Apache-2.0 | https://github.com/crossbeam-rs/crossbeam |
| `crossbeam-skiplist` | 0.1.3 | MIT OR Apache-2.0 | https://github.com/crossbeam-rs/crossbeam |
| `crossbeam-utils` | 0.8.21 | MIT OR Apache-2.0 | https://github.com/crossbeam-rs/crossbeam |
| `crypto-common` | 0.1.7 | MIT OR Apache-2.0 | https://github.com/RustCrypto/traits |
| `crypto-common` | 0.2.2 | MIT OR Apache-2.0 | https://github.com/RustCrypto/traits |
| `cssparser` | 0.37.0 | MPL-2.0 | https://github.com/servo/rust-cssparser |
| `cssparser-macros` | 0.7.0 | MPL-2.0 | https://github.com/servo/rust-cssparser |
| `ctr` | 0.9.2 | MIT OR Apache-2.0 | https://github.com/RustCrypto/block-modes |
| `ctutils` | 0.4.2 | Apache-2.0 OR MIT | https://github.com/RustCrypto/utils |
| `darling` | 0.20.11 | MIT | https://github.com/TedDriggs/darling |
| `darling_core` | 0.20.11 | MIT | https://github.com/TedDriggs/darling |
| `darling_macro` | 0.20.11 | MIT | https://github.com/TedDriggs/darling |
| `dary_heap` | 0.3.9 | MIT OR Apache-2.0 | https://github.com/hanmertens/dary_heap |
| `dashmap` | 6.1.0 | MIT | https://github.com/xacrimon/dashmap |
| `data-encoding` | 2.11.0 | MIT | https://github.com/ia0/data-encoding |
| `deflate64` | 0.1.12 | MIT | https://github.com/anatawa12/deflate64-rs |
| `der` | 0.8.0 | Apache-2.0 OR MIT | https://github.com/RustCrypto/formats |
| `deranged` | 0.5.8 | MIT OR Apache-2.0 | https://github.com/jhpratt/deranged |
| `derive_builder` | 0.20.2 | MIT OR Apache-2.0 | https://github.com/colin-kiegel/rust-derive-builder |
| `derive_builder_core` | 0.20.2 | MIT OR Apache-2.0 | https://github.com/colin-kiegel/rust-derive-builder |
| `derive_builder_macro` | 0.20.2 | MIT OR Apache-2.0 | https://github.com/colin-kiegel/rust-derive-builder |
| `derive_more` | 2.1.1 | MIT | https://github.com/JelteF/derive_more |
| `derive_more-impl` | 2.1.1 | MIT | https://github.com/JelteF/derive_more |
| `digest` | 0.10.7 | MIT OR Apache-2.0 | https://github.com/RustCrypto/traits |
| `digest` | 0.11.3 | MIT OR Apache-2.0 | https://github.com/RustCrypto/traits |
| `dirs` | 6.0.0 | MIT OR Apache-2.0 | https://github.com/soc/dirs-rs |
| `dirs-sys` | 0.5.0 | MIT OR Apache-2.0 | https://github.com/dirs-dev/dirs-sys-rs |
| `displaydoc` | 0.2.5 | MIT OR Apache-2.0 | https://github.com/yaahc/displaydoc |
| `document-features` | 0.2.12 | MIT OR Apache-2.0 | https://github.com/slint-ui/document-features |
| `dtoa` | 1.0.11 | MIT OR Apache-2.0 | https://github.com/dtolnay/dtoa |
| `dtoa-short` | 0.3.5 | MPL-2.0 | https://github.com/upsuper/dtoa-short |
| `dyn-stack` | 0.13.2 | MIT | https://codeberg.org/sarah-quinones/dyn-stack |
| `dyn-stack-macros` | 0.1.3 | MIT | https://github.com/kitegi/dynstack/ |
| `ego-tree` | 0.11.0 | ISC | https://github.com/rust-scraper/ego-tree |
| `either` | 1.15.0 | MIT OR Apache-2.0 | https://github.com/rayon-rs/either |
| `encoding_rs` | 0.8.35 | (Apache-2.0 OR MIT) AND BSD-3-Clause | https://github.com/hsivonen/encoding_rs |
| `env_filter` | 1.0.1 | MIT OR Apache-2.0 | https://github.com/rust-cli/env_logger |
| `env_logger` | 0.11.10 | MIT OR Apache-2.0 | https://github.com/rust-cli/env_logger |
| `equivalent` | 1.0.2 | Apache-2.0 OR MIT | https://github.com/indexmap-rs/equivalent |
| `errno` | 0.3.14 | MIT OR Apache-2.0 | https://github.com/lambda-fairy/rust-errno |
| `esaxx-rs` | 0.1.10 | Apache-2.0 | https://github.com/Narsil/esaxx-rs |
| `event-listener` | 5.4.1 | Apache-2.0 OR MIT | https://github.com/smol-rs/event-listener |
| `event-listener-strategy` | 0.5.4 | Apache-2.0 OR MIT | https://github.com/smol-rs/event-listener-strategy |
| `fallible-iterator` | 0.3.0 | MIT/Apache-2.0 | https://github.com/sfackler/rust-fallible-iterator |
| `fastbloom` | 0.14.1 | MIT OR Apache-2.0 | https://github.com/tomtomwombat/fastbloom/ |
| `fastembed` | 5.13.4 | Apache-2.0 | https://github.com/Anush008/fastembed-rs |
| `fastrand` | 2.4.1 | Apache-2.0 OR MIT | https://github.com/smol-rs/fastrand |
| `filetime` | 0.2.29 | MIT/Apache-2.0 | https://github.com/alexcrichton/filetime |
| `find-msvc-tools` | 0.1.9 | MIT OR Apache-2.0 | https://github.com/rust-lang/cc-rs |
| `fixedbitset` | 0.5.7 | MIT OR Apache-2.0 | https://github.com/petgraph/fixedbitset |
| `flate2` | 1.1.9 | MIT OR Apache-2.0 | https://github.com/rust-lang/flate2-rs |
| `float8` | 0.7.0 | MIT | https://github.com/EricLBuehler/float8 |
| `fnv` | 1.0.7 | Apache-2.0 / MIT | https://github.com/servo/rust-fnv |
| `foldhash` | 0.1.5 | Zlib | https://github.com/orlp/foldhash |
| `foldhash` | 0.2.0 | Zlib | https://github.com/orlp/foldhash |
| `foreign-types` | 0.3.2 | MIT/Apache-2.0 | https://github.com/sfackler/foreign-types |
| `foreign-types-shared` | 0.1.1 | MIT/Apache-2.0 | https://github.com/sfackler/foreign-types |
| `form_urlencoded` | 1.2.2 | MIT OR Apache-2.0 | https://github.com/servo/rust-url |
| `fs4` | 0.13.1 | MIT OR Apache-2.0 | https://github.com/al8n/fs4-rs |
| `funty` | 2.0.0 | MIT | https://github.com/myrrlyn/funty |
| `futures-channel` | 0.3.32 | MIT OR Apache-2.0 | https://github.com/rust-lang/futures-rs |
| `futures-core` | 0.3.32 | MIT OR Apache-2.0 | https://github.com/rust-lang/futures-rs |
| `futures-io` | 0.3.32 | MIT OR Apache-2.0 | https://github.com/rust-lang/futures-rs |
| `futures-macro` | 0.3.32 | MIT OR Apache-2.0 | https://github.com/rust-lang/futures-rs |
| `futures-sink` | 0.3.32 | MIT OR Apache-2.0 | https://github.com/rust-lang/futures-rs |
| `futures-task` | 0.3.32 | MIT OR Apache-2.0 | https://github.com/rust-lang/futures-rs |
| `futures-util` | 0.3.32 | MIT OR Apache-2.0 | https://github.com/rust-lang/futures-rs |
| `gemm` | 0.19.0 | MIT | https://github.com/sarah-ek/gemm/ |
| `gemm-c32` | 0.19.0 | MIT | https://github.com/sarah-ek/gemm/ |
| `gemm-c64` | 0.19.0 | MIT | https://github.com/sarah-ek/gemm/ |
| `gemm-common` | 0.19.0 | MIT | https://github.com/sarah-ek/gemm/ |
| `gemm-f16` | 0.19.0 | MIT | https://github.com/sarah-ek/gemm/ |
| `gemm-f32` | 0.19.0 | MIT | https://github.com/sarah-ek/gemm/ |
| `gemm-f64` | 0.19.0 | MIT | https://github.com/sarah-ek/gemm/ |
| `genawaiter` | 0.99.1 | MIT | https://github.com/whatisaphone/genawaiter |
| `genawaiter-macro` | 0.99.1 | MIT/Apache-2.0 | https://github.com/whatisaphone/genawaiter |
| `generic-array` | 0.14.7 | MIT | https://github.com/fizyk20/generic-array.git |
| `getopts` | 0.2.24 | MIT OR Apache-2.0 | https://github.com/rust-lang/getopts |
| `getrandom` | 0.2.17 | MIT OR Apache-2.0 | https://github.com/rust-random/getrandom |
| `getrandom` | 0.3.4 | MIT OR Apache-2.0 | https://github.com/rust-random/getrandom |
| `getrandom` | 0.4.2 | MIT OR Apache-2.0 | https://github.com/rust-random/getrandom |
| `ghash` | 0.5.1 | Apache-2.0 OR MIT | https://github.com/RustCrypto/universal-hashes |
| `glob` | 0.3.3 | MIT OR Apache-2.0 | https://github.com/rust-lang/glob |
| `h2` | 0.4.13 | MIT | https://github.com/hyperium/h2 |
| `half` | 2.7.1 | MIT OR Apache-2.0 | https://github.com/VoidStarKat/half-rs |
| `hashbrown` | 0.12.3 | MIT OR Apache-2.0 | https://github.com/rust-lang/hashbrown |
| `hashbrown` | 0.14.5 | MIT OR Apache-2.0 | https://github.com/rust-lang/hashbrown |
| `hashbrown` | 0.15.5 | MIT OR Apache-2.0 | https://github.com/rust-lang/hashbrown |
| `hashbrown` | 0.16.1 | MIT OR Apache-2.0 | https://github.com/rust-lang/hashbrown |
| `hashbrown` | 0.17.0 | MIT OR Apache-2.0 | https://github.com/rust-lang/hashbrown |
| `hashlink` | 0.10.0 | MIT OR Apache-2.0 | https://github.com/kyren/hashlink |
| `heck` | 0.5.0 | MIT OR Apache-2.0 | https://github.com/withoutboats/heck |
| `hex` | 0.4.3 | MIT OR Apache-2.0 | https://github.com/KokaKiwi/rust-hex |
| `hf-hub` | 0.5.0 | Apache-2.0 | https://github.com/huggingface/hf-hub |
| `hmac` | 0.12.1 | MIT OR Apache-2.0 | https://github.com/RustCrypto/MACs |
| `hmac` | 0.13.0 | MIT OR Apache-2.0 | https://github.com/RustCrypto/MACs |
| `home` | 0.5.12 | MIT OR Apache-2.0 | https://github.com/rust-lang/cargo |
| `html5ever` | 0.39.0 | MIT OR Apache-2.0 | https://github.com/servo/html5ever |
| `http` | 0.2.12 | MIT OR Apache-2.0 | https://github.com/hyperium/http |
| `http` | 1.4.0 | MIT OR Apache-2.0 | https://github.com/hyperium/http |
| `http-body` | 0.4.6 | MIT | https://github.com/hyperium/http-body |
| `http-body` | 1.0.1 | MIT | https://github.com/hyperium/http-body |
| `http-body-util` | 0.1.3 | MIT | https://github.com/hyperium/http-body |
| `httparse` | 1.10.1 | MIT OR Apache-2.0 | https://github.com/seanmonstar/httparse |
| `httpdate` | 1.0.3 | MIT OR Apache-2.0 | https://github.com/pyfisch/httpdate |
| `hybrid-array` | 0.4.12 | MIT OR Apache-2.0 | https://github.com/RustCrypto/hybrid-array |
| `hyper` | 1.9.0 | MIT | https://github.com/hyperium/hyper |
| `hyper-rustls` | 0.27.9 | Apache-2.0 OR ISC OR MIT | https://github.com/rustls/hyper-rustls |
| `hyper-tls` | 0.6.0 | MIT/Apache-2.0 | https://github.com/hyperium/hyper-tls |
| `hyper-util` | 0.1.20 | MIT | https://github.com/hyperium/hyper-util |
| `iana-time-zone` | 0.1.65 | MIT OR Apache-2.0 | https://github.com/strawlab/iana-time-zone |
| `icu_collections` | 2.2.0 | Unicode-3.0 | https://github.com/unicode-org/icu4x |
| `icu_locale_core` | 2.2.0 | Unicode-3.0 | https://github.com/unicode-org/icu4x |
| `icu_normalizer` | 2.2.0 | Unicode-3.0 | https://github.com/unicode-org/icu4x |
| `icu_normalizer_data` | 2.2.0 | Unicode-3.0 | https://github.com/unicode-org/icu4x |
| `icu_properties` | 2.2.0 | Unicode-3.0 | https://github.com/unicode-org/icu4x |
| `icu_properties_data` | 2.2.0 | Unicode-3.0 | https://github.com/unicode-org/icu4x |
| `icu_provider` | 2.2.0 | Unicode-3.0 | https://github.com/unicode-org/icu4x |
| `ident_case` | 1.0.1 | MIT/Apache-2.0 | https://github.com/TedDriggs/ident_case |
| `idna` | 1.1.0 | MIT OR Apache-2.0 | https://github.com/servo/rust-url/ |
| `idna_adapter` | 1.2.2 | Apache-2.0 OR MIT | https://github.com/hsivonen/idna_adapter |
| `indexmap` | 2.14.0 | Apache-2.0 OR MIT | https://github.com/indexmap-rs/indexmap |
| `indicatif` | 0.18.4 | MIT | https://github.com/console-rs/indicatif |
| `indoc` | 2.0.7 | MIT OR Apache-2.0 | https://github.com/dtolnay/indoc |
| `inout` | 0.1.4 | MIT OR Apache-2.0 | https://github.com/RustCrypto/utils |
| `inout` | 0.2.2 | MIT OR Apache-2.0 | https://github.com/RustCrypto/utils |
| `intrusive-collections` | 0.9.7 | Apache-2.0/MIT | https://github.com/Amanieu/intrusive-rs |
| `io-uring` | 0.7.12 | MIT OR Apache-2.0 | https://github.com/tokio-rs/io-uring |
| `ipnet` | 2.12.0 | MIT OR Apache-2.0 | https://github.com/krisprice/ipnet |
| `iri-string` | 0.7.12 | MIT OR Apache-2.0 | https://github.com/lo48576/iri-string |
| `iri_s` | 0.2.9 | MIT OR Apache-2.0 | https://github.com/rudof-project/rudof |
| `is_terminal_polyfill` | 1.70.2 | MIT OR Apache-2.0 | https://github.com/polyfill-rs/is_terminal_polyfill |
| `itertools` | 0.12.1 | MIT OR Apache-2.0 | https://github.com/rust-itertools/itertools |
| `itertools` | 0.14.0 | MIT OR Apache-2.0 | https://github.com/rust-itertools/itertools |
| `itoa` | 1.0.18 | MIT OR Apache-2.0 | https://github.com/dtolnay/itoa |
| `jiff` | 0.2.28 | Unlicense OR MIT | https://github.com/BurntSushi/jiff |
| `jobserver` | 0.1.34 | MIT OR Apache-2.0 | https://github.com/rust-lang/jobserver-rs |
| `json-event-parser` | 0.2.3 | MIT OR Apache-2.0 | https://github.com/oxigraph/json-event-parser |
| `konst` | 0.2.20 | Zlib | https://github.com/rodrimati1992/konst/ |
| `konst_macro_rules` | 0.2.19 | Zlib | https://github.com/rodrimati1992/konst/ |
| `lazy_static` | 1.5.0 | MIT OR Apache-2.0 | https://github.com/rust-lang-nursery/lazy-static.rs |
| `lazycell` | 1.3.0 | MIT/Apache-2.0 | https://github.com/indiv0/lazycell |
| `libbz2-rs-sys` | 0.2.5 | bzip2-1.0.6 | https://github.com/trifectatechfoundation/libbzip2-rs |
| `libc` | 0.2.186 | MIT OR Apache-2.0 | https://github.com/rust-lang/libc |
| `libloading` | 0.8.9 | ISC | https://github.com/nagisa/rust_libloading/ |
| `libloading` | 0.9.0 | ISC | https://github.com/nagisa/rust_libloading/ |
| `libm` | 0.2.16 | MIT | https://github.com/rust-lang/compiler-builtins |
| `linux-raw-sys` | 0.12.1 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | https://github.com/sunfishcode/linux-raw-sys |
| `linux-raw-sys` | 0.4.15 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | https://github.com/sunfishcode/linux-raw-sys |
| `litemap` | 0.8.2 | Unicode-3.0 | https://github.com/unicode-org/icu4x |
| `litrs` | 1.0.0 | MIT OR Apache-2.0 | https://github.com/LukasKalbertodt/litrs |
| `lock_api` | 0.4.14 | MIT OR Apache-2.0 | https://github.com/Amanieu/parking_lot |
| `log` | 0.4.29 | MIT OR Apache-2.0 | https://github.com/rust-lang/log |
| `lru-slab` | 0.1.2 | MIT OR Apache-2.0 OR Zlib | https://github.com/Ralith/lru-slab |
| `lzma-rust2` | 0.16.4 | Apache-2.0 | https://github.com/hasenbanck/lzma-rust2/ |
| `macro_rules_attribute` | 0.2.2 | Apache-2.0 OR MIT OR Zlib | https://github.com/danielhenrymantilla/macro_rules_attribute-rs |
| `macro_rules_attribute-proc_macro` | 0.2.2 | Apache-2.0 OR MIT OR Zlib | https://github.com/danielhenrymantilla/macro_rules_attribute-rs |
| `markup5ever` | 0.39.0 | MIT OR Apache-2.0 | https://github.com/servo/html5ever |
| `matchers` | 0.2.0 | MIT | https://github.com/hawkw/matchers |
| `matchit` | 0.8.4 | MIT AND BSD-3-Clause | https://github.com/ibraheemdev/matchit |
| `matrixmultiply` | 0.3.10 | MIT/Apache-2.0 | https://github.com/bluss/matrixmultiply/ |
| `md-5` | 0.10.6 | MIT OR Apache-2.0 | https://github.com/RustCrypto/hashes |
| `memchr` | 2.8.0 | Unlicense OR MIT | https://github.com/BurntSushi/memchr |
| `memmap2` | 0.9.10 | MIT OR Apache-2.0 | https://github.com/RazrFalcon/memmap2-rs |
| `memoffset` | 0.9.1 | MIT | https://github.com/Gilnaa/memoffset |
| `mie` | 0.2.9 | MIT OR Apache-2.0 | https://github.com/rudof-project/rudof |
| `miette` | 7.6.0 | Apache-2.0 | https://github.com/zkat/miette |
| `miette-derive` | 7.6.0 | Apache-2.0 | https://github.com/zkat/miette |
| `mime` | 0.3.17 | MIT OR Apache-2.0 | https://github.com/hyperium/mime |
| `minimal-lexical` | 0.2.1 | MIT/Apache-2.0 | https://github.com/Alexhuszagh/minimal-lexical |
| `miniz_oxide` | 0.8.9 | MIT OR Zlib OR Apache-2.0 | https://github.com/Frommi/miniz_oxide/tree/master/miniz_oxide |
| `mio` | 1.2.0 | MIT | https://github.com/tokio-rs/mio |
| `monostate` | 0.1.18 | MIT OR Apache-2.0 | https://github.com/dtolnay/monostate |
| `monostate-impl` | 0.1.18 | MIT OR Apache-2.0 | https://github.com/dtolnay/monostate |
| `multer` | 3.1.0 | MIT | https://github.com/rwf2/multer |
| `native-tls` | 0.2.18 | MIT OR Apache-2.0 | https://github.com/rust-native-tls/rust-native-tls |
| `ndarray` | 0.17.2 | MIT OR Apache-2.0 | https://github.com/rust-ndarray/ndarray |
| `new_debug_unreachable` | 1.0.6 | MIT | https://github.com/mbrubeck/rust-debug-unreachable |
| `nom` | 7.1.3 | MIT | https://github.com/Geal/nom |
| `nu-ansi-term` | 0.50.3 | MIT | https://github.com/nushell/nu-ansi-term |
| `num-bigint` | 0.4.6 | MIT OR Apache-2.0 | https://github.com/rust-num/num-bigint |
| `num-complex` | 0.4.6 | MIT OR Apache-2.0 | https://github.com/rust-num/num-complex |
| `num-conv` | 0.2.1 | MIT OR Apache-2.0 | https://github.com/jhpratt/num-conv |
| `num-integer` | 0.1.46 | MIT OR Apache-2.0 | https://github.com/rust-num/num-integer |
| `num-traits` | 0.2.19 | MIT OR Apache-2.0 | https://github.com/rust-num/num-traits |
| `num_cpus` | 1.17.0 | MIT OR Apache-2.0 | https://github.com/seanmonstar/num_cpus |
| `once_cell` | 1.21.4 | MIT OR Apache-2.0 | https://github.com/matklad/once_cell |
| `onig` | 6.5.3 | MIT | https://github.com/iwillspeak/rust-onig |
| `onig_sys` | 69.9.3 | MIT | https://github.com/rust-onig/rust-onig |
| `opaque-debug` | 0.3.1 | MIT OR Apache-2.0 | https://github.com/RustCrypto/utils |
| `openssl` | 0.10.78 | Apache-2.0 | https://github.com/rust-openssl/rust-openssl |
| `openssl-macros` | 0.1.1 | MIT/Apache-2.0 | crates.io |
| `openssl-probe` | 0.2.1 | MIT OR Apache-2.0 | https://github.com/rustls/openssl-probe |
| `openssl-sys` | 0.9.114 | MIT | https://github.com/rust-openssl/rust-openssl |
| `option-ext` | 0.2.0 | MPL-2.0 | https://github.com/soc/option-ext.git |
| `ort` | 2.0.0-rc.12 | MIT OR Apache-2.0 | https://github.com/pykeio/ort |
| `ort-sys` | 2.0.0-rc.12 | MIT OR Apache-2.0 | https://github.com/pykeio/ort |
| `outref` | 0.5.2 | MIT | https://github.com/Nugine/outref |
| `oxhttp` | 0.3.3 | MIT OR Apache-2.0 | https://github.com/oxigraph/oxhttp |
| `oxigraph` | 0.5.9 | MIT OR Apache-2.0 | https://github.com/oxigraph/oxigraph/tree/main/lib/oxigraph |
| `oxilangtag` | 0.1.5 | MIT | https://github.com/oxigraph/oxilangtag |
| `oxiri` | 0.2.11 | MIT OR Apache-2.0 | https://github.com/oxigraph/oxiri |
| `oxjsonld` | 0.2.5 | MIT OR Apache-2.0 | https://github.com/oxigraph/oxigraph/tree/master/lib/oxjsonld |
| `oxrdf` | 0.3.3 | MIT OR Apache-2.0 | https://github.com/oxigraph/oxigraph/tree/main/lib/oxrdf |
| `oxrdfio` | 0.2.5 | MIT OR Apache-2.0 | https://github.com/oxigraph/oxigraph/tree/master/lib/oxrdfxml |
| `oxrdfxml` | 0.2.3 | MIT OR Apache-2.0 | https://github.com/oxigraph/oxigraph/tree/master/lib/oxrdfxml |
| `oxrocksdb-sys` | 0.5.9 | GPL-2.0-only OR Apache-2.0 | https://github.com/oxigraph/oxigraph/tree/main/oxrocksdb-sys |
| `oxsdatatypes` | 0.2.2 | MIT OR Apache-2.0 | https://github.com/oxigraph/oxigraph/tree/main/lib/oxsdatatypes |
| `oxttl` | 0.2.3 | MIT OR Apache-2.0 | https://github.com/oxigraph/oxigraph/tree/master/lib/oxttl |
| `pack1` | 1.0.0 | Zlib OR Apache-2.0 OR MIT | https://github.com/Lokathor/pack1 |
| `papergrid` | 0.17.0 | MIT | https://github.com/zhiburt/tabled |
| `parking` | 2.2.1 | Apache-2.0 OR MIT | https://github.com/smol-rs/parking |
| `parking_lot` | 0.12.5 | MIT OR Apache-2.0 | https://github.com/Amanieu/parking_lot |
| `parking_lot_core` | 0.9.12 | MIT OR Apache-2.0 | https://github.com/Amanieu/parking_lot |
| `paste` | 1.0.15 | MIT OR Apache-2.0 | https://github.com/dtolnay/paste |
| `pastey` | 0.2.2 | MIT OR Apache-2.0 | https://github.com/as1100k/pastey |
| `pbkdf2` | 0.13.0 | MIT OR Apache-2.0 | https://github.com/RustCrypto/password-hashes |
| `peg` | 0.8.5 | MIT | https://github.com/kevinmehall/rust-peg |
| `peg-macros` | 0.8.5 | MIT | https://github.com/kevinmehall/rust-peg |
| `peg-runtime` | 0.8.5 | MIT | https://github.com/kevinmehall/rust-peg |
| `pem-rfc7468` | 1.0.0 | Apache-2.0 OR MIT | https://github.com/RustCrypto/formats |
| `percent-encoding` | 2.3.2 | MIT OR Apache-2.0 | https://github.com/servo/rust-url/ |
| `petgraph` | 0.8.3 | MIT OR Apache-2.0 | https://github.com/petgraph/petgraph |
| `phf` | 0.13.1 | MIT | https://github.com/rust-phf/rust-phf |
| `phf_codegen` | 0.13.1 | MIT | https://github.com/rust-phf/rust-phf |
| `phf_generator` | 0.13.1 | MIT | https://github.com/rust-phf/rust-phf |
| `phf_macros` | 0.13.1 | MIT | https://github.com/rust-phf/rust-phf |
| `phf_shared` | 0.13.1 | MIT | https://github.com/rust-phf/rust-phf |
| `pin-project-lite` | 0.2.17 | Apache-2.0 OR MIT | https://github.com/taiki-e/pin-project-lite |
| `pin-utils` | 0.1.0 | MIT OR Apache-2.0 | https://github.com/rust-lang-nursery/pin-utils |
| `pkg-config` | 0.3.33 | MIT OR Apache-2.0 | https://github.com/rust-lang/pkg-config-rs |
| `polling` | 3.11.0 | Apache-2.0 OR MIT | https://github.com/smol-rs/polling |
| `polyval` | 0.6.2 | Apache-2.0 OR MIT | https://github.com/RustCrypto/universal-hashes |
| `portable-atomic` | 1.13.1 | Apache-2.0 OR MIT | https://github.com/taiki-e/portable-atomic |
| `potential_utf` | 0.1.5 | Unicode-3.0 | https://github.com/unicode-org/icu4x |
| `powerfmt` | 0.2.0 | MIT OR Apache-2.0 | https://github.com/jhpratt/powerfmt |
| `ppmd-rust` | 1.4.0 | CC0-1.0 OR MIT-0 | https://github.com/hasenbanck/ppmd-rust |
| `ppv-lite86` | 0.2.21 | MIT OR Apache-2.0 | https://github.com/cryptocorrosion/cryptocorrosion |
| `precomputed-hash` | 0.1.1 | MIT | https://github.com/emilio/precomputed-hash |
| `prefixmap` | 0.2.9 | MIT OR Apache-2.0 | https://github.com/rudof-project/rudof |
| `prettyplease` | 0.2.37 | MIT OR Apache-2.0 | https://github.com/dtolnay/prettyplease |
| `proc-macro-crate` | 3.5.0 | MIT OR Apache-2.0 | https://github.com/bkchr/proc-macro-crate |
| `proc-macro-error-attr2` | 2.0.0 | MIT OR Apache-2.0 | https://github.com/GnomedDev/proc-macro-error-2 |
| `proc-macro-error2` | 2.0.1 | MIT OR Apache-2.0 | https://github.com/GnomedDev/proc-macro-error-2 |
| `proc-macro2` | 1.0.106 | MIT OR Apache-2.0 | https://github.com/dtolnay/proc-macro2 |
| `proptest` | 1.11.0 | MIT OR Apache-2.0 | https://github.com/proptest-rs/proptest |
| `prost` | 0.14.3 | Apache-2.0 | https://github.com/tokio-rs/prost |
| `prost-derive` | 0.14.3 | Apache-2.0 | https://github.com/tokio-rs/prost |
| `ptr_meta` | 0.1.4 | MIT | https://github.com/djkoloski/ptr_meta |
| `ptr_meta_derive` | 0.1.4 | MIT | https://github.com/djkoloski/ptr_meta |
| `pulldown-cmark` | 0.13.4 | MIT | https://github.com/raphlinus/pulldown-cmark |
| `pulp` | 0.22.2 | MIT | https://github.com/sarah-quinones/pulp/ |
| `pulp-wasm-simd-flag` | 0.1.0 | MIT | https://github.com/sarah-quinones/pulp/ |
| `quick-error` | 1.2.3 | MIT/Apache-2.0 | http://github.com/tailhook/quick-error |
| `quick-xml` | 0.37.5 | MIT | https://github.com/tafia/quick-xml |
| `quinn` | 0.11.9 | MIT OR Apache-2.0 | https://github.com/quinn-rs/quinn |
| `quinn-proto` | 0.11.14 | MIT OR Apache-2.0 | https://github.com/quinn-rs/quinn |
| `quinn-udp` | 0.5.14 | MIT OR Apache-2.0 | https://github.com/quinn-rs/quinn |
| `quote` | 1.0.45 | MIT OR Apache-2.0 | https://github.com/dtolnay/quote |
| `radium` | 0.7.0 | MIT | https://github.com/bitvecto-rs/radium |
| `rand` | 0.8.6 | MIT OR Apache-2.0 | https://github.com/rust-random/rand |
| `rand` | 0.9.4 | MIT OR Apache-2.0 | https://github.com/rust-random/rand |
| `rand_chacha` | 0.3.1 | MIT OR Apache-2.0 | https://github.com/rust-random/rand |
| `rand_chacha` | 0.9.0 | MIT OR Apache-2.0 | https://github.com/rust-random/rand |
| `rand_core` | 0.6.4 | MIT OR Apache-2.0 | https://github.com/rust-random/rand |
| `rand_core` | 0.9.5 | MIT OR Apache-2.0 | https://github.com/rust-random/rand |
| `rand_distr` | 0.5.1 | MIT OR Apache-2.0 | https://github.com/rust-random/rand_distr |
| `rand_xorshift` | 0.4.0 | MIT OR Apache-2.0 | https://github.com/rust-random/rngs |
| `rapidhash` | 4.4.1 | MIT OR Apache-2.0 | https://github.com/hoxxep/rapidhash |
| `raw-cpuid` | 11.6.0 | MIT | https://github.com/gz/rust-cpuid |
| `rawpointer` | 0.2.1 | MIT/Apache-2.0 | https://github.com/bluss/rawpointer/ |
| `rayon` | 1.12.0 | MIT OR Apache-2.0 | https://github.com/rayon-rs/rayon |
| `rayon-cond` | 0.4.0 | Apache-2.0/MIT | https://github.com/cuviper/rayon-cond |
| `rayon-core` | 1.13.0 | MIT OR Apache-2.0 | https://github.com/rayon-rs/rayon |
| `reborrow` | 0.5.5 | MIT | https://github.com/sarah-ek/reborrow/ |
| `regex` | 1.12.4 | MIT OR Apache-2.0 | https://github.com/rust-lang/regex |
| `regex-automata` | 0.4.14 | MIT OR Apache-2.0 | https://github.com/rust-lang/regex |
| `regex-lite` | 0.1.9 | MIT OR Apache-2.0 | https://github.com/rust-lang/regex |
| `regex-syntax` | 0.8.11 | MIT OR Apache-2.0 | https://github.com/rust-lang/regex |
| `rend` | 0.4.2 | MIT | https://github.com/djkoloski/rend |
| `reqwest` | 0.12.28 | MIT OR Apache-2.0 | https://github.com/seanmonstar/reqwest |
| `ring` | 0.17.14 | Apache-2.0 AND ISC | https://github.com/briansmith/ring |
| `rkyv` | 0.7.46 | MIT | https://github.com/rkyv/rkyv |
| `rkyv_derive` | 0.7.46 | MIT | https://github.com/rkyv/rkyv |
| `roaring` | 0.11.4 | MIT OR Apache-2.0 | https://github.com/RoaringBitmap/roaring-rs |
| `roxmltree` | 0.21.1 | MIT OR Apache-2.0 | https://github.com/RazrFalcon/roxmltree |
| `rudof_rdf` | 0.2.12 | MIT OR Apache-2.0 | https://github.com/rudof-project/rudof |
| `rust_decimal` | 1.41.0 | MIT | https://github.com/paupino/rust-decimal |
| `rustc-hash` | 1.1.0 | Apache-2.0/MIT | https://github.com/rust-lang-nursery/rustc-hash |
| `rustc-hash` | 2.1.2 | Apache-2.0 OR MIT | https://github.com/rust-lang/rustc-hash |
| `rustc_version` | 0.4.1 | MIT OR Apache-2.0 | https://github.com/djc/rustc-version-rs |
| `rustix` | 0.38.44 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | https://github.com/bytecodealliance/rustix |
| `rustix` | 1.1.4 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | https://github.com/bytecodealliance/rustix |
| `rustls` | 0.23.39 | Apache-2.0 OR ISC OR MIT | https://github.com/rustls/rustls |
| `rustls-native-certs` | 0.8.4 | Apache-2.0 OR ISC OR MIT | https://github.com/rustls/rustls-native-certs |
| `rustls-pki-types` | 1.14.1 | MIT OR Apache-2.0 | https://github.com/rustls/pki-types |
| `rustls-platform-verifier` | 0.7.0 | MIT OR Apache-2.0 | https://github.com/rustls/rustls-platform-verifier |
| `rustls-webpki` | 0.103.13 | ISC | https://github.com/rustls/webpki |
| `rustversion` | 1.0.22 | MIT OR Apache-2.0 | https://github.com/dtolnay/rustversion |
| `rusty-fork` | 0.3.1 | MIT/Apache-2.0 | https://github.com/altsysrq/rusty-fork |
| `ryu` | 1.0.23 | Apache-2.0 OR BSL-1.0 | https://github.com/dtolnay/ryu |
| `ryu-js` | 1.0.2 | Apache-2.0 OR BSL-1.0 | https://github.com/boa-dev/ryu-js |
| `safetensors` | 0.7.0 | Apache-2.0 | https://github.com/huggingface/safetensors |
| `scopeguard` | 1.2.0 | MIT OR Apache-2.0 | https://github.com/bluss/scopeguard |
| `scraper` | 0.27.0 | ISC | https://github.com/rust-scraper/scraper |
| `seahash` | 4.1.0 | MIT | https://gitlab.redox-os.org/redox-os/seahash |
| `selectors` | 0.38.0 | MPL-2.0 | https://github.com/servo/stylo |
| `semver` | 1.0.28 | MIT OR Apache-2.0 | https://github.com/dtolnay/semver |
| `seq-macro` | 0.3.6 | MIT OR Apache-2.0 | https://github.com/dtolnay/seq-macro |
| `serde` | 1.0.228 | MIT OR Apache-2.0 | https://github.com/serde-rs/serde |
| `serde_core` | 1.0.228 | MIT OR Apache-2.0 | https://github.com/serde-rs/serde |
| `serde_derive` | 1.0.228 | MIT OR Apache-2.0 | https://github.com/serde-rs/serde |
| `serde_json` | 1.0.149 | MIT OR Apache-2.0 | https://github.com/serde-rs/json |
| `serde_json_canonicalizer` | 0.3.2 | MIT | https://github.com/evik42/serde-json-canonicalizer |
| `serde_path_to_error` | 0.1.20 | MIT OR Apache-2.0 | https://github.com/dtolnay/path-to-error |
| `serde_spanned` | 1.1.1 | MIT OR Apache-2.0 | https://github.com/toml-rs/toml |
| `serde_urlencoded` | 0.7.1 | MIT/Apache-2.0 | https://github.com/nox/serde_urlencoded |
| `servo_arc` | 0.4.3 | MIT OR Apache-2.0 | https://github.com/servo/stylo |
| `sha1` | 0.10.6 | MIT OR Apache-2.0 | https://github.com/RustCrypto/hashes |
| `sha1` | 0.11.0 | MIT OR Apache-2.0 | https://github.com/RustCrypto/hashes |
| `sha1_smol` | 1.0.1 | BSD-3-Clause | https://github.com/mitsuhiko/sha1-smol |
| `sha2` | 0.10.9 | MIT OR Apache-2.0 | https://github.com/RustCrypto/hashes |
| `sha2` | 0.11.0 | MIT OR Apache-2.0 | https://github.com/RustCrypto/hashes |
| `shacl_ast` | 0.2.9 | MIT OR Apache-2.0 | https://github.com/rudof-project/rudof |
| `shacl_ir` | 0.2.9 | MIT OR Apache-2.0 | https://github.com/rudof-project/rudof |
| `shacl_rdf` | 0.2.9 | MIT OR Apache-2.0 | https://github.com/rudof-project/rudof |
| `shacl_validation` | 0.2.12 | MIT OR Apache-2.0 | https://github.com/rudof-project/rudof |
| `sharded-slab` | 0.1.7 | MIT | https://github.com/hawkw/sharded-slab |
| `shlex` | 1.3.0 | MIT OR Apache-2.0 | https://github.com/comex/rust-shlex |
| `signal-hook-registry` | 1.4.8 | MIT OR Apache-2.0 | https://github.com/vorner/signal-hook |
| `simd-adler32` | 0.3.9 | MIT | https://github.com/mcountryman/simd-adler32 |
| `simdutf8` | 0.1.5 | MIT OR Apache-2.0 | https://github.com/rusticstuff/simdutf8 |
| `simsimd` | 6.5.16 | Apache-2.0 | https://github.com/ashvardanian/SimSIMD |
| `siphasher` | 1.0.2 | MIT/Apache-2.0 | https://github.com/jedisct1/rust-siphash |
| `slab` | 0.4.12 | MIT | https://github.com/tokio-rs/slab |
| `smallstr` | 0.3.1 | MIT OR Apache-2.0 | https://github.com/murarth/smallstr |
| `smallvec` | 1.15.1 | MIT OR Apache-2.0 | https://github.com/servo/rust-smallvec |
| `socket2` | 0.6.3 | MIT OR Apache-2.0 | https://github.com/rust-lang/socket2 |
| `socks` | 0.3.4 | MIT/Apache-2.0 | https://github.com/sfackler/rust-socks |
| `softaes` | 0.1.3 | MIT | https://github.com/jedisct1/rust-softaes |
| `sparesults` | 0.3.3 | MIT OR Apache-2.0 | https://github.com/oxigraph/oxigraph/tree/main/lib/sparesults |
| `spareval` | 0.2.6 | MIT OR Apache-2.0 | https://github.com/oxigraph/oxigraph/tree/main/lib/spareval |
| `spargebra` | 0.4.6 | MIT OR Apache-2.0 | https://github.com/oxigraph/oxigraph/tree/main/lib/spargebra |
| `sparopt` | 0.3.6 | MIT OR Apache-2.0 | https://github.com/oxigraph/oxigraph/tree/main/lib/sparopt |
| `sparql_service` | 0.2.12 | MIT OR Apache-2.0 | https://github.com/rudof-project/rudof |
| `spin` | 0.9.8 | MIT | https://github.com/mvdnes/spin-rs.git |
| `spm_precompiled` | 0.1.4 | Apache-2.0 | https://github.com/huggingface/spm_precompiled |
| `stable_deref_trait` | 1.2.1 | MIT OR Apache-2.0 | https://github.com/storyyeller/stable_deref_trait |
| `static_assertions` | 1.1.0 | MIT OR Apache-2.0 | https://github.com/nvzqz/static-assertions-rs |
| `string_cache` | 0.9.0 | MIT OR Apache-2.0 | https://github.com/servo/string-cache |
| `string_cache_codegen` | 0.6.1 | MIT OR Apache-2.0 | https://github.com/servo/string-cache |
| `strsim` | 0.11.1 | MIT | https://github.com/rapidfuzz/strsim-rs |
| `strum` | 0.26.3 | MIT | https://github.com/Peternator7/strum |
| `strum_macros` | 0.26.4 | MIT | https://github.com/Peternator7/strum |
| `subtle` | 2.6.1 | BSD-3-Clause | https://github.com/dalek-cryptography/subtle |
| `symlink` | 0.1.0 | MIT/Apache-2.0 | https://gitlab.com/chris-morgan/symlink |
| `syn` | 1.0.109 | MIT OR Apache-2.0 | https://github.com/dtolnay/syn |
| `syn` | 2.0.117 | MIT OR Apache-2.0 | https://github.com/dtolnay/syn |
| `sync_wrapper` | 1.0.2 | Apache-2.0 | https://github.com/Actyx/sync_wrapper |
| `synstructure` | 0.13.2 | MIT | https://github.com/mystor/synstructure |
| `tabled` | 0.20.0 | MIT | https://github.com/zhiburt/tabled |
| `tabled_derive` | 0.11.0 | MIT | https://github.com/zhiburt/tabled |
| `tap` | 1.0.1 | MIT | https://github.com/myrrlyn/tap |
| `tar` | 0.4.46 | MIT OR Apache-2.0 | https://github.com/composefs/tar-rs |
| `tempfile` | 3.27.0 | MIT OR Apache-2.0 | https://github.com/Stebalien/tempfile |
| `tendril` | 0.5.0 | MIT OR Apache-2.0 | https://github.com/servo/html5ever |
| `testing_table` | 0.3.0 | MIT | https://github.com/zhiburt/tabled |
| `thiserror` | 2.0.18 | MIT OR Apache-2.0 | https://github.com/dtolnay/thiserror |
| `thiserror-impl` | 2.0.18 | MIT OR Apache-2.0 | https://github.com/dtolnay/thiserror |
| `thread_local` | 1.1.9 | MIT OR Apache-2.0 | https://github.com/Amanieu/thread_local-rs |
| `time` | 0.3.47 | MIT OR Apache-2.0 | https://github.com/time-rs/time |
| `time-core` | 0.1.8 | MIT OR Apache-2.0 | https://github.com/time-rs/time |
| `time-macros` | 0.2.27 | MIT OR Apache-2.0 | https://github.com/time-rs/time |
| `tinystr` | 0.8.3 | Unicode-3.0 | https://github.com/unicode-org/icu4x |
| `tinyvec` | 1.11.0 | Zlib OR Apache-2.0 OR MIT | https://github.com/Lokathor/tinyvec |
| `tinyvec_macros` | 0.1.1 | MIT OR Apache-2.0 OR Zlib | https://github.com/Soveu/tinyvec_macros |
| `tokenizers` | 0.22.2 | Apache-2.0 | https://github.com/huggingface/tokenizers |
| `tokio` | 1.52.1 | MIT | https://github.com/tokio-rs/tokio |
| `tokio-macros` | 2.7.0 | MIT | https://github.com/tokio-rs/tokio |
| `tokio-native-tls` | 0.3.1 | MIT | https://github.com/tokio-rs/tls |
| `tokio-rustls` | 0.26.4 | MIT OR Apache-2.0 | https://github.com/rustls/tokio-rustls |
| `tokio-tungstenite` | 0.29.0 | MIT | https://github.com/snapview/tokio-tungstenite |
| `tokio-util` | 0.7.18 | MIT | https://github.com/tokio-rs/tokio |
| `toml` | 0.9.12+spec-1.1.0 | MIT OR Apache-2.0 | https://github.com/toml-rs/toml |
| `toml_datetime` | 0.7.5+spec-1.1.0 | MIT OR Apache-2.0 | https://github.com/toml-rs/toml |
| `toml_datetime` | 1.1.1+spec-1.1.0 | MIT OR Apache-2.0 | https://github.com/toml-rs/toml |
| `toml_edit` | 0.25.11+spec-1.1.0 | MIT OR Apache-2.0 | https://github.com/toml-rs/toml |
| `toml_parser` | 1.1.2+spec-1.1.0 | MIT OR Apache-2.0 | https://github.com/toml-rs/toml |
| `toml_writer` | 1.1.1+spec-1.1.0 | MIT OR Apache-2.0 | https://github.com/toml-rs/toml |
| `tower` | 0.5.3 | MIT | https://github.com/tower-rs/tower |
| `tower-http` | 0.6.8 | MIT | https://github.com/tower-rs/tower-http |
| `tower-layer` | 0.3.3 | MIT | https://github.com/tower-rs/tower |
| `tower-service` | 0.3.3 | MIT | https://github.com/tower-rs/tower |
| `tracing` | 0.1.44 | MIT | https://github.com/tokio-rs/tracing |
| `tracing-appender` | 0.2.5 | MIT | https://github.com/tokio-rs/tracing |
| `tracing-attributes` | 0.1.31 | MIT | https://github.com/tokio-rs/tracing |
| `tracing-core` | 0.1.36 | MIT | https://github.com/tokio-rs/tracing |
| `tracing-log` | 0.2.0 | MIT | https://github.com/tokio-rs/tracing |
| `tracing-subscriber` | 0.3.23 | MIT | https://github.com/tokio-rs/tracing |
| `try-lock` | 0.2.5 | MIT | https://github.com/seanmonstar/try-lock |
| `tungstenite` | 0.29.0 | MIT OR Apache-2.0 | https://github.com/snapview/tungstenite-rs |
| `turso` | 0.6.0-pre.26 | MIT | https://github.com/tursodatabase/turso |
| `turso_core` | 0.6.0-pre.26 | MIT | https://github.com/tursodatabase/turso |
| `turso_ext` | 0.6.0-pre.26 | MIT | https://github.com/tursodatabase/turso |
| `turso_macros` | 0.6.0-pre.26 | MIT | https://github.com/tursodatabase/turso |
| `turso_parser` | 0.6.0-pre.26 | MIT | https://github.com/tursodatabase/turso |
| `turso_sdk_kit` | 0.6.0-pre.26 | MIT | https://github.com/tursodatabase/turso |
| `turso_sdk_kit_macros` | 0.6.0-pre.26 | MIT | https://github.com/tursodatabase/turso |
| `turso_sync_engine` | 0.6.0-pre.26 | MIT | https://github.com/tursodatabase/turso |
| `turso_sync_sdk_kit` | 0.6.0-pre.26 | MIT | https://github.com/tursodatabase/turso |
| `twox-hash` | 2.1.2 | MIT | https://github.com/shepmaster/twox-hash |
| `typed-path` | 0.12.3 | MIT OR Apache-2.0 | https://github.com/chipsenkbeil/typed-path |
| `typenum` | 1.20.0 | MIT OR Apache-2.0 | https://github.com/paholg/typenum |
| `ulid` | 1.2.1 | MIT | https://github.com/dylanhart/ulid-rs |
| `unarray` | 0.1.4 | MIT OR Apache-2.0 | https://github.com/cameron1024/unarray |
| `uncased` | 0.9.10 | MIT OR Apache-2.0 | https://github.com/SergioBenitez/uncased |
| `unicase` | 2.9.0 | MIT OR Apache-2.0 | https://github.com/seanmonstar/unicase |
| `unicode-ident` | 1.0.24 | (MIT OR Apache-2.0) AND Unicode-3.0 | https://github.com/dtolnay/unicode-ident |
| `unicode-normalization-alignments` | 0.1.12 | MIT/Apache-2.0 | https://github.com/n1t0/unicode-normalization |
| `unicode-segmentation` | 1.13.2 | MIT OR Apache-2.0 | https://github.com/unicode-rs/unicode-segmentation |
| `unicode-width` | 0.1.14 | MIT OR Apache-2.0 | https://github.com/unicode-rs/unicode-width |
| `unicode-width` | 0.2.2 | MIT OR Apache-2.0 | https://github.com/unicode-rs/unicode-width |
| `unicode-xid` | 0.2.6 | MIT OR Apache-2.0 | https://github.com/unicode-rs/unicode-xid |
| `unicode_categories` | 0.1.1 | MIT OR Apache-2.0 | https://github.com/swgillespie/unicode-categories |
| `unit-prefix` | 0.5.2 | MIT | https://codeberg.org/commons-rs/unit-prefix |
| `universal-hash` | 0.5.1 | MIT OR Apache-2.0 | https://github.com/RustCrypto/traits |
| `untrusted` | 0.9.0 | ISC | https://github.com/briansmith/untrusted |
| `ureq` | 3.3.0 | MIT OR Apache-2.0 | https://github.com/algesten/ureq |
| `ureq-proto` | 0.6.0 | MIT OR Apache-2.0 | https://github.com/algesten/ureq-proto |
| `url` | 2.5.8 | MIT OR Apache-2.0 | https://github.com/servo/rust-url |
| `urlencoding` | 2.1.3 | MIT | https://github.com/kornelski/rust_urlencoding |
| `utf-8` | 0.7.6 | MIT OR Apache-2.0 | https://github.com/SimonSapin/rust-utf8 |
| `utf8-zero` | 0.8.1 | MIT OR Apache-2.0 | https://github.com/algesten/utf8-zero |
| `utf8_iter` | 1.0.4 | Apache-2.0 OR MIT | https://github.com/hsivonen/utf8_iter |
| `utf8parse` | 0.2.2 | Apache-2.0 OR MIT | https://github.com/alacritty/vte |
| `uuid` | 1.23.1 | Apache-2.0 OR MIT | https://github.com/uuid-rs/uuid |
| `vcpkg` | 0.2.15 | MIT/Apache-2.0 | https://github.com/mcgoo/vcpkg-rs |
| `version_check` | 0.9.5 | MIT/Apache-2.0 | https://github.com/SergioBenitez/version_check |
| `vsimd` | 0.8.0 | MIT | https://github.com/Nugine/simd |
| `vte` | 0.14.1 | Apache-2.0 OR MIT | https://github.com/alacritty/vte |
| `wait-timeout` | 0.2.1 | MIT/Apache-2.0 | https://github.com/alexcrichton/wait-timeout |
| `want` | 0.3.1 | MIT | https://github.com/seanmonstar/want |
| `web_atoms` | 0.2.4 | MIT OR Apache-2.0 | https://github.com/servo/html5ever |
| `webpki-root-certs` | 1.0.7 | CDLA-Permissive-2.0 | https://github.com/rustls/webpki-roots |
| `webpki-roots` | 1.0.7 | CDLA-Permissive-2.0 | https://github.com/rustls/webpki-roots |
| `which` | 4.4.2 | MIT | https://github.com/harryfei/which-rs.git |
| `winnow` | 0.7.15 | MIT | https://github.com/winnow-rs/winnow |
| `winnow` | 1.0.2 | MIT | https://github.com/winnow-rs/winnow |
| `writeable` | 0.6.3 | Unicode-3.0 | https://github.com/unicode-org/icu4x |
| `wyz` | 0.5.1 | MIT | https://github.com/myrrlyn/wyz |
| `xattr` | 1.6.1 | MIT OR Apache-2.0 | https://github.com/Stebalien/xattr |
| `xmlparser` | 0.13.6 | MIT/Apache-2.0 | https://github.com/RazrFalcon/xmlparser |
| `yaml-rust2` | 0.10.4 | MIT OR Apache-2.0 | https://github.com/Ethiraric/yaml-rust2 |
| `yoke` | 0.8.2 | Unicode-3.0 | https://github.com/unicode-org/icu4x |
| `yoke-derive` | 0.8.2 | Unicode-3.0 | https://github.com/unicode-org/icu4x |
| `yrs` | 0.27.0 | MIT | https://github.com/y-crdt/y-crdt/ |
| `zerocopy` | 0.8.48 | BSD-2-Clause OR Apache-2.0 OR MIT | https://github.com/google/zerocopy |
| `zerocopy-derive` | 0.8.48 | BSD-2-Clause OR Apache-2.0 OR MIT | https://github.com/google/zerocopy |
| `zerofrom` | 0.1.7 | Unicode-3.0 | https://github.com/unicode-org/icu4x |
| `zerofrom-derive` | 0.1.7 | Unicode-3.0 | https://github.com/unicode-org/icu4x |
| `zeroize` | 1.8.2 | Apache-2.0 OR MIT | https://github.com/RustCrypto/utils |
| `zerotrie` | 0.2.4 | Unicode-3.0 | https://github.com/unicode-org/icu4x |
| `zerovec` | 0.11.6 | Unicode-3.0 | https://github.com/unicode-org/icu4x |
| `zerovec-derive` | 0.11.3 | Unicode-3.0 | https://github.com/unicode-org/icu4x |
| `zip` | 7.2.0 | MIT | https://github.com/zip-rs/zip2.git |
| `zip` | 8.6.0 | MIT | https://github.com/zip-rs/zip2 |
| `zlib-rs` | 0.6.3 | Zlib | https://github.com/trifectatechfoundation/zlib-rs |
| `zmij` | 1.0.21 | MIT | https://github.com/dtolnay/zmij |
| `zopfli` | 0.8.3 | Apache-2.0 | https://github.com/zopfli-rs/zopfli |
| `zstd` | 0.13.3 | MIT | https://github.com/gyscos/zstd-rs |
| `zstd-safe` | 7.2.4 | MIT OR Apache-2.0 | https://github.com/gyscos/zstd-rs |
| `zstd-sys` | 2.0.16+zstd.1.5.7 | MIT/Apache-2.0 | https://github.com/gyscos/zstd-rs |

