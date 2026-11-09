# Compatibility

## Every row is NOT-IMPLEMENTED

CPU library builds and unit tests exist, and an offscreen Vulkan renderer plus a signed APK
now exist too — but the library exports no GL entry points, so no Minecraft version has ever
been launched through Hematite and nothing below can be tested end to end. Publishing a
table of green ticks here would be fabrication, and the brief explicitly forbids marking
anything VERIFIED without an actual test — for which there is still no device.

So the table below records *what must be tested*, and what each target will exercise. When a
row is verified, it gains a device, a Minecraft version, a log line or screenshot, and a
date. Rows are never updated by reasoning.

Status values: **VERIFIED** · **PARTIAL** · **FAILING** · **NOT-IMPLEMENTED** · **NOT-TESTED**

---

## 1. Minecraft versions

| Version range | Loader / API | Status | What must be proven |
| --- | --- | --- | --- |
| Alpha, Beta, 1.0–1.5 | LWJGL 2, fixed function | NOT-IMPLEMENTED | immediate mode, display lists, no shaders |
| 1.6–1.8.9 | LWJGL 2, fixed function + GL 2.1 | NOT-IMPLEMENTED | client arrays, `GL_EXT_texture_env_combine`, texture atlases |
| 1.9–1.12.2 | LWJGL 2, fixed function | NOT-IMPLEMENTED | the version most OptiFine legacy shader users run |
| 1.13–1.16.5 | LWJGL 3, core-ish profile | NOT-IMPLEMENTED | the LWJGL 2→3 boundary; VAOs become mandatory |
| 1.17–1.20.x | LWJGL 3, core 3.2/3.3 | NOT-IMPLEMENTED | the main modern target |
| 1.21.x | LWJGL 3, core 3.3 | NOT-IMPLEMENTED | current version at time of writing |
| Snapshots / future | LWJGL 3 | NOT-IMPLEMENTED | forward-compatibility is a design goal, not a claim |

## 2. Mods and loaders

| Target | Requirement in GL terms | Status | Note |
| --- | --- | --- | --- |
| Vanilla (any version) | varies by version | NOT-IMPLEMENTED | baseline; must pass first |
| Forge | none of its own | NOT-IMPLEMENTED | mods add requirements unpredictably |
| NeoForge | none of its own | NOT-IMPLEMENTED | |
| Fabric | none of its own | NOT-IMPLEMENTED | |
| Quilt | none of its own | NOT-IMPLEMENTED | |
| OptiFine (legacy, 1.7–1.12) | fixed-function interception + `glProgram*`, custom uniforms | NOT-IMPLEMENTED | hardest legacy target |
| OptiFine (modern) | GL 3.2+ programs | NOT-IMPLEMENTED | |
| Sodium / Embeddium / Rubidium | `buffer_storage`, `multi_draw_indirect`, `base_instance`, instancing, UBO rings | NOT-IMPLEMENTED | exact minimum version per Sodium release is **[UNVERIFIED]** |
| Iris / Oculus | GL 4.3 features on a 3.3 context: compute, SSBO, image load/store, `texture_barrier`, 8 MRT, shadow depth-compare samplers | NOT-IMPLEMENTED | the single biggest feature risk in the project |

## 3. Shader packs

The brief lists BSL, Complementary, SEUS-family, Rethinking Voxels, Photon and MakeUp Ultra
Fast as targets. **None has been run, and none of their source has been redistributed.**

Planned approach, per the brief: synthetic shaders that exercise the same *feature
categories* (MRT ping-pong, shadow maps with depth compare, compute passes, noise volumes,
custom uniform parsing) are written from scratch and tested. That avoids both copyright
problems and the false confidence of "it compiled".

| Pack family | Feature categories it exercises | Status |
| --- | --- | --- |
| BSL | deferred MRT, shadow map, volumetric light (noise volume) | NOT-TESTED |
| Complementary | compute, SSBO, image load/store, complex bloom chain | NOT-TESTED |
| SEUS-family | shadow + water (refraction/reflection, `texture_barrier`) | NOT-TESTED |
| Rethinking Voxels | voxel tracing in compute, many buffers | NOT-TESTED |
| Photon | heavy deferred, many custom uniforms | NOT-TESTED |
| MakeUp Ultra Fast | lightweight, good early smoke test | NOT-TESTED |

## 4. Launchers

| Launcher | Load mechanism | Status |
| --- | --- | --- |
| Aerix Launcher | built-in `RenderSpec` *and* V2 plugin discovery | NOT-IMPLEMENTED |
| ZalithLauncher / ZL2 | V2 plugin APK | NOT-IMPLEMENTED |
| FCL | `fclPlugin_V2` (schema assumed equal — RESEARCH.md A-5) | NOT-IMPLEMENTED |
| PojavLauncher (upstream) | built-in `RenderSpec` only | NOT-IMPLEMENTED |

## 5. Devices and GPUs

No device has been used. The vendors that matter and the classes of problem expected are in
`RESEARCH.md` §5; none of those expectations is evidence.

| Vendor | Family | Status | Note |
| --- | --- | --- | --- |
| Qualcomm | Adreno | NOT-TESTED | tilers; the largest Android population |
| Arm | Mali | NOT-TESTED | most sensitive to attachment load/store |
| Imagination | PowerVR | NOT-TESTED | depth/stencil feedback hazards |
| Samsung | Xclipse | NOT-TESTED | closest to desktop behaviour |

## 6. How a row gets updated

1. Run the specific test: a named Minecraft version plus a named loader/mod, on a named
   device and driver build.
2. Capture evidence: logcat excerpt, `GL_RENDERER`/`GL_VERSION` as reported, screenshot,
   and the Hematite build hash.
3. Set the status and paste the evidence into the row, or link to a file under
   `docs/compat/<target>/`.
4. If it is broken, leave it broken and record the symptom precisely. A "FAILING" row with a
   good description is worth more than a "PARTIAL" row with a guess.
