//! Read-only, per-interface ELF evidence. No product names or whole-image allowlist.
//! Function placement, unrelated driver code and build metadata are not contracts.
use crate::driver_contract::{n, name, range};
use crate::Result;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

#[derive(Clone)]
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
#[derive(Clone)]
struct Symbol {
    name: String,
    kind: u8,
    section: usize,
    value: u64,
    size: u64,
}
pub struct Elf<'a> {
    bytes: &'a [u8],
    sections: Vec<Section>,
    symbols: Vec<Symbol>,
    symtab: usize,
}
fn part(h: &mut Sha256, b: &[u8]) {
    h.update((b.len() as u64).to_le_bytes());
    h.update(b);
}
impl<'a> Elf<'a> {
    pub fn parse(bytes: &'a [u8]) -> Result<Self> {
        if bytes.len() > 32 * 1024 * 1024
            || bytes.get(..6) != Some(b"\x7fELF\x02\x01")
            || n(bytes, 16, 2)? != 1
            || n(bytes, 18, 2)? != 183
            || n(bytes, 58, 2)? != 64
        {
            return Err("probe_elf_format".into());
        }
        let count = n(bytes, 60, 2)? as usize;
        let strings_index = n(bytes, 62, 2)? as usize;
        if count == 0 || count > 4096 || strings_index >= count {
            return Err("probe_elf_sections".into());
        }
        let raw = range(bytes, n(bytes, 40, 8)?, count as u64 * 64)?;
        let p = strings_index * 64;
        let strings = range(bytes, n(raw, p + 24, 8)?, n(raw, p + 32, 8)?)?;
        let mut sections = Vec::new();
        for p in (0..raw.len()).step_by(64) {
            sections.push(Section {
                name: name(strings, n(raw, p, 4)?)?,
                kind: n(raw, p + 4, 4)?,
                flags: n(raw, p + 8, 8)?,
                offset: n(raw, p + 24, 8)?,
                size: n(raw, p + 32, 8)?,
                link: n(raw, p + 40, 4)? as usize,
                info: n(raw, p + 44, 4)? as usize,
                entry: n(raw, p + 56, 8)?,
            });
        }
        for s in &sections {
            if s.kind != 8 {
                range(bytes, s.offset, s.size)?;
            }
        }
        let tables: Vec<_> = sections
            .iter()
            .enumerate()
            .filter(|(_, s)| s.kind == 2)
            .collect();
        if tables.len() != 1 {
            return Err("probe_symbol_table".into());
        }
        let (symtab, table) = tables[0];
        if table.entry != 24 || table.size % 24 != 0 || table.size > 8 * 1024 * 1024 {
            return Err("probe_symbols_size".into());
        }
        let strings = sections.get(table.link).ok_or("probe_strings")?;
        let strings = range(bytes, strings.offset, strings.size)?;
        let raw = range(bytes, table.offset, table.size)?;
        let mut symbols = Vec::new();
        for p in (0..raw.len()).step_by(24) {
            symbols.push(Symbol {
                name: name(strings, n(raw, p, 4)?)?,
                kind: (n(raw, p + 4, 1)? & 15) as u8,
                section: n(raw, p + 6, 2)? as usize,
                value: n(raw, p + 8, 8)?,
                size: n(raw, p + 16, 8)?,
            });
        }
        Ok(Self {
            bytes,
            sections,
            symbols,
            symtab,
        })
    }
    pub fn section(&self, wanted: &str) -> Result<&[u8]> {
        let mut matches = self.sections.iter().filter(|s| s.name == wanted);
        let s = matches
            .next()
            .ok_or_else(|| format!("probe_section_missing:{wanted}"))?;
        if matches.next().is_some() || s.kind == 8 {
            return Err("probe_section_ambiguous".into());
        }
        range(self.bytes, s.offset, s.size)
    }
    fn symbol(&self, wanted: &str) -> Result<&Symbol> {
        let mut matches = self
            .symbols
            .iter()
            .filter(|s| s.name == wanted && s.section != 0 && s.size > 0);
        let s = matches
            .next()
            .ok_or_else(|| format!("probe_symbol_missing:{wanted}"))?;
        if matches.next().is_some() {
            return Err(format!("probe_symbol_ambiguous:{wanted}"));
        }
        Ok(s)
    }
    fn data(&self, s: &Symbol) -> Result<&[u8]> {
        let section = self.sections.get(s.section).ok_or("probe_symbol_section")?;
        if section.kind == 8 || s.value.checked_add(s.size).is_none_or(|e| e > section.size) {
            return Err("probe_symbol_bounds".into());
        }
        range(self.bytes, section.offset + s.value, s.size)
    }
    // A section-symbol relocation is named by its target object, not its address.
    // String literals are compared by content; unknown anonymous targets fail closed.
    fn target(&self, section: usize, at: u64) -> Result<String> {
        let sec = self.sections.get(section).ok_or("probe_target_section")?;
        if sec.flags & 0x20 != 0 && sec.kind != 8 {
            let text = name(range(self.bytes, sec.offset, sec.size)?, at)?;
            return Ok(format!("string:{text}"));
        }
        let matches: Vec<_> = self
            .symbols
            .iter()
            .filter(|s| {
                s.section == section
                    && matches!(s.kind, 1 | 2)
                    && !s.name.is_empty()
                    && s.value <= at
                    && s.value.checked_add(s.size).is_some_and(|e| at < e)
            })
            .collect();
        if let Some(s) = matches.iter().min_by_key(|s| s.size) {
            return Ok(format!("symbol:{}+{}", s.name, at - s.value));
        }
        // Compiler switch tables / constant pools have no STT_OBJECT label.
        // Compare the complete bounded anonymous tail up to the next named object.
        let end = self
            .symbols
            .iter()
            .filter(|s| s.section == section && s.size > 0 && s.value > at)
            .map(|s| s.value)
            .min()
            .unwrap_or(sec.size);
        if sec.kind != 1 || sec.flags & 5 != 0 || end <= at || end - at > 4096 {
            return Err(format!("probe_unnamed_target:{}:{at:x}", sec.name));
        }
        for r in self
            .sections
            .iter()
            .filter(|r| r.kind == 4 && r.info == section)
        {
            if r.entry != 24 || r.size % 24 != 0 {
                return Err("probe_literal_reloc".into());
            }
            let raw = range(self.bytes, r.offset, r.size)?;
            for p in (0..raw.len()).step_by(24) {
                let off = n(raw, p, 8)?;
                if off >= at && off < end {
                    return Err("probe_literal_reloc".into());
                }
            }
        }
        Ok(format!(
            "literal:{:x}",
            Sha256::digest(range(self.bytes, sec.offset + at, end - at)?)
        ))
    }
    fn relocation_target(&self, index: usize, addend: i64) -> Result<String> {
        let s = self.symbols.get(index).ok_or("probe_reloc_symbol")?;
        if s.section == 0 || s.section >= 0xff00 {
            return Ok(format!("external:{}:{addend}", s.name));
        }
        let at = s
            .value
            .checked_add_signed(addend)
            .ok_or("probe_reloc_addend")?;
        if s.kind != 3 && !s.name.is_empty() {
            // Symbol aliases, statics and zero-size linker labels retain their name.
            return Ok(format!("symbol:{}+{addend}", s.name));
        }
        self.target(s.section, at)
    }
    pub fn fingerprint(&self, wanted: &str) -> Result<String> {
        let s = self.symbol(wanted)?;
        if !matches!(s.kind, 1 | 2) || s.size > 256 * 1024 {
            return Err("probe_symbol_kind_or_size".into());
        }
        let mut bytes = self.data(s)?.to_vec();
        let mut refs = BTreeMap::new();
        for r in self
            .sections
            .iter()
            .filter(|r| r.kind == 4 && r.info == s.section)
        {
            if r.entry != 24 || r.size % 24 != 0 || r.link != self.symtab {
                return Err("probe_relocations".into());
            }
            let raw = range(self.bytes, r.offset, r.size)?;
            for p in (0..raw.len()).step_by(24) {
                let at = n(raw, p, 8)?;
                if at < s.value || at >= s.value + s.size {
                    continue;
                }
                let off = (at - s.value) as usize;
                let info = n(raw, p + 8, 8)?;
                let kind = info as u32;
                let target =
                    self.relocation_target((info >> 32) as usize, n(raw, p + 16, 8)? as i64)?;
                let (width, mask) = match kind {
                    257 => (8, 0),                                        // ABS64
                    258 | 261 => (4, 0),                                  // ABS32/PREL32
                    275 | 276 => (4, 0x9f00001f),                         // ADRP
                    274 => (4, 0x9f00001f),                               // ADR
                    277 | 278 | 284 | 285 | 286 | 299 => (4, 0xffc003ff), // ADD/LDST low12
                    282 | 283 => (4, 0xfc000000),                         // B/BL
                    _ => return Err(format!("probe_relocation_kind:{kind}")),
                };
                let chunk = bytes
                    .get_mut(off..off + width)
                    .ok_or("probe_reloc_bounds")?;
                if width == 8 {
                    chunk.fill(0);
                } else {
                    let word = u32::from_le_bytes(chunk.try_into().unwrap()) & mask;
                    chunk.copy_from_slice(&word.to_le_bytes());
                }
                if refs.insert(off, format!("reloc:{kind}:{target}")).is_some() {
                    return Err("probe_reloc_duplicate".into());
                }
            }
        }
        if s.kind == 2 {
            if bytes.len() % 4 != 0 {
                return Err("probe_instructions".into());
            }
            // ld -r can resolve same-section direct calls before producing the ELF.
            // Preserve local control flow; name calls/tail calls leaving the function.
            for off in (0..bytes.len()).step_by(4) {
                if refs.contains_key(&off) {
                    continue;
                }
                let word = u32::from_le_bytes(bytes[off..off + 4].try_into().unwrap());
                if word & 0x7c000000 != 0x14000000 {
                    continue;
                }
                let delta = (((word & 0x03ffffff) << 6) as i32 >> 4) as i64;
                let at = (s.value + off as u64)
                    .checked_add_signed(delta)
                    .ok_or("probe_branch_range")?;
                if at >= s.value && at < s.value + s.size {
                    continue;
                }
                refs.insert(off, format!("branch:{}", self.target(s.section, at)?));
                bytes[off..off + 4].copy_from_slice(&(word & 0xfc000000).to_le_bytes());
            }
        }
        let mut hash = Sha256::new();
        part(&mut hash, b"cg-interface-contract-v1");
        part(&mut hash, wanted.as_bytes());
        part(&mut hash, &[s.kind]);
        part(&mut hash, &bytes);
        for (at, target) in refs {
            part(&mut hash, &(at as u64).to_le_bytes());
            part(&mut hash, target.as_bytes());
        }
        Ok(format!("{:x}", hash.finalize()))
    }
    pub fn word(&self, wanted: &str, offset: i64) -> Result<u32> {
        let s = self.symbol(wanted)?;
        if offset < -4 || offset >= s.size as i64 || offset % 4 != 0 {
            return Err("probe_word_bounds".into());
        }
        let sec = self.sections.get(s.section).ok_or("probe_word_section")?;
        let at = s
            .value
            .checked_add_signed(offset)
            .ok_or("probe_word_offset")?;
        Ok(n(range(self.bytes, sec.offset, sec.size)?, at as usize, 4)? as u32)
    }
    pub fn check_abi(&self, expected: &BTreeMap<String, u64>) -> Result<usize> {
        let raw = self.section("__versions")?;
        if raw.len() % 64 != 0 {
            return Err("probe_versions_format".into());
        }
        let mut found = BTreeMap::new();
        for p in (0..raw.len()).step_by(64) {
            if found
                .insert(name(&raw[p + 8..p + 64], 0)?, n(raw, p, 8)?)
                .is_some()
            {
                return Err("probe_versions_duplicate".into());
            }
        }
        if found.get("module_layout") != expected.get("module_layout")
            || !found.contains_key("module_layout")
        {
            return Err("probe_kernel_module_layout".into());
        }
        let mut checked = 0;
        for (symbol, crc) in expected {
            if let Some(actual) = found.get(symbol) {
                if actual != crc {
                    return Err(format!("probe_kernel_symbol_crc:{symbol}"));
                }
                checked += 1;
            }
        }
        // Imports absent from this reference module are still checked by the
        // kernel's ordinary loader. This result is eligibility, not a load test.
        Ok(checked)
    }
}

#[derive(Deserialize)]
struct Contracts {
    pps: BTreeMap<String, String>,
    power: BTreeMap<String, String>,
}
#[derive(Clone, Debug, Default, Serialize)]
pub struct Evidence {
    pub pps: bool,
    pub power: bool,
    pub pps_error: Option<String>,
    pub power_error: Option<String>,
}
pub fn inspect(bytes: &[u8]) -> Result<Evidence> {
    let elf = Elf::parse(bytes)?;
    let contracts: Contracts =
        serde_json::from_str(include_str!("../kernel/charging-contracts.json"))
            .map_err(|e| e.to_string())?;
    let check = |expected: &BTreeMap<String, String>| -> Result<()> {
        if expected.is_empty() {
            return Err("probe_contract_empty".into());
        }
        for (symbol, fingerprint) in expected {
            if elf.fingerprint(symbol)? != *fingerprint {
                return Err(format!("probe_interface_changed:{symbol}"));
            }
        }
        Ok(())
    };
    let pps = check(&contracts.pps);
    let power = (|| -> Result<()> {
        check(&contracts.power)?;
        let profile: serde_json::Value =
            serde_json::from_str(include_str!("../kernel/pjz110-power-target.json"))
                .map_err(|e| e.to_string())?;
        for (symbol, anchor) in profile["anchors"].as_object().ok_or("probe_anchors")? {
            if elf.word(symbol, -4)? as u64 != anchor["kcfi"].as_u64().ok_or("probe_kcfi")? {
                return Err(format!("probe_kcfi_changed:{symbol}"));
            }
            for i in 0..3 {
                if elf.word(symbol, (i * 4) as i64)? as u64
                    != anchor["entry"][i].as_u64().ok_or("probe_entry")?
                {
                    return Err(format!("probe_entry_changed:{symbol}"));
                }
            }
        }
        Ok(())
    })();
    Ok(Evidence {
        pps: pps.is_ok(),
        power: power.is_ok(),
        pps_error: pps.err(),
        power_error: power.err(),
    })
}
