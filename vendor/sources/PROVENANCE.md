# Vendored crate sources

Materialised by `tools/setup_vendor_sources.py`; each tarball is SHA-256 pinned
there. Manifest sections that cargo does not need to build the crate
(`[dev-dependencies]`, benches, tests, examples, bins, `[workspace]`, `[patch]`,
`[profile.*]`) are stripped so that no un-vendored crate is referenced.

| crate | version | upstream | ref | subdirectory |
| --- | --- | --- | --- | --- |
| naga | 0.14.0 | https://github.com/gfx-rs/naga | `v0.14.0` | . |
| pp-rs | 0.2.1 | https://github.com/Kangz/glslpp-rs | `v0.2.1` | . |
| unicode-xid | 0.2.6 | https://github.com/unicode-rs/unicode-xid | `v0.2.6` | . |
| spirv | 0.2.0+1.5.4 | https://github.com/gfx-rs/rspirv | `spirv-0.2.0%2B1.5.4` | spirv |
| bitflags | 2.4.1 | https://github.com/bitflags/bitflags | `2.4.1` | . |
| bitflags | 1.3.2 | https://github.com/bitflags/bitflags | `1.3.2` | . |
| bit-set | 0.5.3 | https://github.com/contain-rs/bit-set | `v0.5.3` | . |
| bit-vec | 0.6.3 | https://github.com/contain-rs/bit-vec | `d80f4a1c0a66a0aae77a75eb379c99266eababa1` | . |
| rustc-hash | 1.2.0 | https://github.com/rust-lang/rustc-hash | `v1.2.0` | . |
| indexmap | 2.2.6 | https://github.com/indexmap-rs/indexmap | `2.2.6` | . |
| hashbrown | 0.14.5 | https://github.com/rust-lang/hashbrown | `v0.14.5` | . |
| equivalent | 1.0.1 | https://github.com/indexmap-rs/equivalent | `v1.0.1` | . |
| log | 0.4.20 | https://github.com/rust-lang/log | `0.4.20` | . |
| num-traits | 0.2.17 | https://github.com/rust-num/num-traits | `num-traits-0.2.17` | . |
| autocfg | 1.1.0 | https://github.com/cuviper/autocfg | `1.1.0` | . |
| thiserror | 1.0.69 | https://github.com/dtolnay/thiserror | `1.0.69` | . |
| thiserror-impl | 1.0.69 | https://github.com/dtolnay/thiserror | `1.0.69` | impl |
| proc-macro2 | 1.0.92 | https://github.com/dtolnay/proc-macro2 | `1.0.92` | . |
| quote | 1.0.37 | https://github.com/dtolnay/quote | `1.0.37` | . |
| syn | 2.0.90 | https://github.com/dtolnay/syn | `2.0.90` | . |
| unicode-ident | 1.0.14 | https://github.com/dtolnay/unicode-ident | `1.0.14` | . |

naga and pp-rs are MIT OR Apache-2.0; the licences travel with each crate's
sources. See `docs/VENDORED.md` for why vendoring is used at all.
