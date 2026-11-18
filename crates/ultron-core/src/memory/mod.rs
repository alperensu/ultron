//! Aşama 5a ikinci beyin: hafıza sözleşmeleri.
//!
//! Bu modül yalnız tipleri, doğrulamayı ve yetki matrisini tanımlar. Kasa yazımı, indeks,
//! getirme ve özet üretimi sonraki görevlerin (kasa, SQLite depoları, getirme, daemon servisi)
//! sahipliğindedir; burada davranış yoktur.

pub mod contracts;

pub use contracts::*;
