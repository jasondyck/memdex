//! `memdex update`: self-update from GitHub releases, respecting how memdex was installed.
//!
//! - plain binary (install.sh / install.ps1 / release archive): download this platform's
//!   archive + `.sha256`, verify, extract, and swap the binary in place (atomic rename on
//!   Unix; on Windows the running exe is renamed aside and removed on the next run);
//! - Windows setup.exe install (uninstaller next to the exe): run the new installer silently;
//! - package-managed (`/usr/...`, e.g. the .deb): leave it to the package manager.
//!
//! Uses the system `curl` and `tar` (shipped with Linux and Windows 10+), so memdex keeps a
//! single dependency. SHA-256 is computed here because Windows has no `sha256sum`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const DEFAULT_REPO: &str = "jasondyck/memdex";

pub fn run(current: &str, check_only: bool, pinned: Option<String>) -> Result<(), String> {
    let repo = std::env::var("MEMDEX_REPO").unwrap_or_else(|_| DEFAULT_REPO.into());
    // Overriding the releases URL (mirrors, tests) also allows plain http.
    let custom = std::env::var("MEMDEX_RELEASES_URL").ok();
    let base = custom
        .clone()
        .unwrap_or_else(|| format!("https://github.com/{repo}/releases"));
    let proto = if custom.is_some() { "=https,http" } else { "=https" };

    let is_pinned = pinned.is_some();
    let tag = match pinned {
        Some(v) => normalize_tag(&v),
        None => latest_tag(&base, proto)?,
    };
    let latest = tag.trim_start_matches('v');
    let newer = version_lt(current, latest);
    if check_only {
        if newer {
            println!("memdex {latest} is available (installed: {current}). Run `memdex update`.");
        } else {
            println!("memdex {current} is up to date (latest release: {latest}).");
        }
        return Ok(());
    }
    // Without --version, only ever move forward (a local dev build may be newer).
    if (!is_pinned && !newer) || latest == current {
        println!("memdex {current} is up to date (latest release: {latest}).");
        return Ok(());
    }

    let exe = std::env::current_exe()
        .and_then(|p| p.canonicalize())
        .map_err(|e| format!("can't locate the running binary: {e}"))?;
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        a => return Err(format!("no prebuilt binary for {a}; build from source")),
    };
    let tmp = std::env::temp_dir().join(format!("memdex-update-{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp).map_err(|e| e.to_string())?;
    let result = install(&base, proto, &tag, arch, &exe, &tmp);
    let _ = fs::remove_dir_all(&tmp);
    result?;
    println!("Updated memdex {current} → {latest}.");
    Ok(())
}

fn install(base: &str, proto: &str, tag: &str, arch: &str, exe: &Path, tmp: &Path) -> Result<(), String> {
    let ver = tag.trim_start_matches('v');
    if cfg!(unix) && exe.starts_with("/usr") {
        let deb = if arch == "x86_64" { "amd64" } else { "arm64" };
        return Err(format!(
            "{} is managed by your package manager. Update it with the new package:\n  \
             curl -fLO {base}/download/{tag}/memdex_{ver}_{deb}.deb && sudo apt install ./memdex_{ver}_{deb}.deb",
            exe.display()
        ));
    }
    if cfg!(windows) && exe.parent().is_some_and(|d| d.join("unins000.exe").exists()) {
        let a = if arch == "x86_64" { "x64" } else { "arm64" };
        let setup = fetch_verified(base, proto, tag, &format!("memdex-{ver}-{a}-setup.exe"), tmp)?;
        println!("Running the installer for {tag}…");
        let ok = Command::new(&setup)
            .args(["/VERYSILENT", "/SUPPRESSMSGBOXES", "/NORESTART"])
            .status()
            .is_ok_and(|s| s.success());
        return if ok { Ok(()) } else { Err("the installer failed".into()) };
    }

    let target = if cfg!(windows) {
        format!("{arch}-pc-windows-msvc")
    } else if cfg!(target_os = "linux") {
        format!("{arch}-unknown-linux-musl")
    } else {
        return Err("no prebuilt binary for this OS; build from source".into());
    };
    let ext = if cfg!(windows) { "zip" } else { "tar.gz" };
    let archive = fetch_verified(base, proto, tag, &format!("memdex-{target}.{ext}"), tmp)?;
    // bsdtar on Windows reads zip; GNU/bsd tar on Linux reads tar.gz.
    let ok = Command::new("tar")
        .arg("-xf")
        .arg(&archive)
        .arg("-C")
        .arg(tmp)
        .status()
        .is_ok_and(|s| s.success());
    if !ok {
        return Err("couldn't extract the release archive (is `tar` installed?)".into());
    }
    let name = if cfg!(windows) { "memdex.exe" } else { "memdex" };
    let new_bin = tmp.join(format!("memdex-{target}")).join(name);
    let out = Command::new(&new_bin)
        .arg("--version")
        .output()
        .map_err(|e| e.to_string())?;
    let reported = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if reported != format!("memdex {ver}") {
        return Err(format!("downloaded binary reports `{reported}`, expected memdex {ver}"));
    }
    replace_binary(&new_bin, exe)
}

/// Download `asset` and its `.sha256` from the release, verify, return the asset path.
fn fetch_verified(base: &str, proto: &str, tag: &str, asset: &str, tmp: &Path) -> Result<PathBuf, String> {
    let url = format!("{base}/download/{tag}/{asset}");
    let file = tmp.join(asset);
    let sum = tmp.join(format!("{asset}.sha256"));
    println!("Downloading {asset} ({tag})…");
    curl(proto, &url, &file)?;
    curl(proto, &format!("{url}.sha256"), &sum)?;
    let want = fs::read_to_string(&sum)
        .map_err(|e| e.to_string())?
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    let got = sha256_hex(&fs::read(&file).map_err(|e| e.to_string())?);
    if want.len() != 64 || got != want {
        return Err(format!("checksum mismatch for {asset}; not installing"));
    }
    Ok(file)
}

fn curl(proto: &str, url: &str, out: &Path) -> Result<(), String> {
    let status = Command::new("curl")
        .args(["-fsSL", "--proto", proto, "-o"])
        .arg(out)
        .arg(url)
        .status()
        .map_err(|e| format!("couldn't run curl ({e}); install curl or update manually"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("download failed: {url}"))
    }
}

/// Latest release tag, from the redirect of `<base>/latest` (no API, no rate limit).
fn latest_tag(base: &str, proto: &str) -> Result<String, String> {
    let out = Command::new("curl")
        .args(["-fsSIL", "--proto", proto, "-o"])
        .arg(if cfg!(windows) { "NUL" } else { "/dev/null" })
        .args(["-w", "%{url_effective}"])
        .arg(format!("{base}/latest"))
        .output()
        .map_err(|e| format!("couldn't run curl ({e}); install curl or update manually"))?;
    let url = String::from_utf8_lossy(&out.stdout).trim().to_string();
    url.rsplit_once("/tag/")
        .map(|(_, t)| normalize_tag(t))
        .filter(|t| t.len() > 1)
        .ok_or_else(|| format!("couldn't determine the latest release from {base}/latest"))
}

fn normalize_tag(v: &str) -> String {
    let v = v.trim();
    if v.starts_with('v') {
        v.to_string()
    } else {
        format!("v{v}")
    }
}

/// Is `a` older than `b`? Numeric compare of dotted versions.
fn version_lt(a: &str, b: &str) -> bool {
    let parse = |s: &str| -> Vec<u64> { s.split('.').map(|p| p.parse().unwrap_or(0)).collect() };
    parse(a) < parse(b)
}

#[cfg(unix)]
fn replace_binary(new_bin: &Path, exe: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    // Stage next to the target (same filesystem), then rename over it atomically.
    // Running processes keep the old inode, so live MCP sessions are unaffected.
    let staged = exe.with_extension("new");
    fs::copy(new_bin, &staged).map_err(|e| format!("can't write {}: {e}", staged.display()))?;
    fs::set_permissions(&staged, fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
    fs::rename(&staged, exe).map_err(|e| format!("can't replace {}: {e}", exe.display()))
}

#[cfg(windows)]
fn replace_binary(new_bin: &Path, exe: &Path) -> Result<(), String> {
    // A running exe can't be overwritten but can be renamed; the old copy is
    // removed by `cleanup_old_binary` on the next start.
    let old = exe.with_extension("exe.old");
    let _ = fs::remove_file(&old);
    fs::rename(exe, &old).map_err(|e| format!("can't move the running binary aside: {e}"))?;
    if let Err(e) = fs::copy(new_bin, exe) {
        let _ = fs::rename(&old, exe);
        return Err(format!("can't write {}: {e}", exe.display()));
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn replace_binary(_: &Path, _: &Path) -> Result<(), String> {
    Err("self-update isn't supported on this OS".into())
}

/// Windows: remove the previous binary left behind by an update.
pub fn cleanup_old_binary() {
    if cfg!(windows)
        && let Ok(exe) = std::env::current_exe()
    {
        let _ = fs::remove_file(exe.with_extension("exe.old"));
    }
}

/// SHA-256 (FIPS 180-4), small and dependency-free.
pub fn sha256_hex(data: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5, 0xd807aa98,
        0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786,
        0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8,
        0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13,
        0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819,
        0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a,
        0x5b9cca4f, 0x682e6ff3, 0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
    ];
    let mut msg = data.to_vec();
    let bits = (data.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bits.to_be_bytes());
    for block in msg.as_chunks::<64>().0 {
        let mut w = [0u32; 64];
        for (i, word) in block.as_chunks::<4>().0.iter().enumerate() {
            w[i] = u32::from_be_bytes(*word);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (x, v) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *x = x.wrapping_add(v);
        }
    }
    h.iter().map(|x| format!("{x:08x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_known_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        assert_eq!(
            sha256_hex(&vec![b'a'; 1_000_000]),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }

    #[test]
    fn versions_and_tags() {
        assert!(version_lt("0.5.2", "0.6.0"));
        assert!(version_lt("0.9.9", "0.10.0"));
        assert!(!version_lt("0.6.0", "0.6.0"));
        assert!(!version_lt("1.0.0", "0.9.0"));
        assert_eq!(normalize_tag("0.6.0"), "v0.6.0");
        assert_eq!(normalize_tag("v0.6.0"), "v0.6.0");
    }
}
