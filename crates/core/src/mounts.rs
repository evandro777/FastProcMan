//! Mounted file systems on real devices, from `findmnt` (no `libc` for `statvfs`).
use std::process::Command;

#[derive(Debug, Clone, PartialEq)]
pub struct Mount {
    pub device: String,
    pub target: String,
    pub fstype: String,
    pub total: u64,
    pub used: u64,
    pub avail: u64,
}

impl Mount {
    /// Percent used as `df` counts it: of what an ordinary user can reach, so reserved blocks are left out.
    pub fn used_pct(&self) -> u8 {
        let reach = self.used.saturating_add(self.avail).max(1);
        (self.used as f64 * 100.0 / reach as f64).round() as u8
    }
}

/// `None` when `findmnt` failed or hung (the caller keeps its last list). `timeout`: a dead network share would
/// otherwise freeze the sampler thread for good.
pub fn list() -> Option<Vec<Mount>> {
    let out = Command::new("timeout")
        .args(["5", "findmnt", "--real", "-b", "-P", "-o"])
        .arg("SOURCE,TARGET,FSTYPE,SIZE,USED,AVAIL")
        .env("LC_ALL", "C")
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| parse(&String::from_utf8_lossy(&out.stdout)))
}

/// `KEY="value"`; findmnt writes unsafe bytes (quotes, and non-ASCII under `LC_ALL=C`) as `\xNN`.
fn field(line: &str, key: &str) -> String {
    let start = format!("{key}=\"");
    let raw = line
        .split_once(&start)
        .and_then(|(_, rest)| rest.split_once('"'))
        .map_or("", |(v, _)| v);
    let mut parts = raw.split("\\x");
    let mut bytes = parts.next().unwrap_or("").as_bytes().to_vec();
    for part in parts {
        match part.get(..2).and_then(|h| u8::from_str_radix(h, 16).ok()) {
            Some(b) => {
                bytes.push(b);
                bytes.extend_from_slice(&part.as_bytes()[2..]);
            }
            None => bytes.extend_from_slice(format!("\\x{part}").as_bytes()),
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

fn parse(text: &str) -> Vec<Mount> {
    text.lines()
        .filter_map(|line| {
            let num = |key| field(line, key).parse::<u64>().ok();
            let mut device = field(line, "SOURCE");
            // btrfs and bind mounts append the sub-folder: `/dev/sda2[/@home]`.
            if device.ends_with(']')
                && let Some(i) = device.find('[')
            {
                device.truncate(i);
            }
            let m = Mount {
                device,
                target: field(line, "TARGET"),
                fstype: field(line, "FSTYPE"),
                total: num("SIZE").filter(|&n| n > 0)?,
                used: num("USED")?,
                avail: num("AVAIL")?,
            };
            // Snap images and AppImages are read-only program bundles, always "100% full": noise, not storage.
            (m.fstype != "squashfs" && !m.target.starts_with("/tmp/.mount_")).then_some(m)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_findmnt_pairs_and_drops_noise() {
        let text = r#"SOURCE="/dev/nvme0n1p2[/@home]" TARGET="/home" FSTYPE="btrfs" SIZE="1000" USED="600" AVAIL="200"
SOURCE="//nas/share" TARGET="/mnt/My Files\x22\xc3\xa9" FSTYPE="cifs" SIZE="100" USED="0" AVAIL="100"
SOURCE="portal" TARGET="/run/user/1000/doc" FSTYPE="fuse.portal" SIZE="" USED="" AVAIL=""
SOURCE="/dev/loop1" TARGET="/snap/core/1" FSTYPE="squashfs" SIZE="10" USED="10" AVAIL="0"
SOURCE="x.appimage" TARGET="/tmp/.mount_xAbC" FSTYPE="fuse.x.appimage" SIZE="10" USED="10" AVAIL="0"
"#;
        let list = parse(text);
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].device, "/dev/nvme0n1p2");
        assert_eq!((list[0].total, list[0].used_pct()), (1000, 75));
        assert_eq!(list[1].target, "/mnt/My Files\"é");
        assert_eq!(list[1].used_pct(), 0);
    }
}
