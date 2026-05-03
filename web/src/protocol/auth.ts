/// HMAC-SHA256 authentication — matches Rust's crypto::auth module.

const AUTH_HKDF_SALT = new TextEncoder().encode(
  "ShellAnyWhere-auth-key-derivation",
);
const AUTH_HKDF_INFO = new TextEncoder().encode("HMAC-SHA256-auth");
const AUTH_STRETCH_ITERATIONS = 1_000;

/// Derive a 32-byte key from a plaintext token using HKDF-SHA256 + iterative stretching.
/// This matches Rust's AuthKey::derive() — same salt, info, and iteration count.
///
/// Process:
/// 1. HKDF-SHA256 extract+expand → 32-byte seed
/// 2. 1000 rounds of HMAC-SHA256(seed, prev) stretching
///
/// Even a weak token like "a" produces a strong 256-bit key, and brute-force
/// attacks must pay the full stretch cost per guess.
export async function deriveAuthKey(token: string): Promise<CryptoKey> {
  // Step 1: HKDF extract + expand to get 32-byte seed
  const ikm = await crypto.subtle.importKey(
    "raw",
    new TextEncoder().encode(token),
    "HKDF",
    false,
    ["deriveBits"],
  );
  const seedBits = await crypto.subtle.deriveBits(
    {
      name: "HKDF",
      hash: "SHA-256",
      salt: AUTH_HKDF_SALT,
      info: AUTH_HKDF_INFO,
    },
    ikm,
    256,
  );
  const seed = new Uint8Array(seedBits);

  // Step 2: Iterative HMAC-SHA256 stretching (1000 rounds)
  // Each iteration: current = HMAC-SHA256(seed, current)
  const hmacKey = await crypto.subtle.importKey(
    "raw",
    seed,
    { name: "HMAC", hash: "SHA-256" },
    false,
    ["sign"],
  );
  let current = seed;
  for (let i = 0; i < AUTH_STRETCH_ITERATIONS; i++) {
    const sig = await crypto.subtle.sign(
      "HMAC",
      hmacKey,
      current as BufferSource,
    );
    current = new Uint8Array(sig);
  }

  // Import the final stretched key as an HMAC key for auth operations
  return crypto.subtle.importKey(
    "raw",
    current,
    { name: "HMAC", hash: "SHA-256" },
    false,
    ["sign"],
  );
}

/// Compute HMAC-SHA256(derived_key, nonce) for challenge-response auth.
/// The key should be obtained from deriveAuthKey().
export async function computeAuthResponse(
  key: CryptoKey,
  nonce: Uint8Array,
): Promise<Uint8Array> {
  const sig = await crypto.subtle.sign("HMAC", key, nonce as BufferSource);
  return new Uint8Array(sig);
}
