//! Keep-awake: herhangi bir tur çalışırken sistemin uykuya geçmesini engeller.
//!
//! `SetThreadExecutionState(ES_CONTINUOUS)` çağıran iş parçacığına bağlıdır; tokio görevleri
//! iş parçacığı değiştirdiği için bayrak ayrılmış bir işletim sistemi iş parçacığında tutulur
//! ve son tutucu bırakılınca geri alınır.

use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex};

use windows_sys::Win32::System::Power::{
    ES_CONTINUOUS, ES_SYSTEM_REQUIRED, SetThreadExecutionState,
};

#[derive(Default)]
struct State {
    holders: usize,
    release: Option<Sender<()>>,
}

/// Tutucu sayacı; 0→1'de bayrağı kurar, 1→0'da geri alır.
#[derive(Clone, Default)]
pub struct KeepAwake {
    state: Arc<Mutex<State>>,
}

/// Düşünce tutucuyu bırakır.
pub struct KeepAwakeGuard {
    state: Arc<Mutex<State>>,
}

impl KeepAwake {
    pub fn new() -> KeepAwake {
        KeepAwake::default()
    }

    /// Bir tur başlarken çağrılır.
    pub fn acquire(&self) -> KeepAwakeGuard {
        if let Ok(mut s) = self.state.lock() {
            s.holders += 1;
            if s.holders == 1 {
                let (tx, rx) = channel::<()>();
                s.release = Some(tx);
                std::thread::spawn(move || {
                    // SAFETY: yalnız bu iş parçacığının yürütme durumunu değiştirir.
                    unsafe { SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED) };
                    // Gönderici düşünce de `recv` döner.
                    let _ = rx.recv();
                    // SAFETY: yukarıdaki bayrağı geri alır.
                    unsafe { SetThreadExecutionState(ES_CONTINUOUS) };
                });
            }
        }
        KeepAwakeGuard {
            state: self.state.clone(),
        }
    }

    /// Şu an tutan tur sayısı.
    pub fn holders(&self) -> usize {
        self.state.lock().map_or(0, |s| s.holders)
    }
}

impl Drop for KeepAwakeGuard {
    fn drop(&mut self) {
        if let Ok(mut s) = self.state.lock() {
            s.holders = s.holders.saturating_sub(1);
            if s.holders == 0 {
                s.release = None;
            }
        }
    }
}
