//! ChaCha20-Poly1305 around a compressed sample.
//!
//! The key lives in [`crate::UplinkConfig`] for the life of the process. It is
//! not written to disk and nothing in this crate logs it.

use chacha20poly1305::aead::{Aead, Generate, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};

use crate::sample::{MediaSample, Stage};

pub fn random_key() -> [u8; 32] {
    let key: Key = Key::generate();
    let mut out = [0u8; 32];
    out.copy_from_slice(key.as_slice());
    out
}

pub fn seal(key: &[u8; 32], plaintext: &[u8]) -> Result<Vec<u8>, String> {
    let cipher = ChaCha20Poly1305::new((&key[..]).try_into().expect("key is 32 bytes"));
    let nonce = Nonce::generate();
    let ciphertext = cipher
        .encrypt(&nonce, plaintext)
        .map_err(|_| "encrypt failed".to_owned())?;
    let mut out = Vec::with_capacity(12 + ciphertext.len());
    out.extend_from_slice(nonce.as_slice());
    out.extend_from_slice(&ciphertext);
    Ok(out)
}

pub fn open(key: &[u8; 32], blob: &[u8]) -> Result<Vec<u8>, String> {
    if blob.len() < 12 + 16 {
        return Err("encrypted sample is too short".into());
    }
    let cipher = ChaCha20Poly1305::new((&key[..]).try_into().expect("key is 32 bytes"));
    let nonce = Nonce::try_from(&blob[..12]).map_err(|_| "nonce length".to_owned())?;
    cipher
        .decrypt(&nonce, &blob[12..])
        .map_err(|_| "decrypt failed".to_owned())
}

pub fn encrypt_sample(key: &[u8; 32], sample: &MediaSample) -> Result<MediaSample, String> {
    if sample.stage != Stage::Compressed {
        return Err("encryptor expects a compressed sample".into());
    }
    let mut out = sample.clone();
    out.bytes = seal(key, &sample.bytes)?;
    out.stage = Stage::Encrypted;
    Ok(out)
}

pub fn decrypt_sample(key: &[u8; 32], sample: &MediaSample) -> Result<MediaSample, String> {
    if sample.stage != Stage::Encrypted {
        return Err("decryptor expects an encrypted sample".into());
    }
    let mut out = sample.clone();
    out.bytes = open(key, &sample.bytes)?;
    out.stage = Stage::Compressed;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sample::SourceKind;

    #[test]
    fn seal_roundtrip() {
        let key = [9u8; 32];
        let blob = seal(&key, b"frame").unwrap();
        assert_ne!(&blob[12..], b"frame");
        assert_eq!(open(&key, &blob).unwrap(), b"frame");
        let mut wrong = key;
        wrong[0] ^= 1;
        assert!(open(&wrong, &blob).is_err());
    }

    #[test]
    fn sample_stage_moves_to_encrypted() {
        let key = [3u8; 32];
        let mut sample = MediaSample::video(SourceKind::Webcam, 1, 1, 1, b"ADPC".to_vec());
        sample.stage = Stage::Compressed;
        let sealed = encrypt_sample(&key, &sample).unwrap();
        assert_eq!(sealed.stage, Stage::Encrypted);
        let back = decrypt_sample(&key, &sealed).unwrap();
        assert_eq!(back.bytes, sample.bytes);
        assert_eq!(back.stage, Stage::Compressed);
    }
}
