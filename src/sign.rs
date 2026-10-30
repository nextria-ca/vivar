//! Bundle authentication with Ed25519: `pack --sign` signs the checksum list
//! and inventory with the signing key, `unpack` checks the signature against
//! the trusted public keys when asked (or when policy requires).
//!
//! The signing key is 32 random bytes, hex, from `VIVAR_BUNDLE_KEY` or
//! `<config dir>/vivar/bundle.key`; its public key sits beside it in
//! `bundle.pub`. Verifiers trust: `VIVAR_TRUSTED_KEYS`, policy `trusted-keys`,
//! their own `bundle.pub`, and the public key of their own signing key.

use crate::{dirs, ed25519, sha256};
use anyhow::{anyhow, bail, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

pub const SIGNATURE: &str = "vivar-bundle.sig";
const SCHEME: &str = "ed25519";

pub fn key_path() -> Result<PathBuf> {
    Ok(dirs::config()?.join("bundle.key"))
}

pub fn public_key_path() -> Result<PathBuf> {
    Ok(dirs::config()?.join("bundle.pub"))
}

/// The configured signing key, if any.
pub fn key() -> Result<Option<[u8; 32]>> {
    let hex = match std::env::var("VIVAR_BUNDLE_KEY") {
        Ok(k) => k,
        Err(_) => match fs::read_to_string(key_path()?) {
            Ok(k) => k,
            Err(_) => return Ok(None),
        },
    };
    match ed25519::unhex::<32>(&hex) {
        Some(k) => Ok(Some(k)),
        None => bail!(
            "the bundle key must be 64 hex characters (32 bytes); `vivar key generate` makes one"
        ),
    }
}

pub fn require_key() -> Result<[u8; 32]> {
    key()?.ok_or_else(|| {
        anyhow!(
            "no bundle key: set VIVAR_BUNDLE_KEY or run `vivar key generate` (then share {} with the machines that unpack)",
            public_key_path().map(|p| p.display().to_string()).unwrap_or_default()
        )
    })
}

/// The public key of the configured signing key, if any.
pub fn public_key() -> Result<Option<[u8; 32]>> {
    Ok(key()?.map(|k| ed25519::keypair(k).public))
}

/// Writes a fresh random signing key and its public key unless one exists
/// (`force` replaces both).
pub fn generate(force: bool) -> Result<(PathBuf, PathBuf)> {
    let path = key_path()?;
    if path.exists() && !force {
        bail!("{} already exists; pass --force to replace it (bundles signed with the old key will stop verifying)", path.display());
    }
    fs::create_dir_all(path.parent().context("key path has no parent")?)?;
    // New bytes first: a failing random source must not cost the existing key.
    let bytes = random_bytes()?;
    let pair = ed25519::keypair(bytes);
    // Replace, never truncate in place: a key file created elsewhere with wide
    // permissions would otherwise keep them.
    let _ = fs::remove_file(&path);
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
    file.write_all((sha256::hex(&pair.secret) + "\n").as_bytes())?;
    let public = public_key_path()?;
    fs::write(&public, sha256::hex(&pair.public) + "\n")?;
    Ok((path, public))
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

/// What gets signed: the checksum list and the inventory of everything the
/// list leaves out (launchers, activate scripts, symlinks, the manifest), so
/// nothing executable can be swapped under a valid signature.
fn message(checksums: &[u8], inventory: &[u8]) -> Vec<u8> {
    let mut message = Vec::with_capacity(checksums.len() + inventory.len() + 1);
    message.extend_from_slice(checksums);
    message.push(0);
    message.extend_from_slice(inventory);
    message
}

/// The signature file's line: `ed25519 <public key> <signature>`, all hex.
pub fn sign(checksums: &[u8], inventory: &[u8], key: &[u8; 32]) -> String {
    let pair = ed25519::keypair(*key);
    let sig = ed25519::sign(key, &message(checksums, inventory));
    format!(
        "{SCHEME} {} {}\n",
        sha256::hex(&pair.public),
        sha256::hex(&sig)
    )
}

/// What policy says about bundle signers: its keys, and whether they are
/// the only ones.
#[derive(Debug, Clone, Default)]
pub struct Trust {
    pub keys: Vec<String>,
    pub locked: bool,
}

/// Public keys this machine trusts, from every configured source. Keys from
/// policy and the environment are validated; a bad one is an error, not a
/// silently smaller trust set. Under a `locked` policy the policy's keys are
/// the whole set: the environment and the machine's own key don't count.
pub fn trusted_keys(policy_keys: &[String], locked: bool) -> Result<Vec<[u8; 32]>> {
    let mut keys = Vec::new();
    let mut push = |hex: &str, source: &str| -> Result<()> {
        let k = ed25519::unhex::<32>(hex)
            .ok_or_else(|| anyhow!("{source}: {hex:?} is not a 64-hex-character public key"))?;
        if !keys.contains(&k) {
            keys.push(k);
        }
        Ok(())
    };
    for k in policy_keys {
        push(k, "policy trusted-keys")?;
    }
    if locked {
        return Ok(keys);
    }
    if let Ok(list) = std::env::var("VIVAR_TRUSTED_KEYS") {
        for k in list
            .split([',', ' ', '\n'])
            .filter(|k| !k.trim().is_empty())
        {
            push(k, "VIVAR_TRUSTED_KEYS")?;
        }
    }
    if let Ok(own) = fs::read_to_string(public_key_path()?) {
        push(&own, "bundle.pub")?;
    }
    if let Some(public) = public_key()? {
        if !keys.contains(&public) {
            keys.push(public);
        }
    }
    Ok(keys)
}

/// Verifies a bundle directory's signature against the trusted keys; any
/// problem is an error.
pub fn verify(dir: &Path, trusted: &[[u8; 32]]) -> Result<()> {
    let sig = fs::read_to_string(dir.join(SIGNATURE)).context("reading the bundle signature")?;
    let checksums = fs::read(dir.join(crate::bundle::CHECKSUMS))?;
    let inventory = fs::read(dir.join(crate::bundle::INVENTORY))
        .context("this signed bundle has no inventory; it was not made by this vivar")?;
    let fields: Vec<&str> = sig.split_whitespace().collect();
    let (public, signature) = match fields.as_slice() {
        [scheme, public, signature] if *scheme == SCHEME => (
            ed25519::unhex::<32>(public),
            ed25519::unhex::<64>(signature),
        ),
        [legacy] if legacy.len() == 64 => {
            bail!("this bundle carries a shared-key (HMAC) signature from an older vivar; re-pack it with this version")
        }
        _ => bail!("the bundle's signature file is malformed"),
    };
    let (Some(public), Some(signature)) = (public, signature) else {
        bail!("the bundle's signature file is malformed");
    };
    if trusted.is_empty() {
        bail!(
            "no trusted keys: add `trusted-keys` to policy (the only source under a locked policy), set VIVAR_TRUSTED_KEYS, or copy the signer's bundle.pub to {}",
            public_key_path().map(|p| p.display().to_string()).unwrap_or_default()
        );
    }
    if !trusted.contains(&public) {
        bail!(
            "the bundle's signature does not match any trusted key: it was signed with key {}, which this machine does not trust",
            sha256::hex(&public)
        );
    }
    if !ed25519::verify(&public, &message(&checksums, &inventory), &signature) {
        bail!("the bundle's signature does not match its contents: it was altered after signing");
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
        assert_eq!(k, [0xab; 32]);
        let line = sign(b"checksums", b"inventory", &k);
        let fields: Vec<&str> = line.split_whitespace().collect();
        assert_eq!(fields[0], SCHEME);
        assert_eq!(fields[1], sha256::hex(&public_key().unwrap().unwrap()));
        assert_eq!(fields[2].len(), 128);
        assert_ne!(line, sign(b"checksums", b"inventory", &[0xcd; 32]));
        assert_ne!(line, sign(b"checksums", b"other", &k));

        let dir = std::env::temp_dir().join(format!("vivar-sign-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(crate::bundle::CHECKSUMS), b"checksums").unwrap();
        fs::write(dir.join(crate::bundle::INVENTORY), b"inventory").unwrap();
        fs::write(dir.join(SIGNATURE), &line).unwrap();
        let public = public_key().unwrap().unwrap();
        verify(&dir, &[public]).unwrap();
        let err = verify(&dir, &[[0xcd; 32]]).unwrap_err().to_string();
        assert!(err.contains("does not trust"), "{err}");
        let err = verify(&dir, &[]).unwrap_err().to_string();
        assert!(err.contains("no trusted keys"), "{err}");
        fs::write(dir.join(crate::bundle::INVENTORY), b"tampered").unwrap();
        let err = verify(&dir, &[public]).unwrap_err().to_string();
        assert!(err.contains("altered"), "{err}");
        fs::write(dir.join(SIGNATURE), "ab".repeat(32)).unwrap();
        let err = verify(&dir, &[public]).unwrap_err().to_string();
        assert!(err.contains("older vivar"), "{err}");
        let _ = fs::remove_dir_all(&dir);

        std::env::set_var("VIVAR_TRUSTED_KEYS", format!("{},nothex", "cd".repeat(32)));
        assert!(trusted_keys(&[], false).is_err());
        std::env::set_var(
            "VIVAR_TRUSTED_KEYS",
            format!("{} {}", "cd".repeat(32), "ef".repeat(32)),
        );
        let t = trusted_keys(&["cd".repeat(32)], false).unwrap();
        assert!(t.contains(&[0xcd; 32]) && t.contains(&[0xef; 32]) && t.contains(&public));
        // A locked policy trusts its own keys and nothing from the
        // environment or this machine's key.
        let t = trusted_keys(&["cd".repeat(32)], true).unwrap();
        assert_eq!(t, vec![[0xcd; 32]]);
        assert!(trusted_keys(&[], true).unwrap().is_empty());
        std::env::remove_var("VIVAR_TRUSTED_KEYS");

        std::env::set_var("VIVAR_BUNDLE_KEY", "nothex");
        assert!(key().is_err());
        std::env::remove_var("VIVAR_BUNDLE_KEY");
        assert!(random_bytes().unwrap().iter().any(|b| *b != 0));
    }
}
