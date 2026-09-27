//! Compare executable driver content independently of build ID and packaging.
//! The kernel loader still enforces original symbol CRCs; LKMs check live layout.
use crate::Result;
use sha2::{Digest, Sha256};

pub(crate) fn range(b: &[u8], off: u64, size: u64) -> Result<&[u8]> {
    let end = off.checked_add(size).ok_or("elf_range")?;
    b.get(
        usize::try_from(off).map_err(|_| "elf_range")?
            ..usize::try_from(end).map_err(|_| "elf_range")?,
    )
    .ok_or_else(|| "elf_range".into())
}
pub(crate) fn n(b: &[u8], off: usize, size: usize) -> Result<u64> {
    let v = range(b, off as u64, size as u64)?;
    Ok(v.iter()
        .enumerate()
        .fold(0, |a, (i, v)| a | ((*v as u64) << (8 * i))))
}
pub(crate) fn name(b: &[u8], off: u64) -> Result<String> {
    let b = b.get(off as usize..).ok_or("elf_string")?;
    let end = b.iter().position(|c| *c == 0).ok_or("elf_string")?;
    String::from_utf8(b[..end].to_vec()).map_err(|_| "elf_string".into())
}
fn part(hash: &mut Sha256, value: &[u8]) {
    hash.update((value.len() as u64).to_le_bytes());
    hash.update(value);
}
struct Section {
    name: String,
    kind: u64,
    flags: u64,
    offset: u64,
    size: u64,
    link: usize,
    info: usize,
    entry: u64,
}
impl Section {
    fn selected(&self) -> bool {
        self.flags & 2 != 0
            && !matches!(
                self.name.as_str(),
                ".modinfo" | "__versions" | ".gnu.linkonce.this_module"
            )
            && !self.name.starts_with(".note")
    }
}
pub fn digest(b: &[u8]) -> Result<String> {
    if b.len() > 32 * 1024 * 1024
        || b.get(..6) != Some(b"\x7fELF\x02\x01")
        || n(b, 16, 2)? != 1
        || n(b, 18, 2)? != 183
    {
        return Err("elf_driver_format".into());
    }
    let shoff = n(b, 40, 8)?;
    let count = n(b, 60, 2)? as usize;
    if count == 0 || count > 4096 || n(b, 58, 2)? != 64 {
        return Err("elf_sections".into());
    }
    let raw = range(b, shoff, (count * 64) as u64)?;
    let idx = n(b, 62, 2)? as usize;
    if idx >= count {
        return Err("elf_section_names".into());
    }
    let names = range(b, n(raw, idx * 64 + 24, 8)?, n(raw, idx * 64 + 32, 8)?)?;
    let mut sections = Vec::new();
    for i in 0..count {
        let p = i * 64;
        sections.push(Section {
            name: name(names, n(raw, p, 4)?)?,
            kind: n(raw, p + 4, 4)?,
            flags: n(raw, p + 8, 8)?,
            offset: n(raw, p + 24, 8)?,
            size: n(raw, p + 32, 8)?,
            link: n(raw, p + 40, 4)? as usize,
            info: n(raw, p + 44, 4)? as usize,
            entry: n(raw, p + 56, 8)?,
        });
    }
    if !sections.iter().any(|s| s.name == ".text" && s.selected()) {
        return Err("elf_text_missing".into());
    }
    let symbol = |table: usize, index: usize| -> Result<String> {
        let s = sections.get(table).ok_or("elf_symbols")?;
        if s.kind != 2 || s.entry != 24 || (index as u64) >= s.size / 24 {
            return Err("elf_symbol".into());
        }
        let data = range(b, s.offset + index as u64 * 24, 24)?;
        let strings = sections.get(s.link).ok_or("elf_symbol_strings")?;
        let label = name(range(b, strings.offset, strings.size)?, n(data, 0, 4)?)?;
        let section = n(data, 6, 2)? as usize;
        let target = sections
            .get(section)
            .map(|s| s.name.as_str())
            .unwrap_or("ABS");
        Ok(format!(
            "{label}|{target}|{}|{}|{}",
            n(data, 4, 1)?,
            n(data, 8, 8)?,
            n(data, 16, 8)?
        ))
    };
    let mut selected = sections
        .iter()
        .enumerate()
        .filter(|(_, s)| s.selected())
        .collect::<Vec<_>>();
    selected.sort_by(|a, b| a.1.name.cmp(&b.1.name));
    let mut hash = Sha256::new();
    part(&mut hash, b"cg-driver-contract-v1");
    for (index, s) in selected {
        part(&mut hash, s.name.as_bytes());
        part(&mut hash, &s.kind.to_le_bytes());
        part(&mut hash, &s.flags.to_le_bytes());
        part(&mut hash, &s.size.to_le_bytes());
        if s.kind != 8 {
            part(&mut hash, range(b, s.offset, s.size)?);
        }
        let mut refs = Vec::new();
        for r in sections.iter().filter(|r| r.kind == 4 && r.info == index) {
            if r.entry != 24 || r.size % 24 != 0 {
                return Err("elf_relocations".into());
            }
            let bytes = range(b, r.offset, r.size)?;
            for at in (0..bytes.len()).step_by(24) {
                let info = n(bytes, at + 8, 8)?;
                refs.push(format!(
                    "{}|{}|{}|{}",
                    n(bytes, at, 8)?,
                    info as u32,
                    n(bytes, at + 16, 8)?,
                    symbol(r.link, (info >> 32) as usize)?
                ));
            }
        }
        refs.sort();
        for reference in refs {
            part(&mut hash, reference.as_bytes());
        }
    }
    Ok(format!("{:x}", hash.finalize()))
}
