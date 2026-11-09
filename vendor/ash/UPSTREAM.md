# Vendored ash

Source: https://github.com/ash-rs/ash
Revision: b724b78dac8d83879ed7a1aad2b91bb9f2beb5cf (tag 0.38.0)
Version: 0.38.0+1.3.281
License: MIT OR Apache-2.0; upstream license texts included.

`src/` is copied unchanged. Cargo packaging is reduced to a dependency-free path
crate because crates.io is unreachable in this environment. Hematite uses
`Entry::from_static_fn` and owns dynamic-library loading; the optional upstream
libloading dependency is not used. The upstream `loaded` feature is intentionally
unavailable (do not enable it). `std` and `debug` retain upstream behaviour.
No Vulkan driver, shader pack or game code is vendored here.
