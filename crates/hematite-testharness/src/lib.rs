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
}
