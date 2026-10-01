//! Encrypted hot-wallet keystore: Argon2id key derivation +
//! XChaCha20-Poly1305. The secret key never touches disk unencrypted and is
//! held only in process memory while the engine runs.

use argon2::{Algorithm, Argon2, Params, Version};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use chain::solana_sdk::signature::{Keypair, Signer};
use rand::RngCore;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct KeystoreFile {
    pub version: u32,
    pub pubkey: String,
    pub label: String,
    pub kdf: String,
    pub m_cost_kib: u32,
    pub t_cost: u32,
    pub p_cost: u32,
    pub salt: String,
    pub nonce: String,
    pub ciphertext: String,
}

fn derive(pass: &str, salt: &[u8], m: u32, t: u32, p: u32) -> anyhow::Result<[u8; 32]> {
    let params =
        Params::new(m, t, p, Some(32)).map_err(|e| anyhow::anyhow!("argon2 params: {e}"))?;
    let mut key = [0u8; 32];
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password_into(pass.as_bytes(), salt, &mut key)
        .map_err(|e| anyhow::anyhow!("argon2: {e}"))?;
    Ok(key)
}

pub fn encrypt(kp: &Keypair, pass: &str, label: &str) -> anyhow::Result<KeystoreFile> {
    anyhow::ensure!(
        pass.len() >= 12,
        "passphrase must be at least 12 characters"
    );
    let (m, t, p) = (64 * 1024, 3, 1);
    let mut salt = [0u8; 16];
    let mut nonce = [0u8; 24];
    rand::rngs::OsRng.fill_bytes(&mut salt);
    rand::rngs::OsRng.fill_bytes(&mut nonce);
    let key = derive(pass, &salt, m, t, p)?;
    let ct = XChaCha20Poly1305::new((&key).into())
        .encrypt(&XNonce::from(nonce), kp.to_bytes().as_ref())
        .map_err(|_| anyhow::anyhow!("encryption failed"))?;
    Ok(KeystoreFile {
        version: 1,
        pubkey: kp.pubkey().to_string(),
        label: label.into(),
        kdf: "argon2id".into(),
        m_cost_kib: m,
        t_cost: t,
        p_cost: p,
        salt: B64.encode(salt),
        nonce: B64.encode(nonce),
        ciphertext: B64.encode(ct),
    })
}

pub fn decrypt(f: &KeystoreFile, pass: &str) -> anyhow::Result<Keypair> {
    let key = derive(
        pass,
        &B64.decode(&f.salt)?,
        f.m_cost_kib,
        f.t_cost,
        f.p_cost,
    )?;
    let pt = XChaCha20Poly1305::new((&key).into())
        .decrypt(
            &XNonce::from(<[u8; 24]>::try_from(B64.decode(&f.nonce)?.as_slice())?),
            B64.decode(&f.ciphertext)?.as_ref(),
        )
        .map_err(|_| anyhow::anyhow!("wrong passphrase or corrupted keystore"))?;
    let kp = Keypair::try_from(pt.as_slice()).map_err(|e| anyhow::anyhow!("bad key bytes: {e}"))?;
    anyhow::ensure!(
        kp.pubkey().to_string() == f.pubkey,
        "keystore pubkey mismatch"
    );
    Ok(kp)
}

pub fn read(path: &str) -> anyhow::Result<KeystoreFile> {
    Ok(serde_json::from_str(
        &std::fs::read_to_string(path).map_err(|e| anyhow::anyhow!("{path}: {e}"))?,
    )?)
}

pub fn write(path: &str, f: &KeystoreFile) -> anyhow::Result<()> {
    if let Some(dir) = std::path::Path::new(path).parent() {
        std::fs::create_dir_all(dir)?;
    }
    anyhow::ensure!(
        !std::path::Path::new(path).exists(),
        "{path} already exists — refusing to overwrite a key"
    );
    std::fs::write(path, serde_json::to_string_pretty(f)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

pub fn passphrase(env_name: &str, confirm: bool) -> anyhow::Result<String> {
    if let Ok(p) = std::env::var(env_name) {
        if !p.is_empty() {
            return Ok(p);
        }
    }
    let p = rpassword::prompt_password("Keystore passphrase: ")?;
    if confirm {
        let q = rpassword::prompt_password("Repeat passphrase: ")?;
        anyhow::ensure!(p == q, "passphrases do not match");
    }
    Ok(p)
}

pub fn load(path: &str, pass_env: &str) -> anyhow::Result<Keypair> {
    decrypt(&read(path)?, &passphrase(pass_env, false)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_wrong_pass() {
        let kp = Keypair::new();
        let f = encrypt(&kp, "correct horse battery", "t").unwrap();
        assert_eq!(
            decrypt(&f, "correct horse battery").unwrap().pubkey(),
            kp.pubkey()
        );
        assert!(decrypt(&f, "wrong passphrase!!").is_err());
        assert!(encrypt(&kp, "short", "t").is_err());
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("k.json");
        let ps = p.to_str().unwrap();
        write(ps, &f).unwrap();
        assert!(write(ps, &f).is_err(), "must not overwrite");
        assert_eq!(
            decrypt(&read(ps).unwrap(), "correct horse battery")
                .unwrap()
                .pubkey(),
            kp.pubkey()
        );
    }
}
