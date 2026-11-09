# The renderer-plugin contract Hematite's APK must satisfy

Everything here was read from launcher source on GitHub, not guessed. Hematite ships as an
FCL-style **v1 renderer plugin**: a signed APK containing one native library plus
`<application>` meta-data. There is no Java or Kotlin code in the APK, and none is needed —
but there **is** a declared activity, for a non-obvious reason explained below.

Verified launcher source (branch `HEAD` as of 2026-11-09), repo
[`ZalithLauncher/ZalithLauncher2`](https://github.com/ZalithLauncher/ZalithLauncher2), paths
relative to `ZalithLauncher/src/main/java/com/movtery/zalithlauncher/`:

| File | Lines | What it says |
| --- | --- | --- |
| `game/plugin/PluginLoader.kt` | 38-39 | `PACKAGE_FLAGS = GET_META_DATA or GET_SHARED_LIBRARY_FILES` |
| `game/plugin/PluginLoader.kt` | 60-65 | discovery: `queryIntentActivities(Intent(Intent.ACTION_MAIN), PACKAGE_FLAGS).forEach { parseApkPlugin(context, resolve.activityInfo.applicationInfo) }` |
| `game/plugin/renderer/RendererPluginManager.kt` | 88-89 | enabled by `metaData.getBoolean("fclPlugin", false) \|\| metaData.getBoolean("zalithRendererPlugin", false)` |
| `game/plugin/renderer/RendererPluginManager.kt` | 102 | `metaData.getString("renderer")`, split on `:` into `rendererId : glName : eglName` |
| `game/plugin/renderer/RendererPluginManager.kt` | 103-104 | `des` = display name; `pojavEnv` = `KEY=VALUE` pairs joined by `:` |
| `game/plugin/renderer/RendererPluginManager.kt` | 117-123 | `DLOPEN=a,b` extra libraries; `LIB_MESA_NAME`/`MESA_LIBRARY` values are prefixed with `nativeLibraryDir` |
| `game/plugin/renderer/RendererPluginManager.kt` | 134-145 | optional `minMCVer`/`maxMCVer`; a configurable package allow-list |
| `game/plugin/renderer/RendererPluginManager.kt` | 162-164 | `progressEglName`: a value starting with `/` is used as an absolute path, otherwise the bare name (resolved through the library search path) |
| `game/plugin/renderer/RendererPlugin.kt` | — | `getRendererLibrary() = "$nativeLibraryDir/$glName"`, `getRendererEGL() = eglName`, `getDlopenLibrary() = "$nativeLibraryDir/$lib"` |
| `game/launch/GameLauncher.kt` | 212-225 | `dlopenEngine()`: `ZLBridge.dlopen(libPath)` for each dlopen library, then the renderer library |
| `game/launch/GameLauncher.kt` | 233 | JVM argument `-Dorg.lwjgl.opengl.libname=<rendererLibrary>` |
| `game/launch/GameLauncher.kt` | 451-470 | exports `SDL_OPENGL_LIBRARY=rendererId`, the `pojavEnv` pairs, `POJAVEXEC_EGL=eglName` |
| `game/launch/GameLauncher.kt` | 485 | `SDL_EGL_LIBRARY` is set to the resolved EGL path **only if `File(eglPath).isFile()`** |
| `game/launch/GameLauncher.kt` | 489 | `POJAV_RENDERER=rendererId` |

The FCL origin of the same contract is
`FCL-Team/FoldCraftLauncher` `FCL/src/main/java/com/mio/plugin/PluginManager.kt` line 36
(`META_PLUGIN = "fclPlugin"`), line 39 (`META_PLUGIN_V2 = "fclPlugin_V2"`), line 140 (the same
`queryIntentActivities(Intent(ACTION_MAIN), …)` scan), line 165 (`metaData?.getString("renderer")`).
A real sample plugin, `FCL-Team/FCLDriverPlugin`, shows the same manifest shape: an exported
activity with an `ACTION_MAIN`/`LAUNCHER` filter and `meta-data` entries under `<application>`.
`FCL-Team/FCLRendererPlugin` itself is inaccessible (GitHub returns HTTP 451, DMCA).

## Why an activity is required even though nothing starts it

Discovery is `queryIntentActivities(Intent(ACTION_MAIN), …)`. `PackageManager` only answers that
query for packages that declare an activity with an `android.intent.action.MAIN` intent filter.
A manifest with no activities — which is what Hematite shipped before this document existed —
is **invisible to the launcher**, no matter how correct its meta-data is. So the manifest
declares `org.hematite.renderer.PluginInfo` with an `ACTION_MAIN` filter.

That class does not exist: the APK has `android:hasCode="false"` and no `classes.dex`. The
launcher never starts the plugin activity (it only reads `applicationInfo`), so this is safe
for plugin use, and `tools/verify_apk.py` asserts the filter exists. Tapping an app icon would
fail, and no launcher category is declared for it.

## What Hematite declares

`tools/build_apk.py` builds this manifest; `tools/verify_apk.py::verify_plugin_contract` re-reads
the built APK with androguard and asserts every value:

```xml
<manifest package="org.hematite.renderer" android:versionCode="3" android:versionName="0.3.0">
  <uses-sdk android:minSdkVersion="21" android:targetSdkVersion="28"/>
  <application android:label="Hematite Renderer (bring-up)" android:hasCode="false"
               android:debuggable="true" android:allowBackup="false"
               android:extractNativeLibs="true">
    <activity android:name="org.hematite.renderer.PluginInfo"
              android:exported="true" android:label="Hematite Renderer plugin">
      <intent-filter><action android:name="android.intent.action.MAIN"/></intent-filter>
    </activity>
    <meta-data android:name="fclPlugin" android:value="true"/>          <!-- boolean typed value -->
    <meta-data android:name="renderer"
               android:value="hematite:libhematite.so:libhematite.so"/>  <!-- id:gl:egl -->
    <meta-data android:name="des"
               android:value="Hematite (bring-up: fixed-function draws on Vulkan, no presentation)"/>
    <meta-data android:name="pojavEnv"
               android:value="POJAV_RENDERER=hematite:LIBGL_ES=2:LIBGL_MIPMAP=3:LIBGL_NOERROR=1"/>
  </application>
</manifest>
```

Notes that matter:

- **`fclPlugin` must be a boolean-typed AXML value** (`TYPE_INT_BOOLEAN`, data `0xFFFFFFFF`),
  because the launcher calls `Bundle.getBoolean`. A string `"true"` reads back as `false` and the
  plugin is silently skipped. `tools/build_apk.py` emits it with `Attribute("value", True, "bool")`.
- **One library serves both slots.** `libhematite.so` exports the 866 `gl*`/`egl*` symbols, so
  `glName` and `eglName` are the same file. `GameLauncher.kt:212-225` dlopens both; the second
  dlopen returns the already-open handle.
- `LIBGL_ES=2`, `LIBGL_MIPMAP=3`, `LIBGL_NOERROR=1` mirror the environment the launcher itself
  sets for `opengles2*` renderers (`GameLauncher.kt:454-458`). Hematite currently reports a
  GLES 2.0-level frontend; do not raise this to 3 until the ES3 entry points work.
- `minMCVer`/`maxMCVer` are deliberately absent: the launcher treats missing values as
  unconstrained (`RendererPluginManager.kt:134-135`).

## What the launcher does next, and where Hematite stops

Selecting the plugin makes the launcher export `SDL_EGL_LIBRARY` (only if the resolved path is a
real file), `POJAVEXEC_EGL`, `POJAV_RENDERER=hematite`, and `SDL_OPENGL_LIBRARY`, then dlopen the
library and start the JVM with `-Dorg.lwjgl.opengl.libname=<rendererLibrary>`. LWJGL resolves its
GL pointers through `eglGetProcAddress` in that library.

That is exactly how far this is tested. **BUILT-BUT-UNTESTED:** no launcher has been run against
this APK on a device in this environment. **NOT-IMPLEMENTED:** Hematite's EGL is pbuffer-only
(`crates/hematite-gl-frontend/src/egl.rs`), so even after a successful load there is no window
surface and nothing would appear on screen. See `docs/KNOWN_LIMITATIONS.md`.

## The v2 contract, not implemented

`RendererV2PluginManager` reads the key `fclPlugin_V2` as an **integer resource id**
(`metaData.getInt(key, -1) > 0`), resolves it with `getResourcesForApplication(info).getString(id)`,
and JSON-decodes it into a `RendererConfig` (fields `displayName`, `rendererId`, `rendererGLPath`,
`rendererEGLPath`, `dlopenLibPaths`, `env[]`, `minMCVer`, `maxMCVer`; paths prefixed `**|` resolve
against `nativeLibraryDir`). That requires a string *resource*, i.e. a hand-written
`resources.arsc`, which the current APK encoder does not produce. v1 meta-data needs no ARSC, so
Hematite uses v1. Tracked in `docs/ROADMAP.md`.
