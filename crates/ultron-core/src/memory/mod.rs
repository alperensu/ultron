//! Aşama 5a ikinci beyin: hafıza sözleşmeleri.
//!
//! Bu modül yalnız tipleri, doğrulamayı ve yetki matrisini tanımlar. Kasa yazımı, indeks,
//! getirme ve özet üretimi sonraki görevlerin (kasa, SQLite depoları, getirme, daemon servisi)
//! sahipliğindedir; burada davranış yoktur.

pub mod background;
pub mod contracts;
pub mod receipts;
pub mod reconcile;
pub mod search;
pub mod vault;
pub mod suggestions;

pub use contracts::*;
