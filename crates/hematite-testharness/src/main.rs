//! Native Vulkan offscreen bring-up harness. Not an Android launcher plugin.
#![forbid(unsafe_code)]

use hematite_vk_backend::OffscreenRenderer;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut loader = std::env::var_os("HEMATITE_VULKAN_LIBRARY").map(PathBuf::from);
    let mut output = None;
    let mut args = std::env::args_os().skip(1);
    while let Some(argument) = args.next() {
        match argument.to_str() {
            Some("--loader") => loader = Some(args.next().ok_or("--loader needs a path")?.into()),
            Some("--output") => {
                output = Some(PathBuf::from(args.next().ok_or("--output needs a path")?))
            }
            Some("--help") => {
                println!(
                    "hematite-testharness [--loader path/to/libvulkan.so] [--output triangle.ppm]"
                );
                println!("Runs five real Vulkan readback checks. Not a GL/Android/MC compatibility test.");
                return Ok(());
            }
            _ => return Err(format!("unknown argument: {}", argument.to_string_lossy()).into()),
        }
    }
    let mut renderer = OffscreenRenderer::new(loader.as_deref())?;
    println!("Queried Vulkan device: {:#?}", renderer.capabilities());
    let report = hematite_testharness::run(&mut renderer)?;
    for name in &report.passed {
        println!("VERIFIED: {name}");
    }
    if let Some(path) = output {
        let mut file = BufWriter::new(File::create(&path)?);
        hematite_testharness::write_ppm(&report.triangle, &mut file)?;
        file.flush()?;
        println!("Actual Vulkan readback written to {}", path.display());
    }
    println!(
        "{} Vulkan checks passed. This run covers the offscreen Vulkan path only: no \
         Android device, window/swapchain, shader pack or performance (the GL frontend \
         has its own check, ./build.sh gl).",
        report.passed.len()
    );
    Ok(())
}
