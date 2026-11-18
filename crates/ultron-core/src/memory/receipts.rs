//! Makbuz ve kaynak görüntüsü okuma/doğrulama (Aşama 5a, Task 2).
//!
//! Yazım `vault.rs` içindeki commit hattındadır (maskele, temp+sync, değiştirmesiz taşıma);
//! burada yalnız değişmez dosyaların okunması ve zincir doğrulaması vardır.
//!
//! Makbuz dosyası `receipts/<sha256>.json`: sürümlü, kanonik JSON
//! `{"version":1,"source":{...},"excerpt":"...","captured_at":"...","claim_status":"..."}`;
//! dosya adındaki hash dosya baytlarının SHA-256'sıdır.

use std::collections::HashMap;

use super::contracts::{FrontMatter, Receipt, Sha256Hash, SourceRef};
use super::vault::{Vault, VaultError, VaultResult};

const MAX_RECEIPT_FILE: u64 = 16 * 1024;
const MAX_SNAPSHOT_FILE: u64 = 130 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReceiptIssue {
    /// Makbuz dosyası yok.
    Missing,
    /// Dosya var ama hash/kanonik biçim/şema uyuşmuyor ya da okunamıyor.
    Tampered,
    /// Makbuz `File` kaynağına işaret ediyor ve kaynak görüntüsü yok.
    SourceMissing,
    /// Kaynak görüntüsünün hash'i makbuzdakiyle uyuşmuyor.
    SourceTampered,
}

impl Vault {
    /// Makbuzu okur; baytlar kanonik değilse ya da hash uyuşmuyorsa `Tampered`.
    pub fn read_receipt(&self, hash: &Sha256Hash) -> VaultResult<Receipt> {
        let (bytes, _) = self.read_checked(&Receipt::relative_path(hash), MAX_RECEIPT_FILE)?;
        Receipt::from_canonical_json(&bytes, hash).map_err(|_| VaultError::Tampered)
    }

    /// Kaynak görüntüsünü okur ve hash'ini doğrular (gerçekten okunabilir olmalı).
    pub fn read_source_snapshot(&self, locator: &str, expected: &Sha256Hash) -> VaultResult<String> {
        if !locator.starts_with("receipts/sources/") {
            return Err(VaultError::PathEscape);
        }
        let (bytes, _) = self.read_checked(locator, MAX_SNAPSHOT_FILE)?;
        if &Sha256Hash::of(&bytes) != expected {
            return Err(VaultError::Tampered);
        }
        String::from_utf8(bytes).map_err(|_| VaultError::NotUtf8)
    }

    /// Notun makbuz zincirini denetler; sorun yoksa boş döner. İçerik taşımaz.
    pub fn verify_receipts(&self, front: &FrontMatter) -> Vec<(Sha256Hash, ReceiptIssue)> {
        self.verify_receipts_cached(front, &mut HashMap::new())
    }

    /// `verify_receipts`; tarama boyunca aynı makbuzun yeniden okunmaması için önbellekli.
    pub fn verify_receipts_cached(
        &self,
        front: &FrontMatter,
        cache: &mut HashMap<Sha256Hash, Option<ReceiptIssue>>,
    ) -> Vec<(Sha256Hash, ReceiptIssue)> {
        let mut issues = Vec::new();
        for hash in &front.receipts {
            let result = cache.entry(hash.clone()).or_insert_with(|| self.check_one(hash)).to_owned();
            if let Some(issue) = result {
                issues.push((hash.clone(), issue));
            }
        }
        issues
    }

    fn check_one(&self, hash: &Sha256Hash) -> Option<ReceiptIssue> {
        match self.read_receipt(hash) {
            Ok(receipt) => {
                if let SourceRef::File { locator, content_hash } = &receipt.source {
                    return match self.read_source_snapshot(locator, content_hash) {
                        Ok(_) => None,
                        Err(VaultError::NotFound) => Some(ReceiptIssue::SourceMissing),
                        Err(_) => Some(ReceiptIssue::SourceTampered),
                    };
                }
                None
            }
            Err(VaultError::NotFound) => Some(ReceiptIssue::Missing),
            Err(_) => Some(ReceiptIssue::Tampered),
        }
    }
}
