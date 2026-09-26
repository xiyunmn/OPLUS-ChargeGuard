//! Device evidence is tied to loaded code, never inferred from a successful write.
use crate::hardware::Hardware;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{fs::File, io::Read};

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
        Self {
            shell_slots: shell_slots(image.as_deref(), note.as_deref()),
            shell_image_sha256: image,
            shell_loaded_note_sha256: note,
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
