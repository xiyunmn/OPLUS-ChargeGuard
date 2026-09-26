//! Device evidence is tied to loaded code, never inferred from a successful write.
use crate::hardware::Hardware;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{fs::File, io::Read};

pub const SHELL_IMAGE: &str = "eb5e1a6bb5281f72faff6cfee0334471fc9ce79cec37538b44bf4998a40b0c0f";
pub const SHELL_NOTE: &str = "bd782ec5169ca35f14a073b56a68a7d42a84d09ec44c9175bca2ec3ca4acbd2e";

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
    (image == Some(SHELL_IMAGE) && loaded_note == Some(SHELL_NOTE)).then_some(3)
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
