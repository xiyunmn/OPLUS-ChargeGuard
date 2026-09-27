//! Device evidence is tied to loaded code, never inferred from a successful write.
use crate::hardware::Hardware;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, fs::File, io::Read};

pub const SHELL_IMAGE: &str = "eb5e1a6bb5281f72faff6cfee0334471fc9ce79cec37538b44bf4998a40b0c0f";
pub const SHELL_NOTE: &str = "bd782ec5169ca35f14a073b56a68a7d42a84d09ec44c9175bca2ec3ca4acbd2e";
pub const SHELL_IMAGE_COS17: &str =
    "034c75e352124e61436815d0e0b800de368c98405c8883e2ca1bb19c1b5eb45f";
pub const SHELL_NOTE_COS17: &str =
    "313f5425ed0d6c50c9aba5f6a034d5929c42e154f5cf985c2b67e6323f53fa37";

const VERIFIED_SHELL_DRIVERS: &[(&str, &str)] = &[
    (SHELL_IMAGE, SHELL_NOTE),
    (SHELL_IMAGE_COS17, SHELL_NOTE_COS17),
];

#[derive(Clone, Debug, Default, Serialize)]
pub struct Capabilities {
    pub shell_image_sha256: Option<String>,
    pub shell_loaded_note_sha256: Option<String>,
    pub shell_slots: Option<usize>,
    pub pps_driver_sha256: Option<String>,
    pub pps_driver_contract_sha256: Option<String>,
    pub pps_kernel_release: Option<String>,
    pub pps_verified: bool,
    pub power_verified: bool,
    pub pps_probe_error: Option<String>,
    pub power_probe_error: Option<String>,
    pub pps_loaded_note_sha256: Option<String>,
    pub kernel_abi_checked_symbols: usize,
    #[serde(skip)]
    pub evidence_stamp: String,
}
pub const CHARGING_DRIVER: &str = "/vendor_dlkm/lib/modules/oplus_chg_v2.ko";
pub const CHARGING_NOTE: &str = "/sys/module/oplus_chg_v2/notes/.note.gnu.build-id";
fn read_bytes(h: &Hardware, path: &str, limit: u64) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    File::open(h.path(path))
        .ok()?
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    (bytes.len() as u64 <= limit).then_some(bytes)
}
/// Cheap invalidation only; no periodic ELF disassembly or charging-state poll.
pub fn evidence_stamp(h: &Hardware) -> String {
    use std::os::unix::fs::MetadataExt;
    let mut stamp = String::new();
    for path in [
        CHARGING_DRIVER,
        "/vendor_dlkm/lib/modules/horae_shell_temp.ko",
    ] {
        stamp.push_str(&format!(
            "{:?}",
            std::fs::metadata(h.path(path)).ok().map(|m| (
                m.dev(),
                m.ino(),
                m.len(),
                m.mtime(),
                m.mtime_nsec(),
                m.ctime(),
                m.ctime_nsec()
            ))
        ));
    }
    for path in [
        CHARGING_NOTE,
        "/sys/module/horae_shell_temp/notes/.note.gnu.build-id",
    ] {
        stamp.push_str(&format!("{:?}", hash(h, path, 4096)));
    }
    stamp
}
pub fn confirm_loaded(h: &Hardware) -> crate::Result<()> {
    if h.fixture {
        return Ok(());
    }
    let expected = h.capabilities().pps_loaded_note_sha256;
    if expected.is_none() || hash(h, CHARGING_NOTE, 4096) != expected {
        return Err("charging_loaded_driver_changed".into());
    }
    Ok(())
}
fn hash(h: &Hardware, path: &str, limit: u64) -> Option<String> {
    let mut bytes = Vec::new();
    File::open(h.path(path))
        .ok()?
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    (bytes.len() as u64 <= limit).then(|| format!("{:x}", Sha256::digest(&bytes)))
}
pub fn shell_slots(image: Option<&str>, loaded_note: Option<&str>) -> Option<usize> {
    VERIFIED_SHELL_DRIVERS
        .iter()
        .any(|(known_image, known_note)| {
            image == Some(*known_image) && loaded_note == Some(*known_note)
        })
        .then_some(3)
}
impl Capabilities {
    pub fn discover(h: &Hardware) -> Self {
        let image = hash(
            h,
            "/vendor_dlkm/lib/modules/horae_shell_temp.ko",
            8 * 1024 * 1024,
        );
        let note = hash(
            h,
            "/sys/module/horae_shell_temp/notes/.note.gnu.build-id",
            4096,
        );
        let bytes = read_bytes(h, CHARGING_DRIVER, 32 * 1024 * 1024);
        let pps_image = bytes.as_ref().map(|b| format!("{:x}", Sha256::digest(b)));
        let kernel = h.read("/proc/sys/kernel/osrelease").ok();
        let loaded_note = read_bytes(h, CHARGING_NOTE, 4096);
        let loaded_hash = loaded_note
            .as_ref()
            .map(|b| format!("{:x}", Sha256::digest(b)));
        let mut evidence = crate::kernel_probe::Evidence::default();
        let mut abi_checked = 0;
        let common = (|| -> crate::Result<()> {
            let bytes = bytes.as_deref().ok_or("probe_driver_unreadable")?;
            let elf = crate::kernel_probe::Elf::parse(bytes)?;
            let note = elf.section(".note.gnu.build-id")?;
            if note.len() < 20 || loaded_note.as_deref() != Some(note) {
                return Err("probe_loaded_driver_mismatch".into());
            }
            evidence = crate::kernel_probe::inspect(bytes)?;
            let pps_abi: BTreeMap<String, u64> =
                serde_json::from_str(include_str!("../kernel/pjz110-symbols.json"))
                    .map_err(|e| e.to_string())?;
            let power_abi: BTreeMap<String, u64> =
                serde_json::from_str(include_str!("../kernel/pjz110-power-symbols.json"))
                    .map_err(|e| e.to_string())?;
            if let Err(e) = elf.check_abi(&pps_abi) {
                evidence.pps = false;
                evidence.pps_error = Some(e);
            }
            match elf.check_abi(&power_abi) {
                Ok(n) => abi_checked = n,
                Err(e) => {
                    evidence.power = false;
                    evidence.power_error = Some(e);
                }
            }
            if evidence.power {
                for (key, expected) in [
                    ("oplus,vooc_data_width", 7u32),
                    ("oplus,vooc_curr_table_type", 2),
                    ("oplus,vooc_curr_max", 19),
                ] {
                    let path = format!("/sys/firmware/devicetree/base/soc/oplus,vooc/{key}");
                    let value = read_bytes(h, &path, 4)
                        .and_then(|b| <[u8; 4]>::try_from(b).ok())
                        .map(u32::from_be_bytes);
                    if value != Some(expected) {
                        evidence.power = false;
                        evidence.power_error = Some(format!("probe_vooc_table:{key}"));
                        break;
                    }
                }
            }
            Ok(())
        })();
        if let Err(e) = common {
            evidence.pps = false;
            evidence.power = false;
            evidence.pps_error = Some(e.clone());
            evidence.power_error = Some(e);
        }
        Self {
            shell_slots: shell_slots(image.as_deref(), note.as_deref()),
            shell_image_sha256: image,
            shell_loaded_note_sha256: note,
            pps_verified: evidence.pps,
            power_verified: evidence.power,
            pps_probe_error: evidence.pps_error,
            power_probe_error: evidence.power_error,
            pps_driver_sha256: pps_image,
            // Kept for old diagnostic readers; whole-driver digest no longer gates features.
            pps_driver_contract_sha256: None,
            pps_kernel_release: kernel,
            pps_loaded_note_sha256: loaded_hash,
            kernel_abi_checked_symbols: abi_checked,
            evidence_stamp: evidence_stamp(h),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cos16_and_cos17_require_their_own_matching_loaded_note() {
        for (image, note) in VERIFIED_SHELL_DRIVERS {
            assert_eq!(shell_slots(Some(image), Some(note)), Some(3));
            assert_eq!(shell_slots(Some(image), None), None);
            assert_eq!(shell_slots(None, Some(note)), None);
        }
        assert_eq!(shell_slots(Some(SHELL_IMAGE), Some(SHELL_NOTE_COS17)), None);
        assert_eq!(shell_slots(Some(SHELL_IMAGE_COS17), Some(SHELL_NOTE)), None);
        assert_eq!(shell_slots(Some("unknown"), Some("unknown")), None);
    }
}
