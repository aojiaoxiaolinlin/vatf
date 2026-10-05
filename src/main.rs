use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;

use vatf::{SwfCompileMode, SwfCompileSettings, convert_swf};

/// SWF → VAB converter — bake Flash animations into a compact runtime format.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// Input .swf file, or directory containing .swf files.
    input: PathBuf,
    /// Export ExportAssets symbols as a strict static pure-vector UI library.
    #[arg(long)]
    ui: bool,
    /// Allow child animation in exported UI.
    #[arg(long, conflicts_with = "ui")]
    ui_animated: bool,

    /// Output path.
    ///
    /// For a single file: full output path (including .vab extension).
    /// For a directory: output directory (defaults to ./output).
    #[arg(short, long)]
    output: Option<PathBuf>,
}

fn main() -> Result<()> {
    let args = Cli::parse();

    if args.input.is_dir() {
        convert_dir(
            &args.input,
            args.output.as_deref(),
            args.ui,
            args.ui_animated,
        )?;
    } else {
        let output = resolve_output(&args.input, args.output.as_deref());
        convert_single(&args.input, &output, args.ui, args.ui_animated)?;
    }

    Ok(())
}

// ── Single file ─────────────────────────────────────────────────────────────

fn convert_single(
    input: &std::path::Path,
    output: &std::path::Path,
    ui: bool,
    ui_animated: bool,
) -> Result<()> {
    print!(
        "  Converting {} … ",
        input.file_name().unwrap_or_default().to_string_lossy()
    );
    let result = convert_swf(input, output, &settings(ui, ui_animated));
    match &result {
        Ok(report) => println!(
            "✓  ({}; meshes {}→{}, resource payload {}→{}, textures {}→{})",
            format_size(file_size(output)),
            report.before.meshes,
            report.after.meshes,
            format_size(report.before.bytes as u64),
            format_size(report.after.bytes as u64),
            format_size(report.before.texture_bytes as u64),
            format_size(report.after.texture_bytes as u64)
        ),
        Err(e) => eprintln!("✗  {e:#}"),
    }
    result.map(|_| ())
}

// ── Directory mode ──────────────────────────────────────────────────────────

fn convert_dir(
    input_dir: &std::path::Path,
    output_dir: Option<&std::path::Path>,
    ui: bool,
    ui_animated: bool,
) -> Result<()> {
    let output_dir = output_dir
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| input_dir.join("output"));

    let mut entries: Vec<_> = std::fs::read_dir(input_dir)
        .with_context(|| format!("Failed to read directory: {}", input_dir.display()))?
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let path = entry.path();
            (path.extension().and_then(|e| e.to_str()) == Some("swf")).then_some(path)
        })
        .collect();
    entries.sort();

    if entries.is_empty() {
        println!("No .swf files found in {}", input_dir.display());
        return Ok(());
    }

    let total = entries.len();
    let mut ok = 0u32;
    let mut errs = 0u32;

    println!("── Processing {total} .swf files ──");

    for (i, input) in entries.iter().enumerate() {
        let stem = input.file_stem().unwrap_or_default();
        let output = output_dir.join(format!("{}.vab", stem.to_string_lossy()));

        print!("[{:>2}/{total}] {} … ", i + 1, stem.to_string_lossy());
        match convert_swf(input, &output, &settings(ui, ui_animated)) {
            Ok(report) => {
                ok += 1;
                println!(
                    "✓  ({}; meshes {}→{}, resource payload {}→{})",
                    format_size(file_size(&output)),
                    report.before.meshes,
                    report.after.meshes,
                    format_size(report.before.bytes as u64),
                    format_size(report.after.bytes as u64)
                );
            }
            Err(e) => {
                errs += 1;
                eprintln!("✗  {e:#}");
            }
        }
    }

    println!("── Done — {ok} succeeded, {errs} failed (of {total}) ──");
    if errs > 0 {
        Err(anyhow::anyhow!("{errs} conversion(s) failed"))
    } else {
        Ok(())
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────────

/// Resolve the output path for a single input file.
fn resolve_output(input: &std::path::Path, output: Option<&std::path::Path>) -> PathBuf {
    match output {
        Some(p) => {
            if p.is_dir() {
                let stem = input.file_stem().unwrap_or_default();
                p.join(format!("{}.vab", stem.to_string_lossy()))
            } else {
                p.to_path_buf()
            }
        }
        None => {
            let parent = input.parent().unwrap_or(std::path::Path::new("."));
            let stem = input.file_stem().unwrap_or_default();
            parent.join(format!("{}.vab", stem.to_string_lossy()))
        }
    }
}

/// Human-readable file size.
fn format_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.0} KB", bytes / KB)
    } else {
        format!("{bytes} B")
    }
}

fn file_size(path: &std::path::Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

fn settings(ui: bool, ui_animated: bool) -> SwfCompileSettings {
    SwfCompileSettings {
        mode: if ui_animated {
            SwfCompileMode::AnimatedUi
        } else if ui {
            SwfCompileMode::StaticUi
        } else {
            SwfCompileMode::Animation
        },
    }
}
