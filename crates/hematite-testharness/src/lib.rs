//! Real Vulkan offscreen readback checks with analytic pixel expectations.
//!
//! This tests the backend directly, NOT a GL-call frontend, Android window, Minecraft,
//! shader pack, validation layer or hardware performance. Tests fail on missing drivers;
//! they never substitute precomputed pixels for a Vulkan result.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use hematite_fixed_function::immediate::{Immediate, Primitive};
use hematite_vk_backend::{Image, OffscreenRenderer, Vertex};
use std::io::{self, Write};

/// Human-readable checks that actually completed, plus the interpolated triangle readback.
pub struct Report {
    /// Successful check names; no skipped check is listed here.
    pub passed: Vec<&'static str>,
    /// Image returned by Vulkan after the color-interpolation test.
    pub triangle: Image,
}

fn check(condition: bool, message: &str) -> Result<(), Box<dyn std::error::Error>> {
    if condition {
        Ok(())
    } else {
        Err(message.into())
    }
}

fn near(actual: [u8; 4], expected: [u8; 4], tolerance: u8) -> bool {
    actual.iter().zip(expected).all(|(a, b)| a.abs_diff(b) <= tolerance)
}

fn triangle(colors: [[f32; 4]; 3]) -> [Vertex; 3] {
    std::array::from_fn(|index| Vertex {
        position: [[-0.75, -0.75], [0.75, -0.75], [0.0, 0.75]][index],
        color: colors[index],
    })
}

fn barycentric(x: f32, y: f32) -> [f32; 3] {
    let c = (y + 0.75) / 1.5;
    let b = (x + 0.75 - 0.75 * c) / 1.5;
    [1.0 - b - c, b, c]
}

/// Execute five independent rendering checks; missing or wrong pixels return an error.
pub fn run(renderer: &mut OffscreenRenderer) -> Result<Report, Box<dyn std::error::Error>> {
    let mut passed = Vec::new();
    let clear = renderer.render(64, 48, [0.25, 0.5, 0.75, 1.0], &[])?;
    check(clear.rgba.len() == 64 * 48 * 4, "clear readback length")?;
    check(
        clear.rgba.chunks_exact(4).all(|p| near(p.try_into().unwrap(), [64, 128, 191, 255], 1)),
        "clear pixels differ from UNORM expectation",
    )?;
    passed.push("RGBA8 clear and full-image readback");

    let solid =
        renderer.render(64, 64, [0.0, 0.0, 0.0, 1.0], &triangle([[1.0, 0.0, 0.0, 1.0]; 3]))?;
    let mut covered = 0;
    for y in 0..64 {
        for x in 0..64 {
            let weights = barycentric((x as f32 + 0.5) / 32.0 - 1.0, (y as f32 + 0.5) / 32.0 - 1.0);
            let actual = solid.pixel(x, y).ok_or("truncated solid triangle readback")?;
            if weights.iter().all(|w| *w > 0.02) {
                check(near(actual, [255, 0, 0, 255], 1), "solid triangle interior missing")?;
            } else if weights.iter().any(|w| *w < -0.02) {
                check(near(actual, [0, 0, 0, 255], 1), "triangle wrote outside analytic boundary")?;
            }
            covered += usize::from(actual[0] > 200);
        }
    }
    check((1000..1300).contains(&covered), "unexpected triangle coverage")?;
    passed.push("triangle rasterization and analytic coverage");

    let triangle = renderer.render(
        64,
        64,
        [0.0, 0.0, 0.0, 1.0],
        &triangle([[1.0, 0.0, 0.0, 1.0], [0.0, 1.0, 0.0, 1.0], [0.0, 0.0, 1.0, 1.0]]),
    )?;
    for y in 0..64 {
        for x in 0..64 {
            let weights = barycentric((x as f32 + 0.5) / 32.0 - 1.0, (y as f32 + 0.5) / 32.0 - 1.0);
            if weights.iter().all(|w| *w > 0.02) {
                let expected = [
                    (weights[0] * 255.0).round() as u8,
                    (weights[1] * 255.0).round() as u8,
                    (weights[2] * 255.0).round() as u8,
                    255,
                ];
                check(
                    near(triangle.pixel(x, y).ok_or("truncated interpolated image")?, expected, 2),
                    "smooth color interpolation differs from barycentric reference",
                )?;
            }
        }
    }
    passed.push("smooth RGBA interpolation against CPU barycentric reference");

    let mut capture = Immediate::new(4).map_err(|e| format!("capture allocation: {e:?}"))?;
    capture.attributes_mut().color = [0.0, 1.0, 0.0, 0.5];
    capture.begin(Primitive::Quads).map_err(|e| format!("begin: {e:?}"))?;
    for [x, y] in [[-1.0, -1.0], [1.0, -1.0], [1.0, 1.0], [-1.0, 1.0]] {
        capture.vertex([x, y, 0.0, 1.0]).map_err(|e| format!("capture: {e:?}"))?;
    }
    let batch = capture.end().map_err(|e| format!("end: {e:?}"))?;
    let vertices: Vec<_> = batch
        .indices
        .iter()
        .map(|index| {
            let vertex = batch.vertices[*index as usize];
            Vertex {
                position: [vertex.position[0] as f32, vertex.position[1] as f32],
                color: vertex.attributes.color,
            }
        })
        .collect();
    let quad = renderer.render(31, 27, [0.0; 4], &vertices)?;
    check(
        quad.rgba.chunks_exact(4).all(|pixel| near(pixel.try_into().unwrap(), [0, 255, 0, 128], 1)),
        "quad conversion/upload/coverage or alpha readback failed",
    )?;
    passed.push("legacy immediate quad conversion -> Vulkan vertex upload -> readback");

    for (width, height) in [(1, 1), (8, 7), (33, 17), (64, 64)] {
        let image = renderer.render(width, height, [0.0, 0.0, 1.0, 1.0], &[])?;
        check(image.rgba.len() == width as usize * height as usize * 4, "resized readback size")?;
        check(
            image.rgba.chunks_exact(4).all(|p| p == [0, 0, 255, 255]),
            "resize/repeated render has stale pixels",
        )?;
    }
    passed.push("repeated rendering, 1x1 edge case and independent target dimensions");
    Ok(Report { passed, triangle })
}

/// Write an RGB PPM from an actual RGBA readback. Alpha is intentionally omitted.
pub fn write_ppm(image: &Image, mut output: impl Write) -> io::Result<()> {
    let expected =
        (image.width as usize).checked_mul(image.height as usize).and_then(|n| n.checked_mul(4));
    if expected != Some(image.rgba.len()) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid RGBA image length"));
    }
    write!(output, "P6\n{} {}\n255\n", image.width, image.height)?;
    for pixel in image.rgba.chunks_exact(4) {
        output.write_all(&pixel[..3])?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn barycentric_reference_at_vertices_and_center() {
        assert_eq!(barycentric(-0.75, -0.75), [1.0, 0.0, 0.0]);
        assert_eq!(barycentric(0.75, -0.75), [0.0, 1.0, 0.0]);
        assert_eq!(barycentric(0.0, 0.75), [0.0, 0.0, 1.0]);
        for value in barycentric(0.0, -0.25) {
            assert!((value - 1.0 / 3.0).abs() < 1e-6);
        }
    }

    #[test]
    fn ppm_validates_length_and_preserves_rgb() {
        let mut output = Vec::new();
        write_ppm(&Image { width: 1, height: 1, rgba: vec![10, 20, 30, 40] }, &mut output).unwrap();
        assert_eq!(output, b"P6\n1 1\n255\n\x0a\x14\x1e");
        assert!(write_ppm(&Image { width: 1, height: 1, rgba: vec![] }, &mut output).is_err());
    }

    #[test]
    #[ignore = "requires an actual Vulkan 1.1+ driver; run ./build.sh gpu"]
    fn real_vulkan_readback_suite() {
        let loader = std::env::var_os("HEMATITE_VULKAN_LIBRARY").map(PathBuf::from);
        for _ in 0..2 {
            let mut renderer =
                OffscreenRenderer::new(loader.as_deref()).expect("real Vulkan device required");
            let report = run(&mut renderer).expect("Vulkan pixels must match analytic reference");
            assert_eq!(report.passed.len(), 5);
            assert!(!renderer.capabilities().device_name.is_empty());
        }
    }

    /// The SPIR-V Hematite compiles from GLSL must be accepted by a real driver.
    ///
    /// `vkCreateShaderModule` is where a driver parses a module, so this is the
    /// difference between "Hematite produced bytes" and "a Vulkan implementation
    /// accepts these bytes". Legacy GLSL 1.20, `#version 330/450 core` and a
    /// shader-pack style fragment stage are all covered.
    #[test]
    #[ignore = "requires an actual Vulkan 1.1+ driver; run ./build.sh gpu"]
    fn compiled_glsl_loads_into_a_real_driver() {
        use hematite_glsl::{Request, Stage};

        let cases: Vec<(Stage, &str, &str)> = vec![
            (
                Stage::Vertex,
                "legacy 1.20 vertex",
                "#version 120\nattribute vec4 position;\nattribute vec2 uv;\n                 varying vec2 texcoord;\nuniform mat4 mvp;\n                 void main() { texcoord = uv; gl_Position = mvp * position; }\n",
            ),
            (
                Stage::Fragment,
                "legacy 1.20 fragment",
                "#version 120\nvarying vec2 texcoord;\nuniform sampler2D diffuse;\n                 void main() { gl_FragColor = texture2D(diffuse, texcoord); }\n",
            ),
            (
                Stage::Vertex,
                "330 core vertex",
                "#version 330 core\nlayout(location = 0) in vec3 position;\n                 out vec4 colour;\nvoid main() { colour = vec4(position, 1.0);                  gl_Position = vec4(position, 1.0); }\n",
            ),
            (
                Stage::Fragment,
                "450 core fragment",
                "#version 450 core\nlayout(location = 0) out vec4 colour;\n                 layout(binding = 0) uniform Frame { vec4 tint; };\n                 void main() { colour = tint; }\n",
            ),
            (
                Stage::Fragment,
                "shader-pack style fragment (shadow sampler, MRT, include)",
                "#version 120\n#include \"/lib/common.glsl\"\nvarying vec2 texcoord;\n                 uniform sampler2D diffuse;\nuniform sampler2DShadow shadowtex;\n                 void main() {\n                 \x20   vec4 base = texture2D(diffuse, texcoord);\n                 \x20   float shade = shadow2D(shadowtex, vec3(texcoord, 0.5)).r;\n                 \x20   gl_FragColor = vec4(SATURATE(base.rgb * shade), 1.0);\n                 \x20   gl_FragData[1] = vec4(texcoord, 0.0, 1.0);\n}\n",
            ),
        ];
        let includes = [("/lib/common.glsl", "#define SATURATE(x) clamp(x, 0.0, 1.0)\n")];

        let loader = std::env::var_os("HEMATITE_VULKAN_LIBRARY").map(PathBuf::from);
        let mut renderer = OffscreenRenderer::new(loader.as_deref()).expect("real Vulkan device");
        for (stage, label, source) in cases {
            let request = Request { stage, source, includes: &includes, ..Request::default() };
            let shader = match hematite_glsl::compile(&request) {
                Ok(shader) => shader,
                Err(error) => panic!("{label}: {error}"),
            };
            assert_eq!(shader.spirv[0], hematite_glsl::SPIRV_MAGIC, "{label}: magic word");
            renderer
                .accepts_shader_module(&shader.spirv)
                .unwrap_or_else(|error| panic!("{label}: the driver rejected the module: {error}"));
            println!("  driver accepted the {label} module ({} words)", shader.spirv.len());
        }

        // Creating a module alone proves nothing: this driver accepts a truncated
        // module at `vkCreateShaderModule` time. So compile a shader that can only
        // be right if it actually runs, and check the pixels.
        let drawn = renderer
            .render_with_shaders(
                3,
                3,
                [0.0, 1.0, 0.0, 1.0],
                &[
                    Vertex { position: [-3.0, -1.0], color: [1.0, 1.0, 1.0, 1.0] },
                    Vertex { position: [3.0, -1.0], color: [1.0, 1.0, 1.0, 1.0] },
                    Vertex { position: [0.0, 3.0], color: [1.0, 1.0, 1.0, 1.0] },
                ],
                &hematite_glsl::compile(&Request::new(
                    Stage::Vertex,
                    "#version 120\nattribute vec2 aPosition;\n\
                     void main() { gl_Position = vec4(aPosition, 0.0, 1.0); }\n",
                ))
                .expect("draw vertex compiles")
                .spirv,
                &hematite_glsl::compile(&Request::new(
                    Stage::Fragment,
                    "#version 120\nvoid main() { gl_FragColor = vec4(1.0, 0.0, 0.0, 1.0); }\n",
                ))
                .expect("draw fragment compiles")
                .spirv,
            )
            .expect("a shader compiled from GLSL 1.20 must execute on a real driver");
        let mut pixels = Vec::new();
        for y in 0..drawn.height {
            for x in 0..drawn.width {
                pixels.push(drawn.pixel(x, y).expect("pixel inside the image"));
            }
        }
        assert_eq!(pixels.len(), 9, "a 3x3 image has nine pixels");
        assert!(
            pixels.iter().all(|pixel| *pixel == [255, 0, 0, 255]),
            "every pixel must be the red the compiled fragment shader writes, not the green clear: {:?}",
            pixels
        );
        println!("  a GLSL 1.20 program ran on the driver and produced the red it writes");

        let vertex = hematite_glsl::compile(&Request::new(
            Stage::Vertex,
            "#version 120\nvoid main() { gl_Position = vec4(0.0, 0.0, 0.0, 1.0); }\n",
        ))
        .expect("probe vertex compiles");
        let fragment = hematite_glsl::compile(&Request::new(
            Stage::Fragment,
            "#version 120\nvoid main() { gl_FragColor = vec4(1.0, 0.0, 0.0, 1.0); }\n",
        ))
        .expect("probe fragment compiles");
        renderer
            .accepts_pipeline(&vertex.spirv, &fragment.spirv)
            .expect("the driver must build a pipeline from Hematite's SPIR-V");
        println!("  driver built a graphics pipeline from the compiled probe stages");

        // Malformed input must never reach the driver. It cannot be checked on the
        // driver side here: SwiftShader logs a diagnostic and then dereferences the
        // invalid module instead of returning an error, so this test process would
        // be killed by SIGSEGV (observed against the SwiftShader in
        // .cache/vulkan/driver). The rejection therefore has to happen in
        // hematite-glsl, which is where it is asserted.
        let rejected = hematite_glsl::compile(&Request::new(
            Stage::Fragment,
            "#version 120\nvoid main() { gl_FragColor = ; }\n",
        ));
        assert!(rejected.is_err(), "a broken shader must not produce SPIR-V");
        assert!(
            rejected.unwrap_err().to_info_log().contains("Expected"),
            "the info log must name a syntax problem"
        );
    }
}
