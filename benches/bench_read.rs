use criterion::{Criterion, black_box, criterion_group, criterion_main};

/// VAB 文件读取基准测试
///
/// 依赖一个已生成的 .vab 产物。路径不存在、或者产物版本与本 crate 的
/// `VAB_VERSION` 不符时（例如格式改动后还没重新转换），基准会**跳过**而不是
/// panic —— 重新转换即可：`cargo run -- <swf 目录>`。
///
/// 运行：
///   cargo bench --bench bench_read
fn bench_parse_vab(c: &mut Criterion) {
    let path = r"D:\Code\Rust\bevy_flash_remake\assets\spirit2159src.vab";
    let Ok(data) = std::fs::read(path) else {
        eprintln!("skipping: {path} not found — run `cargo run` to regenerate it");
        return;
    };

    // Fail early and legibly on a stale artefact instead of inside the timed loop.
    if let Err(e) = vatf::reader::VabReader::from_bytes(&data) {
        eprintln!("skipping: {path} is not readable by this build: {e:#}");
        return;
    }

    let file_size = data.len();

    c.bench_function("VabReader::from_bytes", |b| {
        b.iter(|| {
            let reader = vatf::reader::VabReader::from_bytes(black_box(&data)).unwrap();
            // Touch all chunk accessors to force full evaluation
            let _ = reader.shape_records();
            let _ = reader.shape_meshes();
            let _ = reader.gradient_uniforms();
            let _ = reader.bitmap_uniforms();
            let _ = reader.texture_data();
            let _ = reader.vertices();
            let _ = reader.indices();
            let _ = reader.morph_entries();
            let _ = reader.baked();
            black_box(reader);
        })
    });

    println!(
        "  File: spirit2159src.vab, {:.1} MB",
        file_size as f64 / 1_000_000.0
    );
}

criterion_group!(benches, bench_parse_vab);
criterion_main!(benches);
