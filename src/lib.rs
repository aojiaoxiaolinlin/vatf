mod bitmap;
mod decoder;
mod matrix;
mod shape_utils;
mod tessellator;

use std::{
    borrow::Cow,
    collections::HashMap,
    env,
    fs::File,
    io::{BufReader, Cursor, Write},
    path::{self, PathBuf},
};

use anyhow::Result;
use bytemuck::{Pod, Zeroable};
use image::{ImageBuffer, ImageFormat};
use swf::{
    CharacterId, GradientInterpolation, GradientSpread, Shape, Tag, decompress_swf, parse_swf,
};
use tracing::{error, info};
use zstd::encode_all;

use crate::{
    bitmap::CompressedBitmap,
    decoder::{decode_define_bits_jpeg_dimensions, glue_tables_to_jpeg, remove_invalid_jpeg_data},
    shape_utils::GradientType,
    tessellator::{Gradient, ShapeTessellator},
};

pub const MAGIC_BYTES: &[u8; 4] = b"VATF";

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct Header {
    pub version: u32,
    pub length: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct ShapeRecord {
    pub id: u16,
    pub sub_shape_count: u16,
    pub sub_shape_offset: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct ShapeMesh {
    pub vertex_count: u32,
    pub vertex_offset: u32,
    pub index_count: u32,
    pub index_offset: u32,
    pub material_type: u16,
    pub texture_offset: u16,
    pub material_offset: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default, Pod, Zeroable)]
pub struct GradientUniforms {
    pub focal_point: f32,
    pub interpolation: i32,
    pub shape: i32,
    pub repeat: i32,
    pub texture_transform: [f32; 6],
}
impl From<Gradient> for GradientUniforms {
    fn from(gradient: Gradient) -> Self {
        Self {
            focal_point: gradient.focal_point.to_f32().clamp(-0.98, 0.98),
            interpolation: (gradient.interpolation == GradientInterpolation::LinearRgb) as i32,
            shape: match gradient.gradient_type {
                GradientType::Linear => 1,
                GradientType::Radial => 2,
                GradientType::Focal => 3,
            },
            repeat: match gradient.repeat_mode {
                GradientSpread::Pad => 1,
                GradientSpread::Reflect => 2,
                GradientSpread::Repeat => 3,
            },
            texture_transform: [0.0; 6],
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct BitmapUniforms {
    pub texture_transform: [f32; 6],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct Vertex {
    pub x: i16,
    pub y: i16,
    pub color: Color,
}

// 针对一个 Shape，计算并量化它的顶点
fn quantize_vertex(x: f32, y: f32, bounds_min: (f32, f32), bounds_max: (f32, f32)) -> (i16, i16) {
    let (min_x, min_y) = bounds_min;
    let (max_x, max_y) = bounds_max;

    // 1. 计算中点和半长
    let center_x = (max_x + min_x) * 0.5;
    let center_y = (max_y + min_y) * 0.5;

    let half_x = (max_x - min_x) * 0.5;
    let half_y = (max_y - min_y) * 0.5;

    // 2. 归一化并映射到 [-32767, 32767]
    // 注意：要处理 half == 0 的除零风险（比如一条垂直的线）
    let q_x = if half_x > 0.0 {
        ((x - center_x) / half_x) * 32767.0
    } else {
        0.0
    };
    let q_y = if half_y > 0.0 {
        ((y - center_y) / half_y) * 32767.0
    } else {
        0.0
    };

    // 3. 四舍五入并转为 i16
    (
        q_x.round().clamp(-32767.0, 32767.0) as i16,
        q_y.round().clamp(-32767.0, 32767.0) as i16,
    )
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, Pod, Zeroable)]
pub struct ChunkHeader {
    // 1. 块类型 (Chunk Type / FourCC)：4 个字节。
    // 用来告诉解析器这块数据是字典、顶点、还是时间轴。
    pub chunk_type: [u8; 4],

    // 2. 块长度 (Chunk Length)：4 个字节的 u32。
    // 表示【紧跟在这个头部后面的数据 (Payload)】有多少个字节。
    pub length: u32,
}

#[derive(Default)]
pub struct VatfBuilder {
    pub shape_records: Vec<ShapeRecord>,

    pub shape_meshes: Vec<ShapeMesh>,

    pub gradient_uniforms: Vec<GradientUniforms>,
    pub bitmap_uniforms: Vec<[f32; 6]>,
    pub texture: Vec<u8>,

    pub vertices: Vec<Vertex>,
    pub indices: Vec<u32>,
}

impl VatfBuilder {
    pub fn process_swf_shape(
        &mut self,
        shape: &Shape,
        bitmap: &HashMap<CharacterId, CompressedBitmap>,
    ) {
        info!("处理: {}", shape.id);
        let bounds_min = (
            shape.shape_bounds.x_min.to_pixels() as f32,
            shape.shape_bounds.y_min.to_pixels() as f32,
        );
        let bounds_max = (
            shape.shape_bounds.x_max.to_pixels() as f32,
            shape.shape_bounds.y_max.to_pixels() as f32,
        );

        let mut tessellator = ShapeTessellator::default();
        let lyon_mesh = tessellator.tessellate_shape(shape.into(), bitmap);
        let gradients = lyon_mesh
            .gradients
            .into_iter()
            .map(|gradient| {
                let color = gradient.compute_gradient_color(256);
                let img = ImageBuffer::from_raw_bgra(256, 1, color).unwrap();
                let mut webp_data = Cursor::new(Vec::new());
                img.write_to(&mut webp_data, ImageFormat::WebP).unwrap();
                (webp_data.into_inner(), GradientUniforms::from(gradient))
            })
            .collect::<Vec<_>>();

        for draw in lyon_mesh.draws {
            let vertex_offset = self.vertices.len() as u32;
            let index_offset = self.indices.len() as u32;

            let vertex_count = draw.vertices.len() as u32;
            let index_count = draw.indices.len() as u32;

            for vertex in draw.vertices {
                let color = Color {
                    r: vertex.color.r,
                    g: vertex.color.g,
                    b: vertex.color.b,
                    a: vertex.color.a,
                };
                let (x, y) = quantize_vertex(vertex.x, vertex.y, bounds_min, bounds_max);
                self.vertices.push(Vertex { x, y, color });
            }
            self.indices.extend(draw.indices);

            match draw.draw_type {
                tessellator::DrawType::Color => {
                    self.shape_meshes.push(ShapeMesh {
                        vertex_count,
                        vertex_offset,
                        index_count,
                        index_offset,
                        material_type: 0,
                        texture_offset: 0,
                        material_offset: 0,
                    });
                }
                tessellator::DrawType::Gradient { matrix, gradient } => {
                    let Some((gradient, gradient_uniforms)) = gradients.get(gradient) else {
                        continue;
                    };

                    let matrix = [
                        matrix[0][0],
                        matrix[0][1],
                        matrix[1][0],
                        matrix[1][1],
                        matrix[2][0],
                        matrix[2][1],
                    ];
                    let mut gradient_uniforms = *gradient_uniforms;
                    gradient_uniforms.texture_transform = matrix;

                    let material_offset = self.gradient_uniforms.len() as u32;
                    self.gradient_uniforms.push(gradient_uniforms.clone());
                    let texture_offset = self.texture.len() as u16;

                    self.texture.extend(gradient);

                    self.shape_meshes.push(ShapeMesh {
                        vertex_count,
                        vertex_offset,
                        index_count,
                        index_offset,
                        material_type: 1,
                        texture_offset,
                        material_offset,
                    });
                }
                tessellator::DrawType::Bitmap(bm) => {
                    let Some(compressed_bitmap) = bitmap.get(&bm.bitmap_id) else {
                        continue;
                    };
                    let decoded = match compressed_bitmap.decode() {
                        Ok(decoded) => decoded,
                        Err(e) => {
                            error!("Failed to decode bitmap: {:?}", e);
                            continue;
                        }
                    };
                    let bitmap_rgba = decoded.into_rgba();
                    let matrix = bm.matrix;
                    let matrix = [
                        matrix[0][0],
                        matrix[0][1],
                        matrix[1][0],
                        matrix[1][1],
                        matrix[2][0],
                        matrix[2][1],
                    ];

                    let texture_offset = self.texture.len() as u16;
                    self.texture.extend(bitmap_rgba.data());
                    let material_offset = self.bitmap_uniforms.len() as u32;
                    self.bitmap_uniforms.push(matrix);

                    self.shape_meshes.push(ShapeMesh {
                        vertex_count,
                        vertex_offset,
                        index_count,
                        index_offset,
                        material_type: 2,
                        texture_offset,
                        material_offset,
                    });
                }
            }
        }
    }

    pub fn write_vatf(&mut self, path: PathBuf) {
        let mut file = std::fs::File::create(path).unwrap();

        let shape_records: &[u8] = bytemuck::cast_slice(&self.shape_records);
        let shape_records_header = ChunkHeader {
            chunk_type: *b"SHAP",
            length: shape_records.len() as u32,
        };
        let shape_meshes: &[u8] = bytemuck::cast_slice(&self.shape_meshes);
        let shape_meshes_header = ChunkHeader {
            chunk_type: *b"SHME",
            length: shape_meshes.len() as u32,
        };

        let gradient_uniforms: &[u8] = bytemuck::cast_slice(&self.gradient_uniforms);
        let gradient_uniforms_header = ChunkHeader {
            chunk_type: *b"GRAD",
            length: gradient_uniforms.len() as u32,
        };
        let bitmap_uniforms: &[u8] = bytemuck::cast_slice(&self.bitmap_uniforms);
        let bitmap_uniforms_header = ChunkHeader {
            chunk_type: *b"BMAP",
            length: bitmap_uniforms.len() as u32,
        };

        let texture: &[u8] = &self.texture;
        let texture_header = ChunkHeader {
            chunk_type: *b"TEXT",
            length: texture.len() as u32,
        };

        let vertex: &[u8] = bytemuck::cast_slice(&self.vertices);
        let vertex_header = ChunkHeader {
            chunk_type: *b"VERT",
            length: vertex.len() as u32,
        };
        let indices: &[u8] = bytemuck::cast_slice(&self.indices);
        let indices_header = ChunkHeader {
            chunk_type: *b"INDX",
            length: indices.len() as u32,
        };

        let header = Header {
            version: 1,
            length: shape_records.len() as u32
                + shape_meshes.len() as u32
                + gradient_uniforms.len() as u32
                + bitmap_uniforms.len() as u32
                + texture.len() as u32
                + vertex.len() as u32
                + indices.len() as u32
                + MAGIC_BYTES.len() as u32
                + std::mem::size_of::<Header>() as u32
                + std::mem::size_of::<ChunkHeader>() as u32 * 7,
        };
        let header = &[header];
        let header: &[u8] = bytemuck::cast_slice(header);
        file.write_all(&[MAGIC_BYTES, header].concat()).unwrap();

        let uncompressed_payload = [
            MAGIC_BYTES,
            header,
            bytemuck::cast_slice(&[shape_records_header]),
            shape_records,
            bytemuck::cast_slice(&[shape_meshes_header]),
            shape_meshes,
            bytemuck::cast_slice(&[gradient_uniforms_header]),
            gradient_uniforms,
            bytemuck::cast_slice(&[bitmap_uniforms_header]),
            bitmap_uniforms,
            bytemuck::cast_slice(&[texture_header]),
            texture,
            bytemuck::cast_slice(&[vertex_header]),
            vertex,
            bytemuck::cast_slice(&[indices_header]),
            indices,
        ]
        .concat();

        let compressed_payload = encode_all(uncompressed_payload.as_slice(), 3).unwrap();

        file.write_all(&compressed_payload).unwrap();

        println!(
            "成功写入！原始大小: {} B, 压缩后: {} B",
            uncompressed_payload.len(),
            compressed_payload.len()
        );
    }
}

pub fn parse_and_convert(path: &path::Path, out_path: &str) -> Result<()> {
    // 校验文件名必须以 .swf 结尾
    if let Some(ext) = path.extension() {
        if ext != "swf" {
            error!("File is not a SWF file: {:?}", path);
        }
    }

    let Ok(file) = File::open(path) else {
        error!("Failed to open file: {:?}", path);
        return Ok(());
    };

    let reader = BufReader::new(file);
    let swf_buf = decompress_swf(reader)?;

    let swf = parse_swf(&swf_buf)?;
    let total_frames = swf.header.num_frames();

    let mut jpeg_tables: Option<Vec<_>> = None;
    let mut bitmap = HashMap::new();
    let mut vatf_builder = VatfBuilder::default();
    parse_tag(swf.tags, &mut vatf_builder, &mut bitmap, &mut jpeg_tables)?;

    // 写入当前目录下文件
    let current_dir: PathBuf = env::current_dir()?;
    let output = current_dir.join("output");
    if !output.exists() {
        std::fs::create_dir_all(&output)?;
    }
    vatf_builder.write_vatf(output.join(out_path));

    Ok(())
}

fn parse_tag(
    tags: Vec<Tag<'_>>,
    vatf_builder: &mut VatfBuilder,
    bitmap: &mut HashMap<CharacterId, CompressedBitmap>,
    jpeg_tables: &mut Option<Vec<u8>>,
) -> Result<()> {
    for tag in tags {
        match tag {
            Tag::DefineShape(shape) => {
                vatf_builder.process_swf_shape(&shape, bitmap);
            }
            Tag::JpegTables(data) => {
                if jpeg_tables.is_some() {
                    eprintln!("SWF contains multiple JPEGTables tags")
                } else {
                    *jpeg_tables = if data.is_empty() {
                        None
                    } else {
                        Some(remove_invalid_jpeg_data(data).into_owned())
                    }
                }
            }
            Tag::DefineBits { id, jpeg_data } => {
                let jpeg_data = glue_tables_to_jpeg(jpeg_data, jpeg_tables.as_deref()).into_owned();
                let (width, height) = decode_define_bits_jpeg_dimensions(&jpeg_data)?;
                bitmap.insert(
                    id,
                    CompressedBitmap::Jpeg {
                        data: jpeg_data,
                        alpha: None,
                        width,
                        height,
                    },
                );
            }
            Tag::DefineBitsJpeg2 { id, jpeg_data } => {
                let (width, height) = decode_define_bits_jpeg_dimensions(jpeg_data)?;
                bitmap.insert(
                    id,
                    CompressedBitmap::Jpeg {
                        data: jpeg_data.to_owned(),
                        alpha: None,
                        width,
                        height,
                    },
                );
            }
            Tag::DefineBitsJpeg3(jpeg_data) => {
                let (width, height) = decode_define_bits_jpeg_dimensions(jpeg_data.data).unwrap();
                bitmap.insert(
                    jpeg_data.id,
                    CompressedBitmap::Jpeg {
                        data: jpeg_data.data.to_vec(),
                        alpha: Some(jpeg_data.alpha_data.to_vec()),
                        width,
                        height,
                    },
                );
            }
            Tag::DefineBitsLossless(bit_loss_less) => {
                bitmap.insert(
                    bit_loss_less.id,
                    CompressedBitmap::Lossless(swf::DefineBitsLossless {
                        version: bit_loss_less.version,
                        id: bit_loss_less.id,
                        format: bit_loss_less.format,
                        width: bit_loss_less.width,
                        height: bit_loss_less.height,
                        data: Cow::Owned(bit_loss_less.data.clone().into_owned()),
                    }),
                );
            }
            Tag::DefineSprite(sprite) => {
                parse_tag(sprite.tags, vatf_builder, bitmap, jpeg_tables)?;
            }
            _ => {}
        }
    }

    Ok(())
}
