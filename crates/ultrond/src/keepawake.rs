//! Keep-awake yaşam döngüsü.
//!
//! `SetThreadExecutionState` iş parçacığına bağlı olduğundan yürütme durumu ayrılmış bir
//! işletim sistemi iş parçacığında tutulur. Aktif turlar ve kota zamanlayıcıları aynı
//! referans sayımlı guard'ı kullanır; son guard bırakıldığında durum aynı iş parçacığında
//! temizlenir.

use std::collections::HashMap;
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;

use windows_sys::Win32::System::Power::{
    ES_CONTINUOUS, ES_SYSTEM_REQUIRED, SetThreadExecutionState,
};

const ACTIVE_FLAGS: u32 = ES_CONTINUOUS | ES_SYSTEM_REQUIRED;
const RELEASE_FLAGS: u32 = ES_CONTINUOUS;

/// Keep-awake tutucusunun sahibi; tanılama için guard üzerinde korunur.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum KeepAwakeReason {
    Turn,
    QuotaTimer,
}

/// Başarısız güç çağrısının hangi geçişte olduğunu belirtir.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PowerCallPhase {
    Acquire,
    Release,
}

/// Güç çağrısının başarısız olma nedenini belirtir.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PowerCallFailureKind {
    /// Win32 güç çağrısı sıfır döndürdü.
    ReturnedZero,
    /// Güç çağrısı veya ayrılmış iş parçacığı beklenen yanıtı üretemedi.
    WorkerStopped,
    /// Ayrılmış iş parçacığı başlatılamadı.
    WorkerUnavailable,
}

/// Keep-awake güç çağrısı başarısızlığının güvenli tanılama bilgisi.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PowerCallFailure {
    pub phase: PowerCallPhase,
    pub kind: PowerCallFailureKind,
}

/// Güç bayrağının doğrulanmış durumu. `Awake` yalnızca Win32 çağrısı sıfır
/// olmayan sonuç verdiğinde kullanılır.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KeepAwakePowerStatus {
    Idle,
    Awake,
    Failed(PowerCallFailure),
}

trait PowerCall: Send + Sync + 'static {
    fn set_execution_state(&self, flags: u32) -> u32;
}

struct Win32PowerCall;

impl PowerCall for Win32PowerCall {
    fn set_execution_state(&self, flags: u32) -> u32 {
        // SAFETY: bu adaptör yalnız ayrılmış keep-awake iş parçacığında çağrılır.
        unsafe { SetThreadExecutionState(flags) }
    }
}

struct PowerSession {
    release: Sender<()>,
    thread: JoinHandle<u32>,
}

impl PowerSession {
    fn stop(self) -> Result<u32, ()> {
        let _ = self.release.send(());
        self.thread.join().map_err(|_| ())
    }
}

#[derive(Default)]
struct State {
    holders: usize,
    generation: u64,
    holders_by_reason: HashMap<KeepAwakeReason, usize>,
    session: Option<PowerSession>,
    power_status: Option<KeepAwakePowerStatus>,
}

struct Inner {
    state: Mutex<State>,
    /// Güç oturumu geçişlerini (başlatma, yeniden deneme, durdurma) seri hale getirir.
    /// Güç çağrıları bu kapı altında, `state` kilidi tutulmadan yapılır.
    power_gate: Mutex<()>,
    power_call: Arc<dyn PowerCall>,
}

/// Keep-awake tutucularının paylaşılan yaşam döngüsü.
#[derive(Clone)]
pub struct KeepAwake {
    inner: Arc<Inner>,
}

/// Keep-awake tutucusu. Son tutucu bırakıldığında güç durumu aynı OS thread'inde temizlenir.
pub struct KeepAwakeGuard {
    inner: Arc<Inner>,
    generation: u64,
    reason: KeepAwakeReason,
}

impl Default for KeepAwake {
    fn default() -> Self {
        Self {
            inner: Arc::new(Inner {
                state: Mutex::new(State::default()),
                power_gate: Mutex::new(()),
                power_call: Arc::new(Win32PowerCall),
            }),
        }
    }
}

impl KeepAwake {
    pub fn new() -> KeepAwake {
        KeepAwake::default()
    }

    /// Bir aktif tur için tutucu edinir; mevcut çağrı noktalarıyla uyumludur.
    pub fn acquire(&self) -> KeepAwakeGuard {
        self.acquire_for(KeepAwakeReason::Turn)
    }

    /// Bir aktif tur ya da gerçek bekleyen kota zamanlayıcısı için tutucu edinir.
    pub fn acquire_for(&self, reason: KeepAwakeReason) -> KeepAwakeGuard {
        let (generation, needs_power) = {
            let mut state = lock_state(&self.inner.state);
            let fresh = state.holders == 0;
            if fresh {
                state.generation = state.generation.wrapping_add(1);
            }
            state.holders = state.holders.saturating_add(1);
            let reason_holders = state.holders_by_reason.entry(reason).or_default();
            *reason_holders = reason_holders.saturating_add(1);
            let needs_power = fresh
                || state.session.is_none()
                || matches!(state.power_status, Some(KeepAwakePowerStatus::Failed(_)));
            (state.generation, needs_power)
        };
        let guard = KeepAwakeGuard {
            inner: self.inner.clone(),
            generation,
            reason,
        };
        if needs_power {
            self.ensure_power(generation);
        }
        guard
    }

    /// Güç tutucularının toplam sayısı; çalışan tur sayacı değildir.
    pub fn holders(&self) -> usize {
        self.inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .holders
    }

    /// Tek bir nedene bağlı güç tutucularının sayısı.
    pub fn holders_for(&self, reason: KeepAwakeReason) -> usize {
        self.inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .holders_by_reason
            .get(&reason)
            .copied()
            .unwrap_or(0)
    }

    /// Son güç geçişinin güvenli durumu. Başarısız bir çağrı `Awake` olarak bildirilmez.
    pub fn power_status(&self) -> KeepAwakePowerStatus {
        self.inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .power_status
            .unwrap_or(KeepAwakePowerStatus::Idle)
    }

    /// Aktif guard'ları iptal eder ve ayrılmış thread'in bırakma çağrısını tamamlar.
    /// Eski guard'ların daha sonra düşmesi sonraki bir yaşam döngüsünü etkilemez.
    pub fn shutdown(&self) {
        let retired = {
            let mut state = lock_state(&self.inner.state);
            if state.holders == 0 && state.session.is_none() {
                return;
            }

            state.holders = 0;
            state.holders_by_reason.clear();
            state.generation = state.generation.wrapping_add(1);
            let generation = state.generation;
            state.session.take().map(|session| (session, generation))
        };
        if let Some((session, generation)) = retired {
            let _gate = lock_gate(&self.inner.power_gate);
            let result = session.stop();
            finish_release(&self.inner.state, generation, result);
        }
    }

    /// Güç oturumu yoksa ya da son edinme başarısızsa ve tutucu hâlâ geçerliyse oturumu kurar.
    /// Güç çağrıları `power_gate` altında, `state` kilidi tutulmadan yapılır.
    fn ensure_power(&self, generation: u64) {
        let _gate = lock_gate(&self.inner.power_gate);

        let stale = {
            let mut state = lock_state(&self.inner.state);
            if state.generation != generation || state.holders == 0 {
                return;
            }
            let failed = matches!(state.power_status, Some(KeepAwakePowerStatus::Failed(_)));
            if state.session.is_some() && !failed {
                return;
            }
            state.session.take()
        };
        if let Some(old) = stale {
            let _ = old.stop();
        }

        let started = start_power_session(self.inner.power_call.clone());
        let mut state = lock_state(&self.inner.state);
        if state.generation != generation || state.holders == 0 || state.session.is_some() {
            drop(state);
            if let Ok((session, _)) = started {
                let _ = session.stop();
            }
            return;
        }
        let status = match started {
            Ok((session, acquire_result)) => {
                state.session = Some(session);
                acquire_status(acquire_result)
            }
            Err(()) => KeepAwakePowerStatus::Failed(PowerCallFailure {
                phase: PowerCallPhase::Acquire,
                kind: PowerCallFailureKind::WorkerUnavailable,
            }),
        };
        state.power_status = Some(status);
    }
}

impl KeepAwakeGuard {
    /// Bu guard'ı oluşturan iş gerekçesi.
    pub fn reason(&self) -> KeepAwakeReason {
        self.reason
    }
}

impl Drop for KeepAwakeGuard {
    fn drop(&mut self) {
        let retired = {
            let mut state = lock_state(&self.inner.state);
            if state.generation != self.generation || state.holders == 0 {
                return;
            }

            state.holders -= 1;
            if let Some(reason_holders) = state.holders_by_reason.get_mut(&self.reason) {
                *reason_holders -= 1;
                if *reason_holders == 0 {
                    state.holders_by_reason.remove(&self.reason);
                }
            }
            if state.holders == 0 {
                let generation = state.generation;
                state.session.take().map(|session| (session, generation))
            } else {
                None
            }
        };
        if let Some((session, generation)) = retired {
            let _gate = lock_gate(&self.inner.power_gate);
            let result = session.stop();
            finish_release(&self.inner.state, generation, result);
        }
    }
}

fn start_power_session(
    power_call: Arc<dyn PowerCall>,
) -> Result<(PowerSession, Result<u32, ()>), ()> {
    if take_injected_spawn_failure() {
        return Err(());
    }
    let (release_tx, release_rx) = channel::<()>();
    let (acquire_tx, acquire_rx) = channel::<u32>();
    let thread = std::thread::Builder::new()
        .name("ultron-keepawake".to_owned())
        .spawn(move || {
            let acquire_result = power_call.set_execution_state(ACTIVE_FLAGS);
            let _ = acquire_tx.send(acquire_result);
            let _ = release_rx.recv();
            power_call.set_execution_state(RELEASE_FLAGS)
        })
        .map_err(|_| ())?;

    let session = PowerSession {
        release: release_tx,
        thread,
    };
    let acquire_result = acquire_rx.recv().map_err(|_| ());
    Ok((session, acquire_result))
}

fn lock_state(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn lock_gate(gate: &Mutex<()>) -> MutexGuard<'_, ()> {
    gate.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn acquire_status(result: Result<u32, ()>) -> KeepAwakePowerStatus {
    match result {
        Ok(0) => KeepAwakePowerStatus::Failed(PowerCallFailure {
            phase: PowerCallPhase::Acquire,
            kind: PowerCallFailureKind::ReturnedZero,
        }),
        Ok(_) => KeepAwakePowerStatus::Awake,
        Err(()) => KeepAwakePowerStatus::Failed(PowerCallFailure {
            phase: PowerCallPhase::Acquire,
            kind: PowerCallFailureKind::WorkerStopped,
        }),
    }
}

/// Oturum durdurulduktan sonra sonucu uygular. Başarılı bırakma her önceki durumdan `Idle`
/// olur. Üretilmesi sırasında yeni bir yaşam döngüsü başladıysa sonuç uygulanmaz.
fn finish_release(state: &Mutex<State>, generation: u64, result: Result<u32, ()>) {
    let mut state = lock_state(state);
    if state.generation != generation {
        return;
    }
    state.power_status = Some(match result {
        Ok(0) => KeepAwakePowerStatus::Failed(PowerCallFailure {
            phase: PowerCallPhase::Release,
            kind: PowerCallFailureKind::ReturnedZero,
        }),
        Ok(_) => KeepAwakePowerStatus::Idle,
        Err(()) => KeepAwakePowerStatus::Failed(PowerCallFailure {
            phase: PowerCallPhase::Release,
            kind: PowerCallFailureKind::WorkerStopped,
        }),
    });
}

#[cfg(test)]
thread_local! {
    static FAIL_NEXT_SPAWN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
fn take_injected_spawn_failure() -> bool {
    FAIL_NEXT_SPAWN.with(|flag| flag.replace(false))
}

#[cfg(not(test))]
fn take_injected_spawn_failure() -> bool {
    false
}

#[cfg(test)]
fn with_power_call(power_call: Arc<dyn PowerCall>) -> KeepAwake {
    KeepAwake {
        inner: Arc::new(Inner {
            state: Mutex::new(State::default()),
            power_gate: Mutex::new(()),
            power_call,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ACTIVE_FLAGS, KeepAwake, KeepAwakePowerStatus, KeepAwakeReason, PowerCall,
        PowerCallFailure, PowerCallFailureKind, PowerCallPhase, RELEASE_FLAGS, with_power_call,
    };
    use std::collections::VecDeque;
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    #[derive(Default)]
    struct RecordingPowerCall {
        calls: Mutex<Vec<u32>>,
        results: Mutex<VecDeque<u32>>,
    }

    impl RecordingPowerCall {
        fn with_results(results: impl IntoIterator<Item = u32>) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                results: Mutex::new(results.into_iter().collect()),
            }
        }

        fn calls(&self) -> Vec<u32> {
            self.calls
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
        }
    }

    impl PowerCall for RecordingPowerCall {
        fn set_execution_state(&self, flags: u32) -> u32 {
            self.calls
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(flags);
            self.results
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .pop_front()
                .unwrap_or(1)
        }
    }

    fn test_keepawake(
        results: impl IntoIterator<Item = u32>,
    ) -> (KeepAwake, Arc<RecordingPowerCall>) {
        let power_call = Arc::new(RecordingPowerCall::with_results(results));
        (with_power_call(power_call.clone()), power_call)
    }

    #[test]
    fn two_turns_and_timer_release_only_after_last_holder_in_drop_order() {
        let (keep, power_call) = test_keepawake([]);
        let first_turn = keep.acquire();
        let second_turn = keep.acquire_for(KeepAwakeReason::Turn);
        let timer = keep.acquire_for(KeepAwakeReason::QuotaTimer);

        assert_eq!(first_turn.reason(), KeepAwakeReason::Turn);
        assert_eq!(timer.reason(), KeepAwakeReason::QuotaTimer);
        assert_eq!(keep.holders(), 3);
        assert_eq!(keep.holders_for(KeepAwakeReason::Turn), 2);
        assert_eq!(keep.holders_for(KeepAwakeReason::QuotaTimer), 1);
        assert_eq!(keep.power_status(), KeepAwakePowerStatus::Awake);
        assert_eq!(power_call.calls(), vec![ACTIVE_FLAGS]);

        drop(second_turn);
        drop(timer);
        assert_eq!(keep.holders(), 1);
        assert_eq!(power_call.calls(), vec![ACTIVE_FLAGS]);

        drop(first_turn);
        assert_eq!(keep.holders(), 0);
        assert_eq!(keep.power_status(), KeepAwakePowerStatus::Idle);
        assert_eq!(power_call.calls(), vec![ACTIVE_FLAGS, RELEASE_FLAGS]);
    }

    #[test]
    fn timer_to_turn_handoff_does_not_release_between_holders() {
        let (keep, power_call) = test_keepawake([]);
        let timer = keep.acquire_for(KeepAwakeReason::QuotaTimer);
        let turn = keep.acquire();

        drop(timer);
        assert_eq!(keep.holders(), 1);
        assert_eq!(keep.holders_for(KeepAwakeReason::Turn), 1);
        assert_eq!(power_call.calls(), vec![ACTIVE_FLAGS]);

        drop(turn);
        assert_eq!(power_call.calls(), vec![ACTIVE_FLAGS, RELEASE_FLAGS]);
    }

    #[test]
    fn shutdown_releases_once_and_old_guards_cannot_affect_next_session() {
        let (keep, power_call) = test_keepawake([]);
        let old_turn = keep.acquire();
        let old_timer = keep.acquire_for(KeepAwakeReason::QuotaTimer);

        keep.shutdown();
        assert_eq!(keep.holders(), 0);
        assert_eq!(keep.power_status(), KeepAwakePowerStatus::Idle);
        assert_eq!(power_call.calls(), vec![ACTIVE_FLAGS, RELEASE_FLAGS]);
        drop(old_turn);
        drop(old_timer);
        assert_eq!(keep.holders(), 0);
        assert_eq!(power_call.calls(), vec![ACTIVE_FLAGS, RELEASE_FLAGS]);

        let next_turn = keep.acquire();
        assert_eq!(keep.holders(), 1);
        drop(next_turn);
        assert_eq!(
            power_call.calls(),
            vec![ACTIVE_FLAGS, RELEASE_FLAGS, ACTIVE_FLAGS, RELEASE_FLAGS]
        );
    }

    #[test]
    fn zero_acquire_result_is_reported_and_never_claimed_as_awake() {
        let (keep, power_call) = test_keepawake([0, 1]);
        let guard = keep.acquire();

        assert_eq!(keep.holders(), 1);
        assert_eq!(
            keep.power_status(),
            KeepAwakePowerStatus::Failed(PowerCallFailure {
                phase: PowerCallPhase::Acquire,
                kind: PowerCallFailureKind::ReturnedZero,
            })
        );
        drop(guard);
        assert_eq!(keep.holders(), 0);
        assert_eq!(keep.power_status(), KeepAwakePowerStatus::Idle);
        assert_eq!(power_call.calls(), vec![ACTIVE_FLAGS, RELEASE_FLAGS]);
    }

    #[test]
    fn zero_release_result_is_reported() {
        let (keep, power_call) = test_keepawake([1, 0]);
        let guard = keep.acquire();
        drop(guard);

        assert_eq!(keep.holders(), 0);
        assert_eq!(
            keep.power_status(),
            KeepAwakePowerStatus::Failed(PowerCallFailure {
                phase: PowerCallPhase::Release,
                kind: PowerCallFailureKind::ReturnedZero,
            })
        );
        assert_eq!(power_call.calls(), vec![ACTIVE_FLAGS, RELEASE_FLAGS]);
    }

    #[test]
    fn worker_spawn_failure_is_retried_by_next_acquire() {
        let (keep, power_call) = test_keepawake([]);
        super::FAIL_NEXT_SPAWN.with(|flag| flag.set(true));

        let first = keep.acquire();
        assert_eq!(keep.holders(), 1);
        assert_eq!(
            keep.power_status(),
            KeepAwakePowerStatus::Failed(PowerCallFailure {
                phase: PowerCallPhase::Acquire,
                kind: PowerCallFailureKind::WorkerUnavailable,
            })
        );
        assert!(power_call.calls().is_empty());

        let second = keep.acquire();
        assert_eq!(keep.holders(), 2);
        assert_eq!(keep.power_status(), KeepAwakePowerStatus::Awake);
        assert_eq!(power_call.calls(), vec![ACTIVE_FLAGS]);

        drop(first);
        drop(second);
        assert_eq!(keep.holders(), 0);
        assert_eq!(keep.power_status(), KeepAwakePowerStatus::Idle);
        assert_eq!(power_call.calls(), vec![ACTIVE_FLAGS, RELEASE_FLAGS]);
    }

    #[test]
    fn failed_acquire_then_successful_retry_on_next_acquire() {
        let (keep, power_call) = test_keepawake([0]);
        let first = keep.acquire();
        assert_eq!(
            keep.power_status(),
            KeepAwakePowerStatus::Failed(PowerCallFailure {
                phase: PowerCallPhase::Acquire,
                kind: PowerCallFailureKind::ReturnedZero,
            })
        );

        let second = keep.acquire();
        assert_eq!(keep.holders(), 2);
        assert_eq!(keep.power_status(), KeepAwakePowerStatus::Awake);
        assert_eq!(
            power_call.calls(),
            vec![ACTIVE_FLAGS, RELEASE_FLAGS, ACTIVE_FLAGS]
        );

        drop(first);
        drop(second);
        assert_eq!(keep.power_status(), KeepAwakePowerStatus::Idle);
        assert_eq!(
            power_call.calls(),
            vec![ACTIVE_FLAGS, RELEASE_FLAGS, ACTIVE_FLAGS, RELEASE_FLAGS]
        );
    }

    #[test]
    fn idle_after_successful_release_following_failed_acquire() {
        let (keep, power_call) = test_keepawake([0, 1]);
        let guard = keep.acquire();
        assert!(matches!(
            keep.power_status(),
            KeepAwakePowerStatus::Failed(_)
        ));

        drop(guard);
        assert_eq!(keep.holders(), 0);
        assert_eq!(keep.power_status(), KeepAwakePowerStatus::Idle);
        assert_eq!(power_call.calls(), vec![ACTIVE_FLAGS, RELEASE_FLAGS]);
    }

    /// Yalnızca etkinleştirme çağrısını bloke eden sahte adaptör.
    struct BlockingPowerCall {
        entered: Mutex<mpsc::Sender<()>>,
        proceed: Mutex<mpsc::Receiver<()>>,
    }

    impl PowerCall for BlockingPowerCall {
        fn set_execution_state(&self, flags: u32) -> u32 {
            if flags == ACTIVE_FLAGS {
                let _ = self
                    .entered
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .send(());
                let _ = self
                    .proceed
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .recv();
            }
            1
        }
    }

    #[test]
    fn power_call_runs_without_holding_state_lock() {
        let (entered_tx, entered_rx) = mpsc::channel();
        let (proceed_tx, proceed_rx) = mpsc::channel();
        let power_call = Arc::new(BlockingPowerCall {
            entered: Mutex::new(entered_tx),
            proceed: Mutex::new(proceed_rx),
        });
        let keep = with_power_call(power_call);

        let acquiring = keep.clone();
        let acquirer = std::thread::spawn(move || acquiring.acquire());
        entered_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("power call should start");

        // Güç çağrısı bloke iken durum sorguları kilitlenmeden dönmelidir.
        assert_eq!(keep.holders(), 1);
        assert_eq!(keep.holders_for(KeepAwakeReason::Turn), 1);
        assert_eq!(keep.power_status(), KeepAwakePowerStatus::Idle);

        proceed_tx
            .send(())
            .expect("power call should still be waiting");
        let guard = acquirer.join().expect("acquire thread should not panic");
        assert_eq!(keep.power_status(), KeepAwakePowerStatus::Awake);
        drop(guard);
        assert_eq!(keep.power_status(), KeepAwakePowerStatus::Idle);
    }
}
