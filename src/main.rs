use std::path::Path;

use vatf::parse_and_convert;

fn main() {
    parse_and_convert(
        Path::new("D:\\Code\\Rust\\bevy_flash\\assets\\spirit2159src.swf"),
        "spirit2159src.vab",
    )
    .unwrap();
}
