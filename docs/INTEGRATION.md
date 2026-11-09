# Integrating Hematite into a launcher

This document states the contract Hematite must satisfy. It is written from the launcher
side because the renderer side is still incomplete: `libhematite.so` exports eight
`hematite_*` bring-up functions and none of the `gl*`/`egl*` entry points below, so no
launcher can load it as a renderer yet — see `STATUS_REPORT.md`.

Everything here was read from source; the file it came from is named so it can be re-checked.

---

## 1. What the host does

Pojav-derived launchers prepare a renderer through `MojoExec`, then let LWJGL bind OpenGL to
the *same* `dlopen` handle.

`MojoLauncher/mojoexec`, `mojoexec.c`:

```c
Java_git_artdeell_mojoexec_MojoExec_prepareEgl(JNIEnv*, jclass, jstring egl_path,
                                              jboolean use_bypass, jboolean use_gles,
                                              jint gles_version) {
    save_jvm_string(env, &native_egl_path, egl_path);
    if (use_bypass && !get_primary_namespace()) return false;
    egl_use_bypass = use_bypass;
    void* preload_handle = mojoexec_acq_egl_handle();   /* dlopen or nsbypass dlopen */
    if (!preload_handle) { printf("MojoExec: Failed to load EGL: %s\n", dlerror()); return false; }
    mojoexec_renderspec.force_gles_context    = use_gles;
    mojoexec_renderspec.override_major_version = gles_version;
    return true;
}
```

`Aerix-launcher`, `app_pojavlauncher/src/main/jni/jvm_hooks/lwjgl_dlopen_hook.c` replaces
`org.lwjgl.system.linux.DynamicLinkLoader.ndlopen` so that:

- a request for `libGLMojo.so` returns `mojoexec_acq_egl_handle()` — **the renderer's handle**
- a request for `libvulkan.so` returns the host's Vulkan handle
- anything else goes to plain `dlopen`

**So:** LWJGL resolves `gl*` from Hematite's shared object. Hematite must therefore export
the GL entry points, not merely the EGL ones.

## 2. What Hematite must provide

### 2.1 Exported symbols

| Group | Requirement |
| --- | --- |
| EGL 1.4/1.5 | `eglGetDisplay`, `eglInitialize`, `eglChooseConfig`, `eglCreateContext`, `eglMakeCurrent`, `eglCreateWindowSurface`, `eglSwapBuffers`, `eglSwapInterval`, `eglDestroySurface`, `eglQueryString`, `eglQuerySurface`, `eglGetProcAddress`, plus the 1.5 sync entry points |
| GL 1.0–4.6 | Every name in `abi/expected_symbols.txt` (866 entries). A **missing** symbol is not a soft failure: LWJGL 3 `dlsym`s by name and will call the result |
| `eglGetProcAddress` | Must resolve every extension Hematite advertises, and must return `NULL` for names it does not implement (that is legal and expected) |

Unimplemented GL entry points must still **exist** and raise `GL_INVALID_OPERATION` (logged
once) rather than being absent. That decision is recorded in `DECISIONS.md` D-002.

### 2.2 EGL configuration expectations

- An `ES 3.x` context is requested by the host (`force_gles_context`, `override_major_version`).
  Hematite presents **desktop GL 3.2+** on top of it through its own GL entry points, so the
  ES version is an internal detail of which driver Hematite drives.
- `eglCreateWindowSurface` receives the Android `Surface` as an `ANativeWindow*` in the
  `native_window` argument. **[UNVERIFIED]** — inferred from standard Android EGL usage;
  needs a device to confirm, per `RESEARCH.md` A-3.
- `eglSwapBuffers` must present *and* return promptly; frame pacing belongs to the pacer, not
  to a sleep inside swap.

### 2.3 Environment variables

Hematite accepts the launcher-set variables in `CONFIG.md` §"Variables Hematite reads but
does not own". The gl4es-specific ones (`LIBGL_USE_MC_COLOR`, `LIBGL_NORMALIZE`,
`LIBGL_NOINTOVLHACK`, `LIBGL_NOERROR`) are **accepted and ignored**, with one `info` log
line saying so.

## 3. The two ways a host can load Hematite

### 3.1 Built-in renderer spec (Zalith-style, `RenderSpec`)

The host ships a Java/Kotlin class equivalent to `EmesisRenderSpec`. What it must do:

```java
String LIBRARY = "libhematite_renderer.so";           // one library, all of EGL and GL

boolean available() {                                  // every ABI it ships must be present
    return new File(Tools.NATIVE_LIB_DIR, LIBRARY).isFile();
}

String glLibraryPath() {                               // LWJGL binds GL to the same handle
    return preloaded ? LIBRARY : new File(Tools.NATIVE_LIB_DIR, LIBRARY).getAbsolutePath();
}

void setupEnvironment(Context ctx, Map<String,String> env) {
    env.put("POJAV_RENDERER", "opengles3");
    env.put("POJAVEXEC_EGL", LIBRARY);
    env.put("LIBGL_GL", "31");        // desktop GL version Hematite should advertise
    env.put("LIBGL_ES", "3");
}

boolean setupRenderer() {
    System.load(libraryPath);          // so the handle LWJGL gets is the loaded copy
    return MojoExec.prepareEgl(libraryPath, /*useBypass=*/false, /*useGles=*/true, /*glesVersion=*/3)
        || MojoExec.prepareEgl(libraryPath, /*useBypass=*/true,  /*useGles=*/true, /*glesVersion=*/3);
}
```

The critical detail, from `EmesisRenderSpec.java`: after `System.load`, LWJGL must be handed
the **bare soname** (`libhematite_renderer.so`), not the absolute path. The linker then
resolves it to the copy already in the process, so the EGL context and the GL calls above it
are the same library state. Handing LWJGL a path opens a *second* mapping and the EGL state
is not shared.

### 3.2 External plugin APK (Zalith 2 / FCL / Aerix discovery)

The host discovers the APK by manifest metadata and parses a JSON block; the rules are
enforced in `aerix/bridge/RendererPluginV2.kt`:

| Field | Constraint |
| --- | --- |
| `rendererId` | matches `[A-Za-z0-9_.-]{1,128}` |
| `displayName` | non-blank, ≤ 160 chars |
| `rendererGLPath` | must match `lib[A-Za-z0-9_.+-]+\.so` and exist in the plugin's native dir |
| `rendererEGLPath` | same, except the literal `libEGL.so`, which means "the system EGL" |
| `dlopenLibPaths` | **must be empty** — this adapter rejects plugins that need preloading |
| `env[]` | ≤ 128 entries, unique keys matching `[A-Za-z_][A-Za-z0-9_]*` |
| whole JSON | ≤ 64 KiB |

Accepted host metadata markers:
`fclPlugin`, `fclPlugin_V2`, `zalithRendererPlugin`, `pojavRendererPlugin`, `aerixPlugin`,
`emesisPlugin`.

**Design consequence (D-001):** Hematite is one `.so` per ABI and loads nothing else at
startup. That is the only shape valid under *both* mechanisms, because the V2 path forbids
`dlopenLibPaths` and the `RenderSpec` path expects a single library to answer for GL.

ABI checks are real: the host reads the ELF header and compares class and `e_machine`
(183 = AArch64, 40 = ARM, 62 = x86-64, 3 = x86). A wrong-ABI `.so` is rejected before load.

## 4. Troubleshooting checklist

Ordered so each step rules out the next. None of these has been exercised, because nothing
has been built.

1. **Does the library exist for this ABI?** `unzip -l hematite.apk | grep libhematite`.
2. **Are the symbols exported?** `nm -D libhematite_renderer.so | grep -c ' T gl'` should be
   in the hundreds, not single digits.
3. **Did the host load it?** Logcat should contain `MojoExec: Loaded EGL …`.
4. **Did LWJGL bind GL to it?** Look for the `LWJGL linkerhook: replacing OpenGL with
   renderspec driver` line (Aerix) or equivalent.
5. **Did context creation succeed?** A `GL_INVALID_OPERATION` storm at startup means the
   renderer loaded but rejected the config — check the EGL config attributes the host asked
   for against what Hematite advertises.
6. **Is it the second mapping problem?** If contexts work but all GL state is ignored, the
   host handed LWJGL a path instead of a soname. §3.1's `glLibraryPath()` is the fix.
7. **Is it a driver problem?** Set `HEMATITE_BACKEND=gles` and relaunch. If GLES works and
   Vulkan does not, the next step is a capability-profile dump, not more code.

## 5. What is not covered here

- **FCL's own plugin schema.** Assumed equivalent to Zalith's (RESEARCH.md A-5); not read.
- **`pojavexec`'s window plumbing.** The submodule was not read; the `ANativeWindow*`
  assumption is therefore unverified.
- **Any launcher not derived from Pojav.** Out of scope by construction.
