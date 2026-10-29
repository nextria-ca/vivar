//! Bundle authentication with a shared key: `pack --sign` writes an HMAC of
//! the checksum list, `unpack` checks it when asked (or when policy requires).
//! The key is 32 random bytes, hex, from `VIVAR_BUNDLE_KEY` or
//! `<config dir>/vivar/bundle.key`.

use crate::{dirs, sha256};
use anyhow::{anyhow, bail, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

pub const SIGNATURE: &str = "vivar-bundle.sig";

pub fn key_path() -> Result<PathBuf> {
    Ok(dirs::config()?.join("bundle.key"))
}

/// The configured key, if any.
pub fn key() -> Result<Option<Vec<u8>>> {
    let hex = match std::env::var("VIVAR_BUNDLE_KEY") {
        Ok(k) => k,
        Err(_) => match fs::read_to_string(key_path()?) {
            Ok(k) => k,
            Err(_) => return Ok(None),
        },
    };
    let hex = hex.trim();
    if hex.len() != 64 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!(
            "the bundle key must be 64 hex characters (32 bytes); `vivar key generate` makes one"
        );
    }
    Ok(Some(
        (0..32)
            .map(|i| u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).unwrap_or(0))
            .collect(),
    ))
}

pub fn require_key() -> Result<Vec<u8>> {
    key()?.ok_or_else(|| {
        anyhow!(
            "no bundle key: set VIVAR_BUNDLE_KEY or run `vivar key generate` (then copy {} to the other machines)",
            key_path().map(|p| p.display().to_string()).unwrap_or_default()
        )
    })
}

/// Writes a fresh random key unless one exists (`force` replaces it).
pub fn generate(force: bool) -> Result<PathBuf> {
    let path = key_path()?;
    if path.exists() && !force {
        bail!("{} already exists; pass --force to replace it (bundles signed with the old key will stop verifying)", path.display());
    }
    fs::create_dir_all(path.parent().context("key path has no parent")?)?;
    // Replace, never truncate in place: a key file created elsewhere with wide
    // permissions would otherwise keep them.
    let _ = fs::remove_file(&path);
    let bytes = random_bytes()?;
    // Private from the first byte: created with 0600, never widened.
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&path)?;
    use std::io::Write;
    file.write_all((sha256::hex(&bytes) + "\n").as_bytes())?;
    Ok(path)
}

fn random_bytes() -> Result<[u8; 32]> {
    let mut out = [0u8; 32];
    #[cfg(unix)]
    {
        use std::io::Read;
        fs::File::open("/dev/urandom")
            .context("opening /dev/urandom")?
            .read_exact(&mut out)?;
    }
    #[cfg(windows)]
    {
        #[link(name = "advapi32")]
        extern "system" {
            #[link_name = "SystemFunction036"]
            fn rtl_gen_random(buffer: *mut u8, length: u32) -> u8;
        }
        // SAFETY: the buffer is valid for `length` bytes for the duration of the call.
        let ok = unsafe { rtl_gen_random(out.as_mut_ptr(), out.len() as u32) };
        if ok == 0 {
            bail!("the system random number generator failed");
        }
    }
    Ok(out)
}

/// Signs a bundle: the HMAC covers the checksum list and the inventory of
/// everything the list leaves out (launchers, activate scripts, symlinks, the
/// manifest), so nothing executable can be swapped under a valid signature.
pub fn sign(checksums: &[u8], inventory: &[u8], key: &[u8]) -> String {
    let mut message = Vec::with_capacity(checksums.len() + inventory.len() + 1);
    message.extend_from_slice(checksums);
    message.push(0);
    message.extend_from_slice(inventory);
    sha256::hex(&sha256::hmac(key, &message))
}

/// Verifies a bundle directory's signature; any problem is an error.
pub fn verify(dir: &Path, key: &[u8]) -> Result<()> {
    let sig = fs::read_to_string(dir.join(SIGNATURE)).context("reading the bundle signature")?;
    let checksums = fs::read(dir.join(crate::bundle::CHECKSUMS))?;
    let inventory = fs::read(dir.join(crate::bundle::INVENTORY))
        .context("this signed bundle has no inventory; it was not made by this vivar")?;
    let sig = sig.trim();
    if sig.len() != 64 || sign(&checksums, &inventory, key) != sig {
        bail!("the bundle's signature does not match the bundle key: it was made with another key or altered");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_parsed_and_signatures_round_trip() {
        std::env::set_var("VIVAR_BUNDLE_KEY", "ab".repeat(32));
        let k = key().unwrap().unwrap();
        assert_eq!(k, vec![0xab; 32]);
        let sig = sign(b"checksums", b"inventory", &k);
        assert_eq!(sig.len(), 64);
        assert_ne!(sig, sign(b"checksums", b"inventory", &[0xcd; 32]));
        assert_ne!(sig, sign(b"checksums", b"other", &k));
        std::env::set_var("VIVAR_BUNDLE_KEY", "nothex");
        assert!(key().is_err());
        std::env::remove_var("VIVAR_BUNDLE_KEY");
        assert!(random_bytes().unwrap().iter().any(|b| *b != 0));
    }
}
