//! Shared SWF compilation settings for the CLI and asset processors.
use crate::{ResourcePruningReport, build_builder_reader};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{io::Cursor, path::Path};

/// Increment when compiler behavior changes, independently of the VAB layout version.
pub const COMPILER_REVISION: u32 = 1;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum SwfCompileMode {
    /// Root playback, clips, events and named skins.
    #[default]
    Animation,
    /// ExportAssets symbols with strictly static pure-vector dependencies.
    StaticUi,
    /// ExportAssets symbols with looping child timelines baked offline.
    AnimatedUi,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SwfCompileSettings {
    pub mode: SwfCompileMode,
}

pub struct CompiledVab {
    pub bytes: Vec<u8>,
    pub pruning: ResourcePruningReport,
}

/// Compile directly from source bytes, without temporary files or runtime assets.
pub fn compile_swf(bytes: &[u8], settings: &SwfCompileSettings) -> Result<CompiledVab> {
    let builder = build_builder_reader(Cursor::new(bytes), settings)?;
    let (bytes, pruning) = builder.to_vab_bytes()?;
    Ok(CompiledVab { bytes, pruning })
}

/// File-based wrapper using exactly the same settings and compiler as `compile_swf`.
pub fn convert_swf(
    input: &Path,
    output: &Path,
    settings: &SwfCompileSettings,
) -> Result<ResourcePruningReport> {
    ensure!(
        input.extension().and_then(|s| s.to_str()) == Some("swf"),
        "Not a .swf file: {}",
        input.display()
    );
    let source =
        std::fs::read(input).with_context(|| format!("Failed to read {}", input.display()))?;
    let compiled = compile_swf(&source, settings)
        .with_context(|| format!("Failed to compile {}", input.display()))?;
    if let Some(parent) = output.parent().filter(|path| !path.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(output, compiled.bytes)
        .with_context(|| format!("Failed to write {}", output.display()))?;
    Ok(compiled.pruning)
}
