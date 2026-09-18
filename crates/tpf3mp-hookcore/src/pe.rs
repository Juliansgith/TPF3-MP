//! A minimal, read-only PE32+ header reader.
//!
//! Just enough to derive a build's identity (timestamp, image size) and to find
//! a code section to scan. It parses bytes by offset and never executes or maps
//! anything, so it is safe to run over an on-disk executable. Only 64-bit PE
//! (PE32+) images are supported, which is all TPF2/TPF3 ship on the desktop.

use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PeError {
    #[error("file is too small to be a PE image")]
    TooSmall,
    #[error("missing the MZ signature")]
    BadDosMagic,
    #[error("missing the PE signature")]
    BadPeMagic,
    #[error("not a 64-bit (PE32+) image")]
    NotPe32Plus,
    #[error("header runs past the end of the file")]
    Truncated,
}

/// One section header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    pub name: String,
    pub virtual_address: u32,
    pub virtual_size: u32,
    pub pointer_to_raw_data: u32,
    pub size_of_raw_data: u32,
}

impl Section {
    /// The on-disk bytes of this section within the whole image.
    pub fn raw<'a>(&self, image: &'a [u8]) -> Option<&'a [u8]> {
        let start = self.pointer_to_raw_data as usize;
        let end = start.checked_add(self.size_of_raw_data as usize)?;
        image.get(start..end)
    }
}

/// The fields of a PE image this crate needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeHeaders {
    pub machine: u16,
    pub timestamp: u32,
    pub image_base: u64,
    pub size_of_image: u32,
    pub sections: Vec<Section>,
}

impl PeHeaders {
    pub fn parse(image: &[u8]) -> Result<Self, PeError> {
        if image.len() < 0x40 {
            return Err(PeError::TooSmall);
        }
        if &image[0..2] != b"MZ" {
            return Err(PeError::BadDosMagic);
        }
        let pe_off = read_u32(image, 0x3C).ok_or(PeError::Truncated)? as usize;
        if image.get(pe_off..pe_off + 4) != Some(b"PE\0\0") {
            return Err(PeError::BadPeMagic);
        }
        let coff = pe_off + 4;
        let machine = read_u16(image, coff).ok_or(PeError::Truncated)?;
        let section_count = read_u16(image, coff + 2).ok_or(PeError::Truncated)? as usize;
        let timestamp = read_u32(image, coff + 4).ok_or(PeError::Truncated)?;
        let optional_size = read_u16(image, coff + 16).ok_or(PeError::Truncated)? as usize;
        let optional = coff + 20;
        let magic = read_u16(image, optional).ok_or(PeError::Truncated)?;
        if magic != 0x20B {
            return Err(PeError::NotPe32Plus);
        }
        let image_base = read_u64(image, optional + 24).ok_or(PeError::Truncated)?;
        let size_of_image = read_u32(image, optional + 56).ok_or(PeError::Truncated)?;

        let mut sections = Vec::with_capacity(section_count);
        let table = optional + optional_size;
        for index in 0..section_count {
            let entry = table + index * 40;
            let name_bytes = image.get(entry..entry + 8).ok_or(PeError::Truncated)?;
            let name_len = name_bytes.iter().position(|&b| b == 0).unwrap_or(8);
            let name = String::from_utf8_lossy(&name_bytes[..name_len]).into_owned();
            sections.push(Section {
                name,
                virtual_size: read_u32(image, entry + 8).ok_or(PeError::Truncated)?,
                virtual_address: read_u32(image, entry + 12).ok_or(PeError::Truncated)?,
                size_of_raw_data: read_u32(image, entry + 16).ok_or(PeError::Truncated)?,
                pointer_to_raw_data: read_u32(image, entry + 20).ok_or(PeError::Truncated)?,
            });
        }
        Ok(Self {
            machine,
            timestamp,
            image_base,
            size_of_image,
            sections,
        })
    }

    /// The section with this exact name (e.g. `.text`).
    pub fn section(&self, name: &str) -> Option<&Section> {
        self.sections.iter().find(|s| s.name == name)
    }
}

fn read_u16(bytes: &[u8], at: usize) -> Option<u16> {
    bytes
        .get(at..at + 2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
}

fn read_u32(bytes: &[u8], at: usize) -> Option<u32> {
    bytes
        .get(at..at + 4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

fn read_u64(bytes: &[u8], at: usize) -> Option<u64> {
    bytes
        .get(at..at + 8)
        .map(|b| u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A hand-built minimal PE32+ with one `.text` section, to prove the offsets.
    fn tiny_pe() -> Vec<u8> {
        let mut image = vec![0u8; 0x400];
        image[0..2].copy_from_slice(b"MZ");
        let pe_off = 0x80u32;
        image[0x3C..0x40].copy_from_slice(&pe_off.to_le_bytes());
        let pe = pe_off as usize;
        image[pe..pe + 4].copy_from_slice(b"PE\0\0");
        let coff = pe + 4;
        image[coff..coff + 2].copy_from_slice(&0x8664u16.to_le_bytes()); // machine
        image[coff + 2..coff + 4].copy_from_slice(&1u16.to_le_bytes()); // sections
        image[coff + 4..coff + 8].copy_from_slice(&0x675A_BCC6u32.to_le_bytes()); // timestamp
        let optional_size = 0xF0u16;
        image[coff + 16..coff + 18].copy_from_slice(&optional_size.to_le_bytes());
        let optional = coff + 20;
        image[optional..optional + 2].copy_from_slice(&0x20Bu16.to_le_bytes()); // PE32+
        image[optional + 24..optional + 32].copy_from_slice(&0x1_4000_0000u64.to_le_bytes());
        image[optional + 56..optional + 60].copy_from_slice(&0x0046_CE00u32.to_le_bytes());
        let table = optional + optional_size as usize;
        image[table..table + 5].copy_from_slice(b".text");
        image[table + 8..table + 12].copy_from_slice(&0x2000u32.to_le_bytes()); // virtual size
        image[table + 12..table + 16].copy_from_slice(&0x1000u32.to_le_bytes()); // virtual addr
        image[table + 16..table + 20].copy_from_slice(&0x2000u32.to_le_bytes()); // raw size
        image[table + 20..table + 24].copy_from_slice(&0x400u32.to_le_bytes()); // raw ptr
        image
    }

    #[test]
    fn parses_a_minimal_image() {
        let image = tiny_pe();
        let pe = PeHeaders::parse(&image).unwrap();
        assert_eq!(pe.machine, 0x8664);
        assert_eq!(pe.timestamp, 0x675A_BCC6);
        assert_eq!(pe.image_base, 0x1_4000_0000);
        let text = pe.section(".text").unwrap();
        assert_eq!(text.virtual_address, 0x1000);
        assert_eq!(text.pointer_to_raw_data, 0x400);
    }

    #[test]
    fn rejects_non_pe_input() {
        assert_eq!(PeHeaders::parse(&[0u8; 8]), Err(PeError::TooSmall));
        assert_eq!(PeHeaders::parse(&[0u8; 0x80]), Err(PeError::BadDosMagic));
    }
}
