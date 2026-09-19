//! Segment encryption keys, per D61.
//!
//! D28 decided the shape and left the design; D61 is the design. One key for
//! each project, wrapped by an installation root key, stored in the catalog.
//!
//! # What this gives, stated plainly
//!
//! Destroying a project key erases that whole project instantly, and an
//! object-store reader without the key reads nothing useful.
//!
//! It does **not** physically destroy one end user's cold bytes. An end-user
//! erasure stays what D28 says it is: immediate and logical in every tier, hot
//! and warm rewritten within the 24-hour target, and cold bytes reclaimed when
//! retention expires them. D28 weighed per-end-user keys and refused them,
//! because that costs a key for every person and a key lookup for every person
//! on a cold scan.
//!
//! # Generations
//!
//! A project key has generations. Each encrypted segment names the generation it
//! used, so rotation writes a new generation and leaves already-written objects
//! readable until retention expires them or compaction rewrites them.
//! Destroying a project destroys every generation.
//!
//! Rotation without generations would mean re-encrypting every cold object on
//! the spot, which is the cost D28 refused for per-end-user keys.
//!
//! # The nonce
//!
//! A fresh random 96-bit nonce for each encrypted block, stored beside the block
//! it belongs to. A nonce derived from the segment identity would be twelve
//! bytes cheaper and is safe right up until something rewrites a segment under
//! one key — and compaction rewrites segments. A repeated nonce in a counter
//! mode is a total loss of confidentiality rather than a degradation, so twelve
//! bytes is not a trade worth making. See L024.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};

/// The algorithm number, which travels in the footer. A later segment can use
/// another one without a major version change.
pub const ALGORITHM_AES_256_GCM: u16 = 1;

/// A 96-bit nonce, which is what AES-GCM takes.
pub const NONCE_BYTES: usize = 12;
/// The authentication tag AES-GCM appends.
pub const TAG_BYTES: usize = 16;
/// What encryption adds to one block.
pub const OVERHEAD_BYTES: usize = NONCE_BYTES + TAG_BYTES;

/// Why a key operation failed.
///
/// Every message is written for the person who has to act on it. None of them
/// says whether a key existed, because that is the fact erasure removes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyError {
    /// The installation root key is missing or the wrong shape.
    RootKey(String),
    /// The key this data needs is gone. This is the ordinary result of an
    /// erasure and is not a fault.
    Destroyed(String),
    /// The bytes did not authenticate.
    Damaged(String),
}

impl std::fmt::Display for KeyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KeyError::RootKey(m) | KeyError::Destroyed(m) | KeyError::Damaged(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for KeyError {}

/// Random bytes from the system source.
///
/// A failure here is not something a storage engine can work around, and
/// carrying on with a weak key would be worse than stopping.
fn random(into: &mut [u8]) -> Result<(), KeyError> {
    getrandom::fill(into).map_err(|e| {
        KeyError::RootKey(format!(
            "The system could not produce the random numbers TallyOwl needs to protect stored data: {e}"
        ))
    })
}

/// The installation root key. It wraps every project key and never encrypts a
/// segment itself.
///
/// It arrives as a secret reference in configuration, which CONVENTIONS.md
/// section 5 already requires: the file holds `file:`, `env:`, or a secret-store
/// reference, and the loader resolves it at startup and never logs the value.
#[derive(Clone)]
pub struct RootKey {
    bytes: [u8; 32],
}

impl std::fmt::Debug for RootKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // A key never reaches a log, an error, or a panic message.
        f.write_str("RootKey(hidden)")
    }
}

impl RootKey {
    /// A root key from 32 raw bytes.
    pub fn from_bytes(bytes: [u8; 32]) -> RootKey {
        RootKey { bytes }
    }

    /// A root key from its configured text form, which is 64 hexadecimal
    /// characters.
    pub fn from_text(text: &str) -> Result<RootKey, KeyError> {
        let text = text.trim();
        let bytes = crate::row::from_hex(text).ok_or_else(|| {
            KeyError::RootKey(
                "The installation key is not in the expected form. It is 64 hexadecimal \
                 characters, such as the value `tallyowl keys new-root` prints."
                    .to_string(),
            )
        })?;
        let bytes: [u8; 32] = bytes.try_into().map_err(|_| {
            KeyError::RootKey(
                "The installation key is the wrong length. It is 64 hexadecimal characters, \
                 which is 32 bytes."
                    .to_string(),
            )
        })?;
        Ok(RootKey { bytes })
    }

    /// A fresh root key, for an operator to store and back up.
    ///
    /// DEPLOYMENT.md section 5a states the matching requirement: this key is
    /// theirs to protect. A data directory restored without it holds unreadable
    /// cold segments, which is the same property that makes erasure work.
    pub fn generate() -> Result<RootKey, KeyError> {
        let mut bytes = [0u8; 32];
        random(&mut bytes)?;
        Ok(RootKey { bytes })
    }

    /// The text form an operator stores. This is the one place a key becomes
    /// text, and the caller is responsible for where it goes.
    pub fn to_text(&self) -> String {
        crate::row::hex(&self.bytes)
    }

    fn cipher(&self) -> Aes256Gcm {
        Aes256Gcm::new_from_slice(&self.bytes).expect("a 32-byte key")
    }

    /// Wrap a project key for storage in the catalog.
    ///
    /// The wrapped form carries the project and the generation as additional
    /// authenticated data, so a wrapped key cannot be moved to another project
    /// or another generation and still open.
    pub fn wrap(
        &self,
        project_id: [u8; 16],
        generation: u32,
        key: &ProjectKey,
    ) -> Result<Vec<u8>, KeyError> {
        seal(
            &self.cipher(),
            &key.bytes,
            &wrap_context(project_id, generation),
        )
    }

    /// Unwrap a project key.
    pub fn unwrap_key(
        &self,
        project_id: [u8; 16],
        generation: u32,
        wrapped: &[u8],
    ) -> Result<ProjectKey, KeyError> {
        let plain = open(
            &self.cipher(),
            wrapped,
            &wrap_context(project_id, generation),
            "the installation key",
        )?;
        let bytes: [u8; 32] = plain
            .try_into()
            .map_err(|_| KeyError::Damaged("A stored key is the wrong length.".to_string()))?;
        Ok(ProjectKey { bytes })
    }
}

fn wrap_context(project_id: [u8; 16], generation: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(20);
    out.extend_from_slice(&project_id);
    out.extend_from_slice(&generation.to_le_bytes());
    out
}

/// One project's key, at one generation.
#[derive(Clone)]
pub struct ProjectKey {
    bytes: [u8; 32],
}

impl std::fmt::Debug for ProjectKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ProjectKey(hidden)")
    }
}

impl ProjectKey {
    pub fn generate() -> Result<ProjectKey, KeyError> {
        let mut bytes = [0u8; 32];
        random(&mut bytes)?;
        Ok(ProjectKey { bytes })
    }

    fn cipher(&self) -> Aes256Gcm {
        Aes256Gcm::new_from_slice(&self.bytes).expect("a 32-byte key")
    }
}

/// What a segment writer and reader use to protect one block at a time.
#[derive(Clone, Debug)]
pub struct Cipher {
    key: ProjectKey,
    /// Which generation of the project key this is, so a reader can find it.
    pub generation: u32,
}

impl Cipher {
    pub fn new(key: ProjectKey, generation: u32) -> Cipher {
        Cipher { key, generation }
    }

    /// Protect one block. The result is the nonce, the ciphertext, and the tag.
    ///
    /// `context` is authenticated and not encrypted. A block therefore cannot be
    /// moved to a different place in the segment and still open, which is what
    /// stops an attacker with the file from reordering pages.
    pub fn seal(&self, plain: &[u8], context: &[u8]) -> Result<Vec<u8>, KeyError> {
        seal(&self.key.cipher(), plain, context)
    }

    /// Open one block.
    pub fn open(&self, sealed: &[u8], context: &[u8]) -> Result<Vec<u8>, KeyError> {
        open(&self.key.cipher(), sealed, context, "this project's key")
    }
}

fn seal(cipher: &Aes256Gcm, plain: &[u8], context: &[u8]) -> Result<Vec<u8>, KeyError> {
    let mut nonce_bytes = [0u8; NONCE_BYTES];
    random(&mut nonce_bytes)?;
    let nonce = Nonce::from_slice(&nonce_bytes);

    let sealed = cipher
        .encrypt(
            nonce,
            Payload {
                msg: plain,
                aad: context,
            },
        )
        .map_err(|_| {
            KeyError::Damaged(
                "Stored data could not be protected before it was written.".to_string(),
            )
        })?;

    let mut out = Vec::with_capacity(NONCE_BYTES + sealed.len());
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&sealed);
    Ok(out)
}

fn open(
    cipher: &Aes256Gcm,
    sealed: &[u8],
    context: &[u8],
    which: &str,
) -> Result<Vec<u8>, KeyError> {
    if sealed.len() < OVERHEAD_BYTES {
        return Err(KeyError::Damaged(
            "A protected part of the stored data is too short to be one.".to_string(),
        ));
    }
    let nonce = Nonce::from_slice(&sealed[..NONCE_BYTES]);
    cipher
        .decrypt(
            nonce,
            Payload {
                msg: &sealed[NONCE_BYTES..],
                aad: context,
            },
        )
        .map_err(|_| {
            // A wrong key and damaged bytes are indistinguishable here, and
            // saying which would tell a reader whether a key exists. That fact
            // is what an erasure removes.
            KeyError::Damaged(format!(
                "This stored data could not be read with {which}. It was written with a \
                 different key, or it has been damaged, or the key has been destroyed."
            ))
        })
}

/// The context a page or an index block is authenticated under.
///
/// The segment identity and the offset are both in it, so a block cannot be
/// moved between segments or between positions and still open.
pub fn block_context(segment_id: [u8; 16], offset: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(24);
    out.extend_from_slice(&segment_id);
    out.extend_from_slice(&offset.to_le_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> RootKey {
        RootKey::generate().unwrap()
    }

    #[test]
    fn a_block_round_trips_under_its_own_context() {
        let cipher = Cipher::new(ProjectKey::generate().unwrap(), 1);
        let context = block_context([1; 16], 4096);
        let sealed = cipher.seal(b"a page of column data", &context).unwrap();
        assert_eq!(
            cipher.open(&sealed, &context).unwrap(),
            b"a page of column data"
        );
    }

    #[test]
    fn encryption_costs_a_fixed_amount_for_each_block() {
        let cipher = Cipher::new(ProjectKey::generate().unwrap(), 1);
        let sealed = cipher
            .seal(&[0u8; 1_000], &block_context([1; 16], 0))
            .unwrap();
        assert_eq!(sealed.len(), 1_000 + OVERHEAD_BYTES);
    }

    #[test]
    fn a_block_cannot_be_moved_to_another_position_and_still_open() {
        // The offset is authenticated, so an attacker with the file cannot
        // reorder pages and have them decode.
        let cipher = Cipher::new(ProjectKey::generate().unwrap(), 1);
        let sealed = cipher.seal(b"page", &block_context([1; 16], 4096)).unwrap();
        assert!(cipher.open(&sealed, &block_context([1; 16], 8192)).is_err());
    }

    #[test]
    fn a_block_cannot_be_moved_to_another_segment_and_still_open() {
        let cipher = Cipher::new(ProjectKey::generate().unwrap(), 1);
        let sealed = cipher.seal(b"page", &block_context([1; 16], 0)).unwrap();
        assert!(cipher.open(&sealed, &block_context([2; 16], 0)).is_err());
    }

    #[test]
    fn one_key_cannot_read_another_projects_block() {
        let first = Cipher::new(ProjectKey::generate().unwrap(), 1);
        let second = Cipher::new(ProjectKey::generate().unwrap(), 1);
        let context = block_context([1; 16], 0);
        let sealed = first.seal(b"page", &context).unwrap();
        assert!(second.open(&sealed, &context).is_err());
    }

    #[test]
    fn a_damaged_block_does_not_authenticate() {
        let cipher = Cipher::new(ProjectKey::generate().unwrap(), 1);
        let context = block_context([1; 16], 0);
        let mut sealed = cipher.seal(b"a page of column data", &context).unwrap();
        let last = sealed.len() - 1;
        sealed[last] ^= 0xff;
        assert!(matches!(
            cipher.open(&sealed, &context),
            Err(KeyError::Damaged(_))
        ));
    }

    #[test]
    fn two_blocks_of_one_value_do_not_produce_one_ciphertext() {
        // The property a random nonce buys. A deterministic nonce would make
        // two identical pages identical on disk, which leaks that they are the
        // same, and would repeat a nonce when compaction rewrites a segment.
        let cipher = Cipher::new(ProjectKey::generate().unwrap(), 1);
        let context = block_context([1; 16], 0);
        let first = cipher.seal(b"page", &context).unwrap();
        let second = cipher.seal(b"page", &context).unwrap();
        assert_ne!(first, second);
        // And both still open.
        assert_eq!(cipher.open(&first, &context).unwrap(), b"page");
        assert_eq!(cipher.open(&second, &context).unwrap(), b"page");
    }

    #[test]
    fn a_project_key_round_trips_through_its_wrapped_form() {
        let root = root();
        let key = ProjectKey::generate().unwrap();
        let wrapped = root.wrap([9; 16], 1, &key).unwrap();
        let back = root.unwrap_key([9; 16], 1, &wrapped).unwrap();

        // The unwrapped key opens what the original sealed.
        let context = block_context([1; 16], 0);
        let sealed = Cipher::new(key, 1).seal(b"page", &context).unwrap();
        assert_eq!(
            Cipher::new(back, 1).open(&sealed, &context).unwrap(),
            b"page"
        );
    }

    #[test]
    fn a_wrapped_key_cannot_be_moved_to_another_project() {
        let root = root();
        let wrapped = root
            .wrap([9; 16], 1, &ProjectKey::generate().unwrap())
            .unwrap();
        assert!(root.unwrap_key([1; 16], 1, &wrapped).is_err());
    }

    #[test]
    fn a_wrapped_key_cannot_be_moved_to_another_generation() {
        // Rotation writes a new generation, and an old wrapped key must not
        // open under the new number.
        let root = root();
        let wrapped = root
            .wrap([9; 16], 1, &ProjectKey::generate().unwrap())
            .unwrap();
        assert!(root.unwrap_key([9; 16], 2, &wrapped).is_err());
    }

    #[test]
    fn the_wrong_root_key_opens_nothing() {
        let wrapped = root()
            .wrap([9; 16], 1, &ProjectKey::generate().unwrap())
            .unwrap();
        assert!(root().unwrap_key([9; 16], 1, &wrapped).is_err());
    }

    #[test]
    fn a_root_key_round_trips_through_its_text_form() {
        let key = RootKey::generate().unwrap();
        let text = key.to_text();
        assert_eq!(text.len(), 64);
        let back = RootKey::from_text(&text).unwrap();

        let project = ProjectKey::generate().unwrap();
        let wrapped = key.wrap([9; 16], 1, &project).unwrap();
        assert!(back.unwrap_key([9; 16], 1, &wrapped).is_ok());
    }

    #[test]
    fn a_root_key_of_the_wrong_shape_is_refused_with_an_instruction() {
        for bad in ["", "not hexadecimal", "abcd", &"ab".repeat(31)] {
            let failure = RootKey::from_text(bad).unwrap_err();
            assert!(matches!(failure, KeyError::RootKey(_)), "{bad}");
            assert!(
                failure.to_string().contains("64 hexadecimal"),
                "the message does not say what a valid one looks like: {failure}"
            );
        }
    }

    #[test]
    fn a_key_never_appears_in_a_debug_rendering() {
        // A key that reached a log or a panic message would undo the whole
        // point. CONVENTIONS.md section 4 forbids it and this is the enforcement.
        let root = RootKey::generate().unwrap();
        let rendered = format!("{root:?}");
        assert_eq!(rendered, "RootKey(hidden)");
        assert!(!rendered.contains(&root.to_text()[..8]));

        let project = ProjectKey::generate().unwrap();
        assert_eq!(format!("{project:?}"), "ProjectKey(hidden)");
        let cipher = Cipher::new(project, 3);
        let rendered = format!("{cipher:?}");
        assert!(rendered.contains("hidden"), "{rendered}");
        assert!(rendered.contains('3'), "the generation is not a secret");
    }

    #[test]
    fn a_failure_does_not_say_whether_a_key_ever_existed() {
        // Saying "no such key" and "wrong key" differently would tell a reader
        // whether a project was erased, which is the fact erasure removes.
        let cipher = Cipher::new(ProjectKey::generate().unwrap(), 1);
        let context = block_context([1; 16], 0);
        let sealed = Cipher::new(ProjectKey::generate().unwrap(), 1)
            .seal(b"page", &context)
            .unwrap();
        let failure = cipher.open(&sealed, &context).unwrap_err().to_string();
        assert!(failure.contains("different key"));
        assert!(failure.contains("destroyed"));
    }

    #[test]
    fn two_generated_keys_differ() {
        assert_ne!(
            RootKey::generate().unwrap().to_text(),
            RootKey::generate().unwrap().to_text()
        );
    }
}
