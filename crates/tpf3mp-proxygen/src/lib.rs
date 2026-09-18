//! Generates a Windows proxy DLL from any DLL's export table.
//!
//! TPF3's proxy candidate - the small DLL the executable imports from its own
//! folder, like TPF2's `alut.dll` - is not known until release. So rather than
//! hand-write a forwarder, this crate reads a DLL's exports and emits a proxy
//! that forwards every one of them to the renamed original (`foo.dll` ->
//! `foo_real.dll`). The forwarding is done by the linker from a `.def` file
//! (`name=foo_real.name`), the same mechanism the TPF2 `alut.dll` proxy used,
//! so the proxy needs no code of its own beyond an optional load hook.
//!
//! Parsing is pure byte reading over the file image (see [`parse_exports`]);
//! generation is pure string building (see [`generate`]). Neither needs to run
//! on Windows, so the whole crate builds and unit-tests on every platform; only
//! the end-to-end "build the proxy and re-read it" test is Windows-gated.

use std::io;
use std::path::{Path, PathBuf};

use thiserror::Error;
use tpf3mp_hookcore::pe::{PeError, PeHeaders};

#[derive(Debug, Error)]
pub enum ProxyError {
    #[error("not a PE image: {0}")]
    Pe(#[from] PeError),
    #[error("the DLL has no export table")]
    NoExports,
    #[error("the export table runs past the end of the file")]
    Truncated,
    #[error("the export table is implausibly large ({0} functions)")]
    TooManyExports(u32),
}

/// One exported symbol.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Export {
    /// The export name, or `None` for an ordinal-only (NONAME) export.
    pub name: Option<String>,
    /// The export ordinal.
    pub ordinal: u16,
    /// Whether this export is itself a forwarder (its address points into the
    /// export directory).
    pub forwarder: bool,
}

/// A DLL's export table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exports {
    /// The internal module name from the export directory.
    pub module_name: String,
    /// The ordinal base.
    pub base: u32,
    pub entries: Vec<Export>,
}

impl Exports {
    /// The named exports, in ordinal order.
    pub fn named(&self) -> impl Iterator<Item = (&str, u16)> {
        self.entries
            .iter()
            .filter_map(|e| e.name.as_deref().map(|name| (name, e.ordinal)))
    }
}

const MAX_EXPORTS: u32 = 1 << 18;

/// Reads a DLL's export table from its file image.
pub fn parse_exports(image: &[u8]) -> Result<Exports, ProxyError> {
    let pe = PeHeaders::parse(image)?;
    let (export_rva, export_size) = export_directory(image).ok_or(ProxyError::NoExports)?;
    if export_rva == 0 || export_size == 0 {
        return Err(ProxyError::NoExports);
    }
    let dir = rva_to_offset(&pe, export_rva).ok_or(ProxyError::Truncated)?;
    let header = image.get(dir..dir + 40).ok_or(ProxyError::Truncated)?;
    let name_rva = read_u32(header, 12);
    let base = read_u32(header, 16);
    let function_count = read_u32(header, 20);
    let name_count = read_u32(header, 24);
    let functions_rva = read_u32(header, 28);
    let names_rva = read_u32(header, 32);
    let ordinals_rva = read_u32(header, 36);
    if function_count > MAX_EXPORTS || name_count > MAX_EXPORTS {
        return Err(ProxyError::TooManyExports(function_count.max(name_count)));
    }

    let module_name = rva_to_offset(&pe, name_rva)
        .map(|off| read_cstr(image, off))
        .unwrap_or_default();

    let functions_off = rva_to_offset(&pe, functions_rva).ok_or(ProxyError::Truncated)?;
    let names_off = rva_to_offset(&pe, names_rva);
    let ordinals_off = rva_to_offset(&pe, ordinals_rva);

    // Map each function slot to its name (if any).
    let mut name_of: Vec<Option<String>> = vec![None; function_count as usize];
    if let (Some(names_off), Some(ordinals_off)) = (names_off, ordinals_off) {
        for i in 0..name_count as usize {
            let name_ptr = read_u32_at(image, names_off + i * 4).ok_or(ProxyError::Truncated)?;
            let slot = read_u16_at(image, ordinals_off + i * 2).ok_or(ProxyError::Truncated)?;
            let name = rva_to_offset(&pe, name_ptr)
                .map(|off| read_cstr(image, off))
                .unwrap_or_default();
            if let Some(entry) = name_of.get_mut(slot as usize) {
                *entry = Some(name);
            }
        }
    }

    let mut entries = Vec::new();
    for index in 0..function_count as usize {
        let function_rva =
            read_u32_at(image, functions_off + index * 4).ok_or(ProxyError::Truncated)?;
        if function_rva == 0 {
            continue; // empty ordinal slot
        }
        let ordinal = base + index as u32;
        let forwarder = function_rva >= export_rva && function_rva < export_rva + export_size;
        entries.push(Export {
            name: name_of.get_mut(index).and_then(Option::take),
            ordinal: ordinal as u16,
            forwarder,
        });
    }

    Ok(Exports {
        module_name,
        base,
        entries,
    })
}

/// The files that make up a generated proxy crate.
#[derive(Debug, Clone)]
pub struct ProxyFiles {
    /// The linker `.def` with one forwarder per export, kept as the readable
    /// manifest and usable directly with `link /DLL /NOENTRY /DEF:`.
    pub def: String,
    pub def_file_name: String,
    pub cargo_toml: String,
    /// The crate source: the forwarders embedded as `.drectve` linker
    /// directives, which is what cargo actually builds.
    pub lib_rs: String,
}

/// Builds the proxy source for `proxy_stem.dll` forwarding to `real_stem.dll`.
pub fn generate(exports: &Exports, proxy_stem: &str, real_stem: &str) -> ProxyFiles {
    let lib_name = sanitize_ident(proxy_stem);
    let def_file_name = format!("{proxy_stem}.def");

    // The .def is the readable manifest. `directives` is the same set of
    // forwarders as command-line `/EXPORT` directives, which get embedded in the
    // `.drectve` section below.
    let mut def = String::from("; Generated by tpf3mp-proxygen. Every export forwards to the\n");
    def.push_str(&format!("; renamed original ({real_stem}.dll).\nEXPORTS\n"));
    let mut directives = String::new();
    for export in &exports.entries {
        match &export.name {
            Some(name) => {
                def.push_str(&format!("    {name}={real_stem}.{name}\n"));
                directives.push_str(&format!(" /EXPORT:{name}={real_stem}.{name}"));
            }
            None => {
                // Ordinal-only export: forward by ordinal, keep it nameless.
                let ord = export.ordinal;
                def.push_str(&format!(
                    "    proxy_ordinal_{ord}={real_stem}.#{ord} @{ord} NONAME\n"
                ));
                directives.push_str(&format!(
                    " /EXPORT:proxy_ordinal_{ord}={real_stem}.#{ord},@{ord},NONAME"
                ));
            }
        }
    }

    let cargo_toml = format!(
        "[package]\n\
         name = \"{proxy_stem}-proxy\"\n\
         version = \"0.0.0\"\n\
         edition = \"2021\"\n\
         publish = false\n\n\
         [lib]\n\
         name = \"{lib_name}\"\n\
         crate-type = [\"cdylib\"]\n"
    );

    // The forwarders live in the object's `.drectve` section, exactly as a C++
    // proxy's `#pragma comment(linker, "/export:...")` does. This coexists with
    // the `.def` rustc generates for the cdylib, unlike passing a second `/DEF`.
    let directive_len = directives.len();
    let lib_rs = format!(
        "//! Generated proxy for `{proxy_stem}.dll`, forwarding every export to\n\
         //! `{real_stem}.dll`. Rename the stock `{proxy_stem}.dll` to\n\
         //! `{real_stem}.dll` and drop the built `{proxy_stem}.dll` beside it.\n\
         //!\n\
         //! The export forwarders are emitted as `/EXPORT` linker directives in\n\
         //! the `.drectve` section (mirroring `{def_file_name}`). Add a `DllMain`\n\
         //! here if the proxy also needs to bootstrap the hook.\n\
         \n\
         #[used]\n\
         #[unsafe(link_section = \".drectve\")]\n\
         static EXPORT_DIRECTIVES: [u8; {directive_len}] = *b\"{directives}\";\n"
    );

    ProxyFiles {
        def,
        def_file_name,
        cargo_toml,
        lib_rs,
    }
}

/// Writes a generated proxy crate into `dir`, returning the crate root.
pub fn write_proxy(
    dir: &Path,
    exports: &Exports,
    proxy_stem: &str,
    real_stem: &str,
) -> io::Result<PathBuf> {
    let files = generate(exports, proxy_stem, real_stem);
    std::fs::create_dir_all(dir.join("src"))?;
    std::fs::write(dir.join("Cargo.toml"), files.cargo_toml)?;
    std::fs::write(dir.join("src").join("lib.rs"), files.lib_rs)?;
    std::fs::write(dir.join(&files.def_file_name), files.def)?;
    Ok(dir.to_path_buf())
}

fn sanitize_ident(stem: &str) -> String {
    let mut out: String = stem
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    if out.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        out.insert(0, '_');
    }
    out
}

/// Reads the export data directory (RVA, size) from the optional header.
fn export_directory(image: &[u8]) -> Option<(u32, u32)> {
    let pe_off = read_u32_at(image, 0x3C)? as usize;
    let optional = pe_off + 4 + 20;
    if read_u16_at(image, optional)? != 0x20B {
        return None; // PE32+ only
    }
    if read_u32_at(image, optional + 108)? < 1 {
        return None; // no data directories
    }
    let entry = optional + 112; // DataDirectory[0] = export table
    Some((read_u32_at(image, entry)?, read_u32_at(image, entry + 4)?))
}

fn rva_to_offset(pe: &PeHeaders, rva: u32) -> Option<usize> {
    for section in &pe.sections {
        let start = section.virtual_address;
        let virtual_end = start.checked_add(section.virtual_size.max(section.size_of_raw_data))?;
        if rva >= start && rva < virtual_end {
            let delta = rva - start;
            if delta < section.size_of_raw_data {
                return Some((section.pointer_to_raw_data + delta) as usize);
            }
            return None;
        }
    }
    None
}

fn read_u32(bytes: &[u8], at: usize) -> u32 {
    read_u32_at(bytes, at).unwrap_or(0)
}

fn read_u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    bytes
        .get(at..at + 4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

fn read_u16_at(bytes: &[u8], at: usize) -> Option<u16> {
    bytes
        .get(at..at + 2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
}

fn read_cstr(bytes: &[u8], at: usize) -> String {
    let slice = bytes.get(at..).unwrap_or(&[]);
    let end = slice.iter().position(|&b| b == 0).unwrap_or(slice.len());
    String::from_utf8_lossy(&slice[..end]).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a minimal PE32+ whose export table names `names`, so the parser
    /// can be exercised without a real DLL. Functions point outside the export
    /// directory, so none are forwarders.
    fn export_pe(module: &str, names: &[&str]) -> Vec<u8> {
        let n = names.len();
        // Section .rdata: virtual 0x1000, raw at 0x400.
        let sec_rva = 0x1000u32;
        let sec_raw = 0x400usize;
        let mut image = vec![0u8; sec_raw + 0x400];

        // DOS + PE + COFF + optional headers.
        image[0..2].copy_from_slice(b"MZ");
        let pe_off = 0x80usize;
        image[0x3C..0x40].copy_from_slice(&(pe_off as u32).to_le_bytes());
        image[pe_off..pe_off + 4].copy_from_slice(b"PE\0\0");
        let coff = pe_off + 4;
        image[coff..coff + 2].copy_from_slice(&0x8664u16.to_le_bytes());
        image[coff + 2..coff + 4].copy_from_slice(&1u16.to_le_bytes()); // 1 section
        let optional_size = 0xF0u16; // 112 + 16*8 - trimmed; enough for our reads
        image[coff + 16..coff + 18].copy_from_slice(&optional_size.to_le_bytes());
        let optional = coff + 20;
        image[optional..optional + 2].copy_from_slice(&0x20Bu16.to_le_bytes());
        image[optional + 108..optional + 112].copy_from_slice(&16u32.to_le_bytes()); // NumberOfRvaAndSizes

        // Lay out the export structures inside the section.
        let dir_off = sec_raw;
        let functions_off = dir_off + 40;
        let names_off = functions_off + n * 4;
        let ordinals_off = names_off + n * 4;
        let module_off = ordinals_off + n * 2;
        let mut cursor = module_off + module.len() + 1;
        let mut name_offsets = Vec::new();
        for name in names {
            name_offsets.push(cursor);
            cursor += name.len() + 1;
        }
        let to_rva = |file_off: usize| sec_rva + (file_off - sec_raw) as u32;

        // Export directory.
        image[dir_off + 12..dir_off + 16].copy_from_slice(&to_rva(module_off).to_le_bytes());
        image[dir_off + 16..dir_off + 20].copy_from_slice(&1u32.to_le_bytes()); // base
        image[dir_off + 20..dir_off + 24].copy_from_slice(&(n as u32).to_le_bytes());
        image[dir_off + 24..dir_off + 28].copy_from_slice(&(n as u32).to_le_bytes());
        image[dir_off + 28..dir_off + 32].copy_from_slice(&to_rva(functions_off).to_le_bytes());
        image[dir_off + 32..dir_off + 36].copy_from_slice(&to_rva(names_off).to_le_bytes());
        image[dir_off + 36..dir_off + 40].copy_from_slice(&to_rva(ordinals_off).to_le_bytes());

        for i in 0..n {
            // Function RVA: outside the export dir, so not a forwarder.
            image[functions_off + i * 4..functions_off + i * 4 + 4]
                .copy_from_slice(&0x9000u32.to_le_bytes());
            image[names_off + i * 4..names_off + i * 4 + 4]
                .copy_from_slice(&to_rva(name_offsets[i]).to_le_bytes());
            image[ordinals_off + i * 2..ordinals_off + i * 2 + 2]
                .copy_from_slice(&(i as u16).to_le_bytes());
        }
        image[module_off..module_off + module.len()].copy_from_slice(module.as_bytes());
        for (name, &off) in names.iter().zip(name_offsets.iter()) {
            image[off..off + name.len()].copy_from_slice(name.as_bytes());
        }

        // Section header.
        let table = optional + optional_size as usize;
        image[table..table + 6].copy_from_slice(b".rdata");
        image[table + 8..table + 12].copy_from_slice(&0x400u32.to_le_bytes()); // virtual size
        image[table + 12..table + 16].copy_from_slice(&sec_rva.to_le_bytes());
        image[table + 16..table + 20].copy_from_slice(&0x400u32.to_le_bytes()); // raw size
        image[table + 20..table + 24].copy_from_slice(&(sec_raw as u32).to_le_bytes());

        // Export data directory entry.
        image[optional + 112..optional + 116].copy_from_slice(&sec_rva.to_le_bytes());
        image[optional + 116..optional + 120].copy_from_slice(&0x300u32.to_le_bytes());

        image
    }

    #[test]
    fn parses_named_exports() {
        let image = export_pe("sample.dll", &["alutInit", "alutExit", "alutGetError"]);
        let exports = parse_exports(&image).unwrap();
        assert_eq!(exports.module_name, "sample.dll");
        assert_eq!(exports.base, 1);
        let names: Vec<_> = exports.named().map(|(n, o)| (n.to_string(), o)).collect();
        assert_eq!(
            names,
            vec![
                ("alutInit".to_string(), 1),
                ("alutExit".to_string(), 2),
                ("alutGetError".to_string(), 3),
            ]
        );
        assert!(exports.entries.iter().all(|e| !e.forwarder));
    }

    #[test]
    fn generated_def_forwards_every_export() {
        let image = export_pe("alut.dll", &["alutInit", "alutExit"]);
        let exports = parse_exports(&image).unwrap();
        let files = generate(&exports, "alut", "alut_real");
        assert!(files.def.contains("alutInit=alut_real.alutInit"));
        assert!(files.def.contains("alutExit=alut_real.alutExit"));
        assert!(
            !files.def.contains('@'),
            "named forwarders carry no explicit ordinal"
        );
        assert!(files.cargo_toml.contains("name = \"alut\""));
        assert_eq!(files.def_file_name, "alut.def");
        // The crate carries the forwarders as .drectve /EXPORT directives.
        assert!(files.lib_rs.contains(".drectve"));
        assert!(files.lib_rs.contains("/EXPORT:alutInit=alut_real.alutInit"));
        assert!(files.lib_rs.contains("/EXPORT:alutExit=alut_real.alutExit"));
    }

    #[test]
    fn non_pe_input_is_rejected() {
        assert!(matches!(parse_exports(&[0u8; 16]), Err(ProxyError::Pe(_))));
    }

    #[test]
    fn sanitize_ident_makes_a_valid_lib_name() {
        assert_eq!(sanitize_ident("alut"), "alut");
        assert_eq!(sanitize_ident("lib-foo.bar"), "lib_foo_bar");
        assert_eq!(sanitize_ident("3d"), "_3d");
    }
}
