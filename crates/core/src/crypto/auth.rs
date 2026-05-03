use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use rand::RngCore;
use sha2::Sha256;
use std::path::PathBuf;

type HmacSha256 = Hmac<Sha256>;

/// Salt for HKDF key derivation — must match the web client.
const AUTH_HKDF_SALT: &[u8] = b"ShellAnyWhere-auth-key-derivation";
/// Info string for HKDF expand — must match the web client.
const AUTH_HKDF_INFO: &[u8] = b"HMAC-SHA256-auth";
/// Number of HMAC-SHA256 stretch iterations after HKDF extract.
/// Each guess requires this many HMAC operations, raising brute-force cost.
/// 1000 iterations adds ~5-20ms latency at startup (negligible) but forces
/// an attacker to pay 1000× per guess compared to raw HMAC.
const AUTH_STRETCH_ITERATIONS: usize = 1_000;

/// A derived authentication key — holds the stretched output, NOT the plaintext token.
///
/// Created once at startup from the raw token string. The plaintext token is
/// consumed and never stored. All HMAC operations use this 32-byte derived key.
///
/// Derivation uses HKDF-SHA256 extract + expand followed by iterative HMAC
/// stretching (1000 rounds). This means even a weak token like "a" produces
/// a cryptographically strong 256-bit key, and brute-force attacks must pay
/// the full stretch cost per guess (~1000 × HMAC-SHA256 per attempt).
#[derive(Clone)]
pub struct AuthKey {
    derived: [u8; 32],
}

impl AuthKey {
    /// Derive a fixed 32-byte key from a plaintext token.
    /// The plaintext token is consumed and not retained.
    ///
    /// Process:
    /// 1. HKDF-SHA256 extract(salt, token) → PRK
    /// 2. HKDF-SHA256 expand(PRK, info) → 32-byte seed
    /// 3. Iterative HMAC-SHA256 stretch: 1000 rounds of HMAC(seed, prev)
    ///    to raise brute-force cost per guess.
    pub fn derive(token: &str) -> Self {
        // Step 1+2: HKDF extract + expand to get a 32-byte seed
        let hkdf = Hkdf::<Sha256>::new(Some(AUTH_HKDF_SALT), token.as_bytes());
        let mut seed = [0u8; 32];
        hkdf.expand(AUTH_HKDF_INFO, &mut seed)
            .expect("32 bytes is valid output length for HKDF-SHA256");

        // Step 3: Iterative HMAC-SHA256 stretching
        // Each iteration: prev = HMAC-SHA256(seed, prev)
        // This is similar to PBKDF2's core loop but simpler.
        let mut current = seed;
        for _ in 0..AUTH_STRETCH_ITERATIONS {
            let mut mac = HmacSha256::new_from_slice(&seed).expect("HMAC can take key of any size");
            mac.update(&current);
            current = mac.finalize().into_bytes().into();
        }

        Self { derived: current }
    }

    /// Constant-time comparison of two derived keys.
    pub fn ct_eq(&self, other: &AuthKey) -> bool {
        subtle::ConstantTimeEq::ct_eq(&self.derived[..], &other.derived[..]).unwrap_u8() == 1
    }

    /// Compute HMAC-SHA256(derived_key, nonce) for challenge-response auth.
    pub fn compute_hmac(&self, nonce: &[u8]) -> Vec<u8> {
        let mut mac =
            HmacSha256::new_from_slice(&self.derived).expect("HMAC can take key of any size");
        mac.update(nonce);
        mac.finalize().into_bytes().to_vec()
    }

    /// Verify an HMAC response against this key using constant-time comparison.
    pub fn verify_hmac(&self, nonce: &[u8], response: &[u8]) -> bool {
        let mut mac =
            HmacSha256::new_from_slice(&self.derived).expect("HMAC can take key of any size");
        mac.update(nonce);
        mac.verify_slice(response).is_ok()
    }

    /// Derive a deterministic Ed25519 SSH public key from this auth key.
    ///
    /// The 32-byte `derived` value is used directly as an Ed25519 signing key seed.
    /// The resulting public key is returned as an OpenSSH format string
    /// (e.g. "ssh-ed25519 AAAA... ShellAnyWhere").
    /// This is deterministic: the same token always produces the same public key.
    pub fn derive_ssh_public_key(&self) -> String {
        use ed25519_dalek::SigningKey;
        let signing_key = SigningKey::from_bytes(&self.derived);
        let verifying_key = signing_key.verifying_key();
        let pub_bytes = verifying_key.to_bytes();

        // OpenSSH wire format: string "ssh-ed25519" + 32-byte public key
        let mut wire = Vec::with_capacity(4 + 11 + 4 + 32);
        wire.extend_from_slice(&(11u32).to_be_bytes());
        wire.extend_from_slice(b"ssh-ed25519");
        wire.extend_from_slice(&(32u32).to_be_bytes());
        wire.extend_from_slice(&pub_bytes);

        format!("ssh-ed25519 {} ShellAnyWhere", base64_encode(&wire))
    }

    /// Derive a deterministic OpenSSH format private key from this auth key.
    ///
    /// The 32-byte `derived` value is used as an Ed25519 signing key seed.
    /// Returns the complete OpenSSH private key as a PEM-like string
    /// that can be written directly to a file and used with `ssh -i`.
    pub fn derive_openssh_private_key(&self) -> String {
        use base64::Engine;
        use ed25519_dalek::SigningKey;

        let signing_key = SigningKey::from_bytes(&self.derived);
        let verifying_key = signing_key.verifying_key();
        let pub_bytes = verifying_key.to_bytes();
        let keypair_bytes: [u8; 64] = {
            let mut kb = [0u8; 64];
            kb[..32].copy_from_slice(&self.derived);
            kb[32..].copy_from_slice(&pub_bytes);
            kb
        };

        let comment = "ShellAnyWhere";

        // Public key wire format
        let mut pub_wire = Vec::with_capacity(4 + 11 + 4 + 32);
        let alg = b"ssh-ed25519";
        pub_wire.extend_from_slice(&(alg.len() as u32).to_be_bytes());
        pub_wire.extend_from_slice(alg);
        pub_wire.extend_from_slice(&(32u32).to_be_bytes());
        pub_wire.extend_from_slice(&pub_bytes);

        // Private key wire format
        let mut priv_wire = Vec::new();
        let checkint: u32 = 0x12345678;
        priv_wire.extend_from_slice(&checkint.to_be_bytes());
        priv_wire.extend_from_slice(&checkint.to_be_bytes());
        priv_wire.extend_from_slice(&(alg.len() as u32).to_be_bytes());
        priv_wire.extend_from_slice(alg);
        priv_wire.extend_from_slice(&(32u32).to_be_bytes());
        priv_wire.extend_from_slice(&pub_bytes);
        priv_wire.extend_from_slice(&(64u32).to_be_bytes());
        priv_wire.extend_from_slice(&keypair_bytes);
        priv_wire.extend_from_slice(&(comment.len() as u32).to_be_bytes());
        priv_wire.extend_from_slice(comment.as_bytes());
        let pad_len = (8 - (priv_wire.len() % 8)) % 8;
        for i in 1..=pad_len {
            priv_wire.push(i as u8);
        }

        // Assemble full blob
        let magic = b"openssh-key-v1\0";
        let cipher = b"none";
        let kdfname = b"none";

        let mut blob = Vec::new();
        blob.extend_from_slice(magic);
        blob.extend_from_slice(&(cipher.len() as u32).to_be_bytes());
        blob.extend_from_slice(cipher);
        blob.extend_from_slice(&(kdfname.len() as u32).to_be_bytes());
        blob.extend_from_slice(kdfname);
        blob.extend_from_slice(&0u32.to_be_bytes());
        blob.extend_from_slice(&1u32.to_be_bytes());
        blob.extend_from_slice(&(pub_wire.len() as u32).to_be_bytes());
        blob.extend_from_slice(&pub_wire);
        blob.extend_from_slice(&(priv_wire.len() as u32).to_be_bytes());
        blob.extend_from_slice(&priv_wire);

        let b64 = base64::engine::general_purpose::STANDARD.encode(&blob);

        let mut pem = String::from("-----BEGIN OPENSSH PRIVATE KEY-----\n");
        for chunk in b64.as_bytes().chunks(70) {
            pem.push_str(&String::from_utf8_lossy(chunk));
            pem.push('\n');
        }
        pem.push_str("-----END OPENSSH PRIVATE KEY-----\n");
        pem
    }
}

/// Get the default token file path (config dir/token).
pub fn default_token_file_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config")
        .join("ShellAnyWhere")
        .join("token")
}

/// Thread-local override for token file path (set from config).
use std::sync::OnceLock;
static TOKEN_FILE_PATH: OnceLock<PathBuf> = OnceLock::new();

/// Set the token file path (called once at startup from config).
pub fn set_token_file_path(path: PathBuf) {
    let _ = TOKEN_FILE_PATH.set(path);
}

/// Get the token file path (config override or default).
pub fn token_file_path() -> PathBuf {
    TOKEN_FILE_PATH
        .get()
        .cloned()
        .unwrap_or_else(default_token_file_path)
}

/// Generate a random 256-bit hex token
pub fn generate_token() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    hex::encode(&bytes)
}

/// Generate a random 32-byte nonce for challenge-response auth.
pub fn generate_nonce() -> Vec<u8> {
    let mut nonce = vec![0u8; 32];
    rand::rng().fill_bytes(&mut nonce);
    nonce
}

/// Compute HMAC-SHA256(derived_key, nonce) for challenge-response auth.
pub fn compute_auth_response(key: &AuthKey, nonce: &[u8]) -> Vec<u8> {
    key.compute_hmac(nonce)
}

/// Verify a challenge-response against the derived key.
/// Uses constant-time comparison to prevent timing attacks.
pub fn verify_auth_response(key: &AuthKey, nonce: &[u8], response: &[u8]) -> bool {
    key.verify_hmac(nonce, response)
}

/// Save token to file
pub fn save_token(token: &str) -> anyhow::Result<()> {
    let path = token_file_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, token)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(&path, perms)?;
    }
    #[cfg(not(unix))]
    {
        log::debug!("Token file created at {:?} (NTFS ACLs apply)", path);
    }
    Ok(())
}

/// Load token from file
pub fn load_token() -> anyhow::Result<String> {
    let path = token_file_path();
    let token = std::fs::read_to_string(&path)?.trim().to_string();
    Ok(token)
}

/// Get or create a token:
/// 1. Use provided token if given
/// 2. Load from file if exists
/// 3. Generate new token and save to file
pub fn get_or_create_token(provided: Option<String>) -> anyhow::Result<String> {
    if let Some(token) = provided {
        return Ok(token);
    }

    match load_token() {
        Ok(token) if !token.is_empty() => {
            log::info!("Loaded token from {:?}", token_file_path());
            Ok(token)
        }
        _ => {
            let token = generate_token();
            save_token(&token)?;
            log::info!("Generated new token, saved to {:?}", token_file_path());
            Ok(token)
        }
    }
}

// ── Server-side mutual auth helpers ────────────────────────────────────────

/// Handle server-side mutual authentication step 1:
/// Given the client's nonce, generate server nonce and proof.
///
/// Returns `(server_nonce, server_proof)` where:
/// - `server_nonce` is a fresh 32-byte random nonce
/// - `server_proof = HMAC-SHA256(derived_key, client_nonce)`
///
/// Send these as `AuthChallenge { nonce, proof }` to the client.
pub fn server_auth_challenge(key: &AuthKey, client_nonce: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let server_nonce = generate_nonce();
    let proof = compute_auth_response(key, client_nonce);
    (server_nonce, proof)
}

/// Handle server-side mutual authentication step 2:
/// Verify the client's response against the server nonce.
///
/// Returns `true` if `response == HMAC-SHA256(derived_key, server_nonce)`.
pub fn server_auth_verify(key: &AuthKey, server_nonce: &[u8], response: &[u8]) -> bool {
    verify_auth_response(key, server_nonce, response)
}

mod hex {
    pub fn encode(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{:02x}", b)).collect()
    }
}

mod base64_encode {
    use base64::Engine;
    pub fn encode(data: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(data)
    }
}

fn base64_encode(data: &[u8]) -> String {
    base64_encode::encode(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_token() {
        let token1 = generate_token();
        let token2 = generate_token();
        assert_eq!(token1.len(), 64);
        assert_ne!(token1, token2);
    }

    #[test]
    fn test_derive_key_deterministic() {
        let key1 = AuthKey::derive("same-token");
        let key2 = AuthKey::derive("same-token");
        assert_eq!(key1.derived, key2.derived);
    }

    #[test]
    fn test_derive_key_different_tokens() {
        let key1 = AuthKey::derive("token-a");
        let key2 = AuthKey::derive("token-b");
        assert_ne!(key1.derived, key2.derived);
    }

    #[test]
    fn test_challenge_response_roundtrip() {
        let key = AuthKey::derive("test-token-abc123");
        let nonce = generate_nonce();
        let response = compute_auth_response(&key, &nonce);
        assert!(verify_auth_response(&key, &nonce, &response));
    }

    #[test]
    fn test_challenge_response_wrong_key() {
        let nonce = generate_nonce();
        let key_correct = AuthKey::derive("correct-token");
        let key_wrong = AuthKey::derive("wrong-token");
        let response = compute_auth_response(&key_correct, &nonce);
        assert!(!verify_auth_response(&key_wrong, &nonce, &response));
    }

    #[test]
    fn test_challenge_response_wrong_nonce() {
        let key = AuthKey::derive("test-token");
        let nonce1 = generate_nonce();
        let nonce2 = generate_nonce();
        let response = compute_auth_response(&key, &nonce1);
        assert!(!verify_auth_response(&key, &nonce2, &response));
    }

    #[test]
    fn test_challenge_response_no_replay() {
        let key = AuthKey::derive("test-token");
        let nonce1 = generate_nonce();
        let nonce2 = generate_nonce();
        let response1 = compute_auth_response(&key, &nonce1);
        let response2 = compute_auth_response(&key, &nonce2);
        assert_ne!(response1, response2);
        assert!(!verify_auth_response(&key, &nonce2, &response1));
    }

    #[test]
    fn test_nonce_is_random() {
        let nonce1 = generate_nonce();
        let nonce2 = generate_nonce();
        assert_eq!(nonce1.len(), 32);
        assert_ne!(nonce1, nonce2);
    }

    #[test]
    fn test_server_auth_challenge_and_verify() {
        let key = AuthKey::derive("shared-secret-token");
        let client_nonce = generate_nonce();

        let (server_nonce, server_proof) = server_auth_challenge(&key, &client_nonce);

        assert!(verify_auth_response(&key, &client_nonce, &server_proof));
        let key_wrong = AuthKey::derive("wrong-token");
        assert!(!verify_auth_response(
            &key_wrong,
            &client_nonce,
            &server_proof
        ));

        let client_response = compute_auth_response(&key, &server_nonce);

        assert!(server_auth_verify(&key, &server_nonce, &client_response));
        assert!(!server_auth_verify(
            &key_wrong,
            &server_nonce,
            &client_response
        ));
    }

    #[test]
    fn test_mutual_auth_detects_mitm() {
        let client_key = AuthKey::derive("real-token");
        let mitm_key = AuthKey::derive("fake-token");
        let client_nonce = generate_nonce();

        let (_server_nonce, server_proof) = server_auth_challenge(&mitm_key, &client_nonce);

        assert!(!verify_auth_response(
            &client_key,
            &client_nonce,
            &server_proof
        ));
    }
}
