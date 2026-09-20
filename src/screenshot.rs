#[cfg(unix)]
#[path = "screenshot/unix.rs"]
mod platform;
#[cfg(windows)]
#[path = "screenshot/windows.rs"]
mod platform;

pub use platform::capture;

pub struct Screenshot {
    pub data: Vec<u8>,
    pub mime_type: &'static str,
    pub backend: &'static str,
    pub width: u32,
    pub height: u32,
    pub source_width: u32,
    pub source_height: u32,
}

impl Screenshot {
    fn new(
        data: Vec<u8>,
        mime_type: &'static str,
        backend: &'static str,
        source: Option<(u32, u32)>,
    ) -> Result<Self, String> {
        let (width, height) = dimensions(&data).ok_or("screenshot has invalid image dimensions")?;
        let (source_width, source_height) = source.unwrap_or((width, height));
        Ok(Self {
            data,
            mime_type,
            backend,
            width,
            height,
            source_width,
            source_height,
        })
    }
}

/// Read PNG IHDR or JPEG SOF dimensions without adding an image decoder.
fn dimensions(data: &[u8]) -> Option<(u32, u32)> {
    let size = if data.starts_with(b"\x89PNG\r\n\x1a\n") && data.get(12..16)? == b"IHDR" {
        (
            u32::from_be_bytes(data.get(16..20)?.try_into().ok()?),
            u32::from_be_bytes(data.get(20..24)?.try_into().ok()?),
        )
    } else if data.starts_with(b"\xff\xd8") {
        let mut offset = 2;
        loop {
            if *data.get(offset)? != 0xff {
                return None;
            }
            while *data.get(offset)? == 0xff {
                offset += 1;
            }
            let marker = *data.get(offset)?;
            offset += 1;
            if matches!(marker, 0xd9 | 0xda) {
                return None;
            }
            if matches!(marker, 0x01 | 0xd0..=0xd8) {
                continue;
            }
            let length = usize::from(u16::from_be_bytes(
                data.get(offset..offset + 2)?.try_into().ok()?,
            ));
            if length < 2 {
                return None;
            }
            let segment = data.get(offset..offset + length)?;
            if matches!(marker, 0xc0..=0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf) {
                break (
                    u32::from(u16::from_be_bytes(segment.get(5..7)?.try_into().ok()?)),
                    u32::from(u16::from_be_bytes(segment.get(3..5)?.try_into().ok()?)),
                );
            }
            offset += length;
        }
    } else {
        return None;
    };
    (size.0 > 0 && size.1 > 0 && size.0 <= i32::MAX as u32 && size.1 <= i32::MAX as u32)
        .then_some(size)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_png_and_jpeg_dimensions() {
        let png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\x07\x80\0\0\x04\x38";
        let jpeg = b"\xff\xd8\xff\xe0\0\x04ab\xff\xc0\0\x08\x08\x04\x38\x07\x80\0\xff\xd9";
        assert_eq!(dimensions(png), Some((1920, 1080)));
        assert_eq!(dimensions(jpeg), Some((1920, 1080)));
        for n in 0..jpeg.len() - 2 {
            assert_eq!(dimensions(&jpeg[..n]), None);
        }
    }
}
