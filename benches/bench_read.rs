use criterion::{Criterion, black_box, criterion_group, criterion_main};

/// VAB 读取基准：使用项目内 SWF，在计时前生成当前格式的 VAB。
/// SWF 编译不计入读取时间；缺失素材或编译失败会报错而不是跳过。
/// 运行：cargo bench --bench bench_read
fn bench_parse_vab(c: &mut Criterion) {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/spirit2159src.swf");
    let source = std::fs::read(&path).expect("missing bundled spirit2159src.swf fixture");
    let compiled = vatf::compile_swf(&source, &vatf::SwfCompileSettings::default())
        .expect("failed to compile benchmark fixture");
    let data = compiled.bytes;
    vatf::reader::VabReader::from_bytes(&data).expect("invalid compiled benchmark VAB");

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
