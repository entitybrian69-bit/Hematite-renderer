# Hematite Renderer — Decisions

Every entry: **decision**, **alternatives considered**, **reason**. Where the spec was
ambiguous, the choice is the one that best serves raw performance and shader-pack support,
per §1 of the brief. Entries marked *(revisit)* have a known cost that should be re-examined
with real device data.

D-001 through D-015 describe the planned renderer, not completed implementation. Later
entries carry explicit verification scope; STATUS_REPORT.md is authoritative for what exists.

---

## D-001 — One shared object per ABI, containing frontend + backend

**Decision.** Ship `libhematite_renderer.so` per ABI. It exports EGL, the GL entry points
and `eglGetProcAddress`; the chosen backend is statically linked in.

**Alternatives.** (a) Separate `libhematite_egl.so` + `libhematite_vk.so` with a
`dlopen` between them. (b) Lean on `dlopenLibPaths` in the V2 plugin metadata.

**Reason.** `RendererPluginV2.kt` explicitly rejects any plugin whose `dlopenLibPaths` is
non-empty ("This V2 plugin requires preloading, which this adapter does not yet support"),
and `nativeLibrary()` refuses paths outside the plugin's native dir. Meanwhile the built-in
`RenderSpec` path (`EmesisRenderSpec`) also preloads a single library and expects the same
handle to answer for GL. One `.so` is the only shape that satisfies **both** host paths, and
it also avoids a second mapping of GL state. Cost: bigger binary, one relocation set.

## D-002 — Python-free, C-ABI-stable export table, generated not hand-written

**Decision.** Generate the export table from a checked-in spec file (`abi/gl_export.def`)
into Rust, with an explicit `extern "C"` fn per entry point. Not a single varargs dispatcher.

**Alternatives.** (a) One `glCall(name, ...)` trampoline. (b) Hand-written per-function
modules.

**Reason.** LWJGL 3 `dlsym`s exact symbol names; a trampoline does not provide them. Hand-
writing ~1200 entry points invites signature drift — the brief's §15 demands each be checked
against real GL prototypes. A generator with one signature table gives a single place to
audit types (`GLsizeiptr` = `isize` on 64-bit, `i32` on ILP32; `GLboolean` = `u8`) and lets
the build emit an `nm -D` manifest that CI diffs against the expected symbol list.

## D-003 — Backend boundary is a data IR, not a trait call per GL function

**Decision.** Record `Command` values; backends consume slices.

**Alternatives.** `trait Backend` with a method per GL call, called inline.

**Reason.** Draw merging, render-pass merging, and skip-a-frame-when-pipeline-not-ready all
need lookahead and sequence context. Also keeps translation off the render thread.
See `ARCHITECTURE.md` §2. Cost: an extra copy into the command ring; mitigated by keeping
`Command` small (largest variant is a draw descriptor, ≤ 64 bytes) and moving bulk payloads
(vertex data) to a separate ring.

## D-004 — GLSL translation: glslang primary, naga as a second opinion, not a shunt

**Decision.** Preprocess/rewrite legacy GLSL ourselves, then compile to SPIR-V with glslang.
Run naga as a *validation and fallback* translator when glslang fails or when the backend is
GLES (naga can emit ESSL-ish and handles some constructs differently).

**Alternatives.** (a) naga only. (b) glslang only. (c) Write a GLSL front-end.

**Reason.** glslang is the reference implementation for "does this compile as Vulkan GLSL"
and produces diagnostics pack authors recognise; naga has better luck with some malformed-
but-accepted-by-drivers input and has no C++ build requirement. A fallback chain with a
recorded outcome per shader is cheaper than either alone. **(revisit)** — the brief requires
evidence for this choice; the evidence must come from running a shader corpus, which needs a
working toolchain. Until then this is unproven and is labelled as such in `STATUS_REPORT.md`.

## D-005 — Fixed-function variants keyed by hash of the state word, not by string

**Decision.** Build a 64-bit `ff_key` from the fields that actually affect generated code
(lighting on/off, light count, two-sided, fog mode, texture env mode per unit, texgen mode,
alpha test, clip plane count, vertex format flags). Resolve through an open-addressed table of
`(ff_key → program)`. Shade with specialization constants for the *cheap* varying fields.

**Alternatives.** Canonical-GLSL-text + string map (what GL4ES does), or a separate variant
per state permutation.

**Reason.** String formation and hashing on the render path shows up in profiles of legacy
MC; a u64 key with a good mixer is a single multiply-xor chain. Specialization constants keep
the variant count from exploding (e.g. fog distance scaling is a push constant, not a key
bit). **(revisit)** — needs measurement on legacy MC before it is claimed faster.

## D-006 — Display lists are baked into immutable command + vertex buffers

**Decision.** On `glEndList`, replay the recorded list once into a compact form: matrices
folded, static geometry concatenated into one device-local vertex and index buffer, state
changes deduplicated, and a linear op list retained for the non-geometry operations. On
`glCallList`, emit the retained ops and one draw per merged geometry block.

**Alternatives.** Record and replay GL calls verbatim. Compile to a display-list-specific
mini-backend.

**Reason.** Legacy Minecraft builds terrain from display lists; verbatim replay repeats all
per-vertex state work every frame. Baking is the single biggest lever for 1.12-and-below.
Nested lists and `glCallLists` with a base are handled by reference, not by inlining, to keep
`glNewList` inside a list legal and cheap.

## D-007 — Deferred pass begin, lazy end, and merge on FBO bind

**Decision.** Do not begin a render pass when `glBindFramebuffer` is called; begin on the
first draw, dispatch or clear that needs it. End it when something incompatible arrives
(different attachment set, present, readback, compute). When a new FBO is bound whose
attachment set is identical, keep the current pass open.

**Alternatives.** Begin on bind, end on unbind (simple, matches GL semantics literally).

**Reason.** Tile-based GPUs pay for every pass; Minecraft binds and unbinds FBOs far more
often than it actually needs a break. Attachment load/store ops are then chosen from real
usage: `CLEAR` only where the first access is a write and no earlier read exists;
`DONT_CARE` where the attachment is never read or was invalidated. This is also what makes
transient (lazily allocated) MSAA and depth attachments safe.

## D-008 — Pipeline policy: skip, never block, and pre-warm from a manifest

**Decision.** Default async-compile policy is **skip the draw** for the frames until the
pipeline is ready (max 3 frames), then force-create synchronously on the 4th so a draw can
never disappear forever. Every successfully created pipeline is appended to a per-device,
per-MC-version, per-pack manifest; on the next launch those are created during loading.

**Alternatives.** Block (visible hitch), or fallback pipeline (wrong output for a frame).

**Reason.** The brief's priority is stutter-free handling; a skipped draw for ≤3 frames at a
first encounter is less user-visible than a 200 ms stall, and the manifest makes the *second*
launch clean. The bounded-skip rule guarantees progress.

## D-009 — Descriptors: per-frame bump pool + layout cache, indexing when present

**Decision.** Descriptor set layouts are canonicalised and cached by a hash of their binding
signature. Sets are allocated from a per-frame, per-thread bump allocator that is reset on
fence completion. With `VK_EXT_descriptor_indexing` + `PARTIALLY_BOUND`, textures are bound
through one large array, which removes most per-draw set writes.

**Alternatives.** Push descriptors everywhere; `VK_EXT_descriptor_buffer`.

**Reason.** Only some devices have push descriptors, and descriptor buffers are rare on
Android today. The two-tier design gives a portable fast path plus a faster path where
indexing exists. No per-draw `vkAllocateDescriptorSets`, ever.

## D-010 — Backend selection is capability-and-quirks driven, with an explicit override

**Decision.** Choose Vulkan when the device exposes the required core feature set *and* no
quirk entry marks the driver unhealthy; otherwise GLES 3.x. `HEMATITE_BACKEND` overrides.
Switching requires a relaunch (the `.so` is already mapped by then).

**Reason.** A runtime switch would mean two live backends and doubled state tracking for no
user benefit; a relaunch is what every launcher already does between profiles.

## D-011 — BC/DXT textures are transcoded, and S3TC is not falsely advertised

**Decision.** Do not advertise `GL_EXT_texture_compression_s3tc` unless the driver genuinely
decodes BC. Transcode BC1/2/3 → ASTC (if supported) or RGBA8 on worker threads at upload.

**Reason.** Advertising S3TC and then returning garbage breaks resource packs silently; the
brief requires graceful degradation with a log, and silent black textures are the worst
failure mode. Transcoding cost is paid once per texture and is cacheable.

## D-012 — Vertex data from client arrays is copied, and cached by content identity

**Decision.** For non-VBO draws, upload the accessed range into the ring once and key the
cache on `(pointer, stride, count, first, a cheap fingerprint of the first and last element)`.

**Alternatives.** Always re-upload (simple, costly); never cache (correct, slower).

**Reason.** Legacy MC redraws the same client array many times per frame in some paths. The
fingerprint makes re-use safe enough that a wrong hit is a torn frame, not a crash — but it
is still a heuristic, so it is documented as a correctness/performance trade with a strict
mode that disables it (D-013).

## D-013 — "Fast" and "strict" validation are runtime modes, fast is default

**Decision.** Fast mode: skip redundant-error checks, skip format-compatibility validation on
bind, skip `glGetError` bookkeeping unless the game asks. Strict mode: full validation,
which turns every mistake into a real GL error.

**Reason.** The brief ranks hot-path speed first but requires a safe path for debugging. The
concrete rule: anything that can *crash* (null deref, OOB, division by zero) is always
checked; anything that can only produce a *wrong pixel* is checked only in strict mode.

## D-014 — Configuration: environment first, file second, one precedence order

**Decision.** Resolution order for a key: explicit env var (`HEMATITE_*`) > config file >
built-in default. Launcher-set legacy variables (`LIBGL_*`, `POJAVEXEC_EGL`, `POJAV_RENDERER`)
are *read and honoured where meaningful* but never override `HEMATITE_*`.

**Reason.** Launchers can only set env vars, users can only edit files, and the two must not
fight. A single documented order (CONFIG.md) beats per-key special cases.

## D-015 — No fast-path allocation after warm-up

**Decision.** The render path performs zero heap allocations once the first few frames have
warmed the pools. Rings are pre-sized and grow only at frame boundaries; state tables are
fixed-capacity with open addressing.

**Reason.** Android allocators are slow and the render thread is the wrong place to find out.
Enforced by a debug-only allocation counter that panics if it ticks inside a frame.

---

## Decisions deliberately deferred

| Topic | Why deferred |
| --- | --- |
| Swappy vs. own frame pacer | Swappy needs a native dependency and a working build; an own impl needs a device to validate. The interface is designed (`architecture` §6/pacing) but not chosen. |
| Geometry/tessellation emulation strategy | Depends on measured device feature coverage; emulation cost is unknown. |
| Transform feedback emulation via SSBO | Legal but changes semantics for `GL_RASTERIZER_DISCARD`; needs a corpus test to validate. |
| FSR1-style upscaler vs. simple bilinear | Only worth it if render-scale is actually used; needs a device to tune sharpening. |

## D-016 — Real compiler checks supersede syntax-only validation

**Decision.** Pin native Rust 1.85.0 (including CI), and run Cargo check, Clippy with
`-D warnings`, rustfmt and tests through `./build.sh check`. In this sandbox use the
SHA-256-pinned bundled npm toolchain when official distribution hosts are blocked. Keep
the independently working WASI-hosted compiler as a fallback, not as Android verification.

**Alternatives.** Keep using syntax-only checks; rely on a future CI run; repeatedly retry
blocked Rust distribution servers; use the original 1.75 pin without testing that compiler.

**Reason.** Actual compilation already found invalid derives/imports. Native tools run all
mandated CPU checks and make the build loop practical. Registry repackaging is a supply-chain
risk documented in ENVIRONMENT.md; official distributions remain preferred outside this
restricted sandbox. The WASI compiler is 1.83.0-dev and compiles sources directly without
Cargo/MSRV enforcement; that is secondary target coverage, not the pinned native build.

**Status.** VERIFIED for native CPU checks and WASI tests; Android targets remain absent.

## D-017 — ABI declarations are not implementations

**Decision.** Emit genuine `extern "C"` foreign declarations, not functions whose bodies
panic. Keep the symbol manifest as a planned ABI contract; do not ship a library of panic
stubs or an APK that only looks like a renderer. Remove the CI job that attempted to build
a nonexistent frontend and replace it with real generated-declaration compilation.

**Alternatives.** Export 866 panic functions to satisfy `nm`; return fake success/defaults;
leave a permanently failing job for a crate that does not exist.

**Reason.** False-success APIs and FFI aborts make shader debugging and device diagnosis
worse. A correct signature contract is useful, but does not prove implementation,
Khronos conformance, symbol exports or shader support. All missing pieces are explicit in
KNOWN_LIMITATIONS.md. Android artifacts/releases wait for real renderer implementations.

**Status.** VERIFIED for declaration generation, pointer-qualifier regressions and native /
WASI compilation. Runtime GL/EGL implementation is NOT-IMPLEMENTED.

## D-018 — Preserve legacy vertex data; bound CPU capture

**Decision.** Store object-space double-precision homogeneous positions and current
attributes; leave transformation/lighting to future GPU shader variants. Use column-major,
post-multiplied GL matrices and explicit stack/capture capacities. Expand legacy smooth
geometry to indexed lists with preserved winding, without silently dropping capacity errors.

**Alternatives.** Transform/light every vertex on the CPU; grow buffers in each begin/end;
claim flat-shading support without implementing provoking-vertex rules.

**Reason.** Retaining object coordinates supports shader compatibility and avoids repeated
CPU vertex math. Preallocation bounds render-thread allocation and protects memory. Correct
strip winding and incomplete-primitive handling are tested before any backend integration.
No performance claim follows from these design arguments; GPU upload, flat shading,
lighting, display lists and shader variants are still NOT-IMPLEMENTED.

**Status.** VERIFIED for CPU matrices, bounded capture and smooth topology conversion only.

## D-019 — Cache identity is complete-field content identity, corruption is a miss

**Decision.** Use domain-separated SHA-256 with length-prefixed, ordered fields; versioned
records include key, payload length and CRC32. App-private disk writes use create-new temp
files, sync, then same-directory rename. Bound individual payload reads before loading.

**Alternatives.** Unstable `DefaultHasher` keys; concatenate source/options without lengths;
write directly over live entries; treat corrupt cache data as a valid shader.

**Reason.** Warm shader/pipeline reuse is central to stutter reduction, but stale or truncated
records must not reach the driver. Compiler/options/device fields belong in the key; CRC32
catches corruption, not hostile modification. Standard-library-only implementation remains
buildable here. Published SHA-256/CRC32 vectors and record/IO regression tests provide actual
evidence; driver cache acceptance and performance require later real integration.

**Status.** VERIFIED for record identity/integrity and disk IO on Linux + WASI. Quota,
eviction, crash-temp cleanup, shader compilation and device-header validation remain
NOT-IMPLEMENTED. Atomic visibility is not a power-loss durability guarantee.

## D-020 — Vendor `ash` 0.38.0 instead of writing Vulkan bindings

**Decision.** Vendor `ash` at its `0.38.0` tag (commit `b724b78`) in `vendor/ash`, with a
reduced Cargo manifest exposing only the `std` and `debug` features. The optional
`libloading`-based `loaded` feature is deliberately absent: Hematite loads the loader
itself with a 40-line `dlopen` shim and constructs `Entry::from_static_fn`, so the loader
handle, instance and device share one ownership chain.

**Alternatives.** Wait for crates.io access (blocks all Vulkan work); write raw `vk` FFI by
hand (re-implements ~5 MB of generated bindings); use `vulkano`/`wgpu` (higher-level, larger,
also unreachable).

**Reason.** `ash` is thin over the C ABI, so a real driver exercises real Vulkan rather than
an abstraction nobody has tested here. Vendoring keeps the workspace buildable with the
registry blocked, and `UPSTREAM.md` records revision, license and exactly what was changed.
Upstream's `src/` is unmodified except five `mem::size_of*` qualifications that the current
`unused_qualifications` lint rejects; that edit is recorded in the same file.

**Status.** VERIFIED: builds with `-D warnings`, drives SwiftShader through instance
creation, device selection, pipeline creation, draw and readback.

## D-021 — Verify the backend with analytic pixel references on a software driver

**Decision.** Compare driver readback against a CPU-computed barycentric reference (clear
colour, coverage bounds, per-pixel interpolation, 1×1 and repeated renders) using
SwiftShader, a real Vulkan implementation extracted from a pinned npm package. Tests fail
when the driver is missing rather than substituting expected values.

**Alternatives.** Golden images (records whatever the first run produced, including bugs);
mocked Vulkan calls (tests nothing); deferring all GPU verification to a device that does
not exist here.

**Reason.** An analytic reference is a specification, not a snapshot: it catches a wrong
viewport, a broken layout transition or a stale buffer instead of enshrining it. SwiftShader
is a genuine driver, so this is real Vulkan execution — but it is CPU-based, lenient, and
runs without validation layers, so it cannot reveal hardware-specific or synchronization
defects. That limit is stated wherever the result is quoted.

**Status.** VERIFIED on SwiftShader (reports Vulkan 1.3.0). Hardware and validation layers:
NOT-IMPLEMENTED in this environment.

## D-022 — Link the Android library with Zig's `ld.lld` and generated stub libraries

**Decision.** Cross-compile with the pinned Rust `rust-std` for `aarch64-linux-android` and
link through `tools/android_linker.py`: it collects the symbols the link inputs leave
undefined, generates a stub `.so` per Android library defining exactly those symbols, then
drives `zig ld.lld` with the stub directory as the only search path and
`-z max-page-size=16384`.

**Alternatives.** Wait for an NDK (unreachable); use Zig's Android target (the PyPI wheel
ships no bionic, and its driver refuses `...-android*` outright); let `zig cc` resolve
`-lc`/`-lunwind` (verified with `zig cc -v`: it substitutes its musl `libc.so` and static
`libunwind.a`, which embedded musl code and left `__errno_location`, `fprintf`, `dladdr` and
`pthread_rwlock_*` undefined for bionic); build a fully static musl library (avoids bionic
but breaks `dlopen` of `libvulkan.so`).

**Reason.** Stub `.so` files are how the NDK represents system libraries anyway: definitions
for the linker, implementations at load time. Driving `ld.lld` directly keeps every library
decision inside the tool, so `DT_NEEDED` is `libdl.so`, `libunwind.so`, `libc.so` — the same
names an NDK link records — and no foreign libc code is embedded. The tool verifies its own
output (ELF64 AArch64 `ET_DYN`, `DT_NEEDED` subset, 16 KB segments, no unexpected undefined
symbol) and aborts otherwise.

**Status.** VERIFIED structurally; `tools/inspect_library.py` prints all 45 undefined
symbols for review. Execution on Android is NOT-IMPLEMENTED (no device), and the
symbol-to-library table is the one assumption only a device can disprove (E-7).

## D-023 — Write the AXML, ZIP and signature schemes instead of requiring a JDK

**Decision.** `tools/build_apk.py` encodes binary Android XML, assembles a ZIP with the
native library stored uncompressed and 16 KB aligned, and signs with APK Signature Scheme
v1 (JAR, PKCS#7 over `CERT.SF`) and v2 (APK Signing Block). Manifest attribute resource IDs
are copied from AOSP `public.xml`.

**Alternatives.** Ship no APK; ship an unsigned ZIP renamed `.apk`; install a JDK and
Gradle (unreachable).

**Reason.** A container that a device can at least parse and verify is worth more than a
missing artifact, provided every byte is checkable. The v2 layout was taken from AOSP
`apksig` source (`V2SchemeSigner`, `ApkSigningBlockUtils`), and `tools/verify_apk.py`
re-checks the result with two independent implementations: androguard parses the manifest
and reports package/SDK values, apksigtool recomputes the v2 content digest and verifies the
RSA signature (`v2 verified`).

**Status.** VERIFIED by those tools. Android's own verifier was never run; installation,
resource handling and Play requirements are untested. `targetSdkVersion 28` keeps the v1
signature acceptable.

## D-024 — Keep the shipped ABI narrow and honest

**Decision.** `libhematite.so` exports eight `hematite_*` functions (version, status text,
last error, create, destroy, device name, offscreen render, `glOrtho` matrix) and none of
the 866 GL/EGL symbols.

**Alternatives.** Export the 866 names as panicking stubs so `nm -D` matches the manifest;
pretend the library is a renderer plugin.

**Reason.** A launcher that resolves `glDrawArrays` against a panicking stub crashes inside
the game process; that is worse than a library that is visibly not a GL implementation. The
narrow ABI is testable end to end (it was, on x86_64 through `ctypes` against SwiftShader)
and gives the Android build something real to ship while the frontend is written.

**Status.** VERIFIED on host; BUILT-BUT-UNTESTED for arm64. Panics abort in release builds
by design (`panic = "abort"`), so a bug kills the launcher rather than unwinding through
foreign frames; `catch_unwind` guards are live in development and test builds.

---

### D-025 — Export all 866 entry points; generate the thunks, hand-write the subset

**Decision.** `xtask gen-abi` emits a `#[no_mangle]` definition for every entry point in
`abi/gl_export.def` (`crates/hematite-gl-frontend/src/gen/exports.rs`). If `src/api.rs` or
`src/egl.rs` defines the same name, the thunk forwards to it — so the compiler proves the
implementation's signature matches the table. Otherwise the thunk calls
`crate::unsupported`, which raises `GL_INVALID_OPERATION`, increments
`hematite_gl_unsupported_count()` and returns the zero value of the declared return type.

**Why.** LWJGL resolves entry points by name and calls whatever it gets, so a partial
export list turns into a null-pointer call inside the launcher. Exporting everything, with
honest accounting of what is not implemented, lets a host probe capability instead of
guessing. Generation also keeps 866 signatures in exactly one place.

**Alternatives.** Panicking stubs (a launcher probing capabilities would abort the process);
fake success and plausible defaults (silently wrong pixels, the failure mode this project
exists to avoid); hand-writing 866 signatures (unmaintainable, and nothing would check them
against the table).

### D-026 — `glCompileShader` fails loudly instead of pretending

**Decision.** GLSL is not compiled. `glCompileShader` always leaves `GL_COMPILE_STATUS`
false and writes an info log naming the reason, so `glLinkProgram` and `glUseProgram` cannot
activate a shader-driven pipeline, and `glGetUniformLocation`/`glGetAttribLocation` return
-1.

**Why.** A program Hematite cannot honour would produce arbitrary wrong pixels, and the
application would have no way to tell. Failing at compile time is the only honest option
until a translator exists, and it leaves the fixed-function path — which really renders —
usable.

### D-027 — The GL frontend rasterizes on the CPU, not through the Vulkan backend

**Decision.** `hematite-gl-frontend` owns a software rasterizer (clip, perspective-correct
interpolation, depth, blend, scissor, culling) and a software EGL whose `eglSwapBuffers`
publishes the frame to a snapshot; it does not call `hematite-vk-backend`.

**Why.** It makes the GL path verifiable with no driver, no device and no window system —
exactly the constraints of this environment — and it keeps the Vulkan path free to grow its
own texture/depth/pipeline work without a half-finished bridge. Routing GL through Vulkan is
tracked as ROADMAP step 2.2b, with the same `./build.sh gl` checks as the exit criterion.

**Alternatives.** Bridging GL to Vulkan now (a large surface, unverifiable here); routing GL
to a GLES driver (none is available in this environment).
