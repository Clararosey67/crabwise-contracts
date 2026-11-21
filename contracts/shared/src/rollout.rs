//! Gradual rollout of new contract versions (closes #684).
//!
//! Supports:
//!
//! 1. **Canary deployments** — a canary contract ID plus a traffic share.
//! 2. **Feature flags** — named, admin-toggled booleans.
//! 3. **Traffic splitting** — sticky per-caller routing via SHA-256 of the
//!    caller address, compared against `canary_bps`.
//! 4. **Rollback triggers** — when health error-rate exceeds a threshold,
//!    `should_rollback` is true and `health_check` auto-disables the canary.
//! 5. **Manual rollback** — `trigger_rollback` zeroes canary traffic, disables
//!    flags, and pauses the contract.
//!
//! ## Storage schema — O(1) flag writes (closes #993)
//!
//! The old design stored a `Vec<Symbol>` under `FlagIndex` and rewrote it on
//! every `set_feature_flag` call, giving O(n) write cost for an n-flag
//! collection. This revision removes that vector entirely:
//!
//! - **Per-flag presence key** — `RolloutKey::FlagPresent(flag)` is a `bool`
//!   written once (the first time a flag is set) and never read again during a
//!   write. This replaces the linear deduplication scan.
//! - **Counter** — `RolloutKey::FlagCount` tracks the number of distinct flags
//!   so `RolloutState::flag_count` still works without the index vector.
//!
//! New flags are validated against both the effective feature-flag cap and
//! the bounded rollback list before any storage is changed. Existing flags
//! remain mutable at or above a lowered cap and do not rewrite the list.
//! `disable_all_flags` enumerates the bounded `FlagList` during rollback.
//!
//! Procedures are documented in `docs/DEPLOY.md`.

use crate::{health, validation};
use soroban_sdk::{
    contracterror, contracttype, panic_with_error, symbol_short, xdr::ToXdr, Address, Env, Symbol,
    Vec,
};

const BPS_DENOM: u32 = 10_000;
/// Default error-rate (bps) that arms an automatic canary rollback.
pub const DEFAULT_ROLLBACK_ERROR_BPS: u32 = 500;
/// Hard cap on the number of feature flags this module tracks in `FlagList`.
/// Enforced in `set_feature_flag` to prevent unbounded storage growth (#993).
pub const FLAG_LIST_CAP: u32 = 64;

/// Hard ceiling on the number of named feature flags a single contract may
/// carry (closes #985).
///
/// The flag index lives in **instance** storage and is rewritten on every flag
/// change, so an unbounded index grows until it hits the contract's instance
/// size limit — at which point *every* flag write fails, including the one an
/// operator would use to roll back. The index is therefore capped.
///
/// 32 is deliberately small: feature flags are a fixed vocabulary ("new
/// checkout", "beta feed"), not a growing registry, and the whole index is
/// rewritten on every change. It is well inside the instance-size budget while
/// leaving room for the canary and health blocks (`shared::rollout`,
/// `shared::health`) that share the same tier. This value is the **ceiling**:
/// an admin may lower the effective cap with [`set_flag_cap`], but never raise
/// it without shipping new code.
pub const MAX_FEATURE_FLAGS: u32 = 32;

/// Typed failures of the feature-flag cap (closes #985).
///
/// The matching entry in the unified catalogue
/// (`shared::errors::SharedErrorCode`) is `TooManyFeatureFlags = 19`.
#[contracterror]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RolloutError {
    /// The contract already holds the effective cap of feature flags; adding
    /// another would grow the instance-stored index past the limit. The
    /// current cap is returned by [`flag_cap`].
    FlagLimitReached = 1,
    /// A requested cap of `0` (every flag write blocked) or one above
    /// [`MAX_FEATURE_FLAGS`].
    InvalidFlagCap = 2,
}

impl core::fmt::Display for RolloutError {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        match self {
            Self::FlagLimitReached => write!(
                f,
                "feature-flag cap reached; see rollout::flag_cap for the current limit"
            ),
            Self::InvalidFlagCap => write!(f, "feature-flag cap must be between 1 and the ceiling"),
        }
    }
}

pub fn get_suggestion(error: RolloutError) -> Symbol {
    match error {
        RolloutError::FlagLimitReached => symbol_short!("FLAG_CAP"),
        RolloutError::InvalidFlagCap => symbol_short!("BAD_CAP"),
    }
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RolloutKey {
    Phase,
    CanaryBps,
    Canary,
    Stable,
    RollbackErrorBps,
    /// Payload: `bool` — current enabled/disabled state of one flag.
    Flag(Symbol),
    /// Presence sentinel: `bool` — written once when a new flag is first seen.
    /// Never mutated after creation. Allows O(1) deduplication in
    /// `set_feature_flag` without scanning the whole list.
    FlagPresent(Symbol),
    /// Counter: `u32` — number of distinct flags ever registered.
    FlagCount,
    /// Compact enumeration list: `Vec<Symbol>` — kept for the rollback path
    /// only; bounded by `FLAG_LIST_CAP`. Not written on ordinary flag updates.
    ///
    /// This replaces the old `FlagIndex` key (#993). `FlagIndex` is retained as
    /// a dead variant so that a deployed contract that has an old
    /// `FlagIndex` entry in storage does not treat it as an unknown key.
    FlagList,
    /// Retained to avoid unknown-key panics on contracts that stored the old
    /// `FlagIndex` vector. No longer written by this module.
    #[allow(dead_code)]
    FlagIndex,
    /// Optional admin-lowered feature-flag cap; absent means
    /// [`MAX_FEATURE_FLAGS`] (#985). Kept separate from `FlagIndex` so reading
    /// the cap never walks the index.
    MaxFlags,
}

#[contracttype]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RolloutPhase {
    /// No canary traffic; flags may still be used independently.
    Off = 0,
    /// A fraction of callers are sent to the canary contract.
    Canary = 1,
    /// All traffic is on the new version (`canary_bps == 10000`).
    Full = 2,
    /// Canary has been disabled after a trigger or manual rollback.
    RolledBack = 3,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RolloutState {
    pub phase: RolloutPhase,
    pub canary_bps: u32,
    pub canary: Option<Address>,
    pub stable: Option<Address>,
    pub rollback_error_bps: u32,
    /// Number of distinct feature flags ever registered (from `FlagCount`).
    pub flag_count: u32,
    /// Effective feature-flag cap, never above [`MAX_FEATURE_FLAGS`] (#985).
    pub flag_cap: u32,
}

// ── Internal helpers ─────────────────────────────────────────────────────────

/// Read the bounded flag-name list used by the rollback path.
/// Returns an empty `Vec` when no flags have been registered yet.
fn flag_list(env: &Env) -> Vec<Symbol> {
    env.storage()
        .instance()
        .get(&RolloutKey::FlagList)
        .unwrap_or_else(|| Vec::new(env))
}

/// The effective feature-flag cap for this contract (#985).
///
/// Defaults to [`MAX_FEATURE_FLAGS`] and is clamped to it on read, so even a
/// value written by an older/newer build cannot exceed the code ceiling.
pub fn flag_cap(env: &Env) -> u32 {
    match env
        .storage()
        .instance()
        .get::<RolloutKey, u32>(&RolloutKey::MaxFlags)
    {
        Some(cap) if cap > 0 => cap.min(MAX_FEATURE_FLAGS),
        _ => MAX_FEATURE_FLAGS,
    }
}

/// Lower the feature-flag cap, within the code ceiling (#985).
///
/// Allowing only a *lowering* means future tuning never requires shipping new
/// code, while the ceiling in [`MAX_FEATURE_FLAGS`] keeps the hard bound
/// auditable. Lowering below the current flag count is permitted (existing
/// flags keep working) but blocks any new flag until it is raised again.
pub fn set_flag_cap(env: &Env, cap: u32) -> Result<(), RolloutError> {
    if cap == 0 || cap > MAX_FEATURE_FLAGS {
        return Err(RolloutError::InvalidFlagCap);
    }
    env.storage().instance().set(&RolloutKey::MaxFlags, &cap);
    env.events().publish((symbol_short!("flag_cap"),), cap);
    Ok(())
}

fn flag_count(env: &Env) -> u32 {
    env.storage()
        .instance()
        .get(&RolloutKey::FlagCount)
        .unwrap_or(0)
}

// ── Public API ───────────────────────────────────────────────────────────────

pub fn get_canary_bps(env: &Env) -> u32 {
    env.storage()
        .instance()
        .get(&RolloutKey::CanaryBps)
        .unwrap_or(0)
}

pub fn get_phase(env: &Env) -> RolloutPhase {
    env.storage()
        .instance()
        .get(&RolloutKey::Phase)
        .unwrap_or(RolloutPhase::Off)
}

pub fn get_rollback_error_bps(env: &Env) -> u32 {
    env.storage()
        .instance()
        .get(&RolloutKey::RollbackErrorBps)
        .unwrap_or(DEFAULT_ROLLBACK_ERROR_BPS)
}

pub fn get_state(env: &Env) -> RolloutState {
    RolloutState {
        phase: get_phase(env),
        canary_bps: get_canary_bps(env),
        canary: env.storage().instance().get(&RolloutKey::Canary),
        stable: env.storage().instance().get(&RolloutKey::Stable),
        rollback_error_bps: get_rollback_error_bps(env),
        flag_count: flag_count(env),
        flag_cap: flag_cap(env),
    }
}

fn phase_from_bps(bps: u32) -> RolloutPhase {
    if bps == 0 {
        RolloutPhase::Off
    } else if bps >= BPS_DENOM {
        RolloutPhase::Full
    } else {
        RolloutPhase::Canary
    }
}

/// Register canary/stable contract IDs and the share of traffic sent to canary.
pub fn set_canary_deployment(env: &Env, canary: Address, stable: Address, canary_bps: u32) {
    if canary_bps > BPS_DENOM {
        panic!("canary_bps exceeds 10000");
    }
    env.storage().instance().set(&RolloutKey::Canary, &canary);
    env.storage().instance().set(&RolloutKey::Stable, &stable);
    env.storage()
        .instance()
        .set(&RolloutKey::CanaryBps, &canary_bps);
    let phase = phase_from_bps(canary_bps);
    env.storage().instance().set(&RolloutKey::Phase, &phase);
    env.events()
        .publish((symbol_short!("canary"),), (canary, stable, canary_bps));
}

pub fn set_canary_bps(env: &Env, canary_bps: u32) {
    if canary_bps > BPS_DENOM {
        panic!("canary_bps exceeds 10000");
    }
    env.storage()
        .instance()
        .set(&RolloutKey::CanaryBps, &canary_bps);
    env.storage()
        .instance()
        .set(&RolloutKey::Phase, &phase_from_bps(canary_bps));
}

/// Sticky traffic split: SHA-256(caller) mod 10000 < canary_bps.
///
/// Returns `false` when the rollout has been rolled back or the share is 0.
pub fn route_to_canary(env: &Env, caller: &Address) -> bool {
    if get_phase(env) == RolloutPhase::RolledBack {
        return false;
    }
    let canary_bps = get_canary_bps(env);
    if canary_bps == 0 {
        return false;
    }
    if canary_bps >= BPS_DENOM {
        return true;
    }
    let digest = env.crypto().sha256(&caller.clone().to_xdr(env));
    let bytes = digest.to_array();
    let bucket = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) % BPS_DENOM;
    bucket < canary_bps
}

/// Set or update a feature flag.
///
/// New flags must fit both the effective cap and the rollback list before any
/// payload, presence marker, counter, list, or event is written. Existing flags
/// can still be toggled at capacity (including after the cap is lowered), with
/// one payload write and no list rewrite.
pub fn set_feature_flag(env: &Env, flag: &Symbol, enabled: bool) {
    let already_present: bool = env
        .storage()
        .instance()
        .get(&RolloutKey::FlagPresent(flag.clone()))
        .unwrap_or(false);

    if !already_present {
        let count = flag_count(env);
        let mut list = flag_list(env);
        let cap = flag_cap(env);
        if !validation::has_capacity_for_one(count, cap)
            || !validation::has_capacity_for_one(list.len(), cap.min(FLAG_LIST_CAP))
        {
            panic_with_error!(env, RolloutError::FlagLimitReached);
        }

        list.push_back(flag.clone());
        env.storage().instance().set(&RolloutKey::FlagList, &list);
        env.storage()
            .instance()
            .set(&RolloutKey::FlagPresent(flag.clone()), &true);
        env.storage()
            .instance()
            .set(&RolloutKey::FlagCount, &(count + 1));
    }

    env.storage()
        .instance()
        .set(&RolloutKey::Flag(flag.clone()), &enabled);
    env.events()
        .publish((symbol_short!("feat_flg"),), (flag.clone(), enabled));
}

pub fn is_feature_enabled(env: &Env, flag: &Symbol) -> bool {
    if get_phase(env) == RolloutPhase::RolledBack {
        return false;
    }
    env.storage()
        .instance()
        .get(&RolloutKey::Flag(flag.clone()))
        .unwrap_or(false)
}

pub fn set_rollback_trigger(env: &Env, error_bps: u32) {
    if error_bps == 0 || error_bps > BPS_DENOM {
        panic!("invalid rollback trigger");
    }
    env.storage()
        .instance()
        .set(&RolloutKey::RollbackErrorBps, &error_bps);
    env.events().publish((symbol_short!("rb_trig"),), error_bps);
}

/// `true` when health error-rate meets the rollback trigger, or phase is already RolledBack.
pub fn should_rollback(env: &Env) -> bool {
    if get_phase(env) == RolloutPhase::RolledBack {
        return true;
    }
    let metrics = health::get_metrics(env);
    health::error_bps(&metrics) >= get_rollback_error_bps(env)
        && (metrics.ok_count + metrics.error_count) > 0
}

/// Disable all known flags. Used by the rollback path.
///
/// Reads the bounded `FlagList` (at most `FLAG_LIST_CAP` entries) and writes
/// `false` for each. This is an emergency / administrative path, so the
/// bounded O(n) scan here is acceptable and far cheaper than the old O(n)
/// rewrite on every ordinary setter call.
fn disable_all_flags(env: &Env) {
    let list = flag_list(env);
    for flag in list.iter() {
        env.storage()
            .instance()
            .set(&RolloutKey::Flag(flag.clone()), &false);
    }
}

fn apply_rollback(env: &Env) {
    env.storage().instance().set(&RolloutKey::CanaryBps, &0_u32);
    env.storage()
        .instance()
        .set(&RolloutKey::Phase, &RolloutPhase::RolledBack);
    disable_all_flags(env);
    env.events()
        .publish((symbol_short!("rollback"),), env.ledger().sequence());
}

/// Automatic rollback used by health checks when the trigger fires.
pub fn maybe_auto_rollback(env: &Env) -> bool {
    if get_phase(env) == RolloutPhase::RolledBack {
        return true;
    }
    if get_canary_bps(env) == 0 && flag_list(env).is_empty() {
        return false;
    }
    if should_rollback(env) {
        apply_rollback(env);
        return true;
    }
    false
}

/// Admin-initiated rollback: disable canary + flags and pause the contract.
/// Caller must already have authorized `admin` (the contract wrapper calls
/// `require_auth` once to avoid a double-auth HostError).
pub fn trigger_rollback(env: &Env, admin: &Address) {
    apply_rollback(env);
    env.storage()
        .instance()
        .set(&crate::pause::PauseDataKey::Paused, &true);
    env.events().publish(
        (soroban_sdk::Symbol::new(env, "contract_paused"),),
        crate::pause::ContractPausedEvent {
            admin: admin.clone(),
        },
    );
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use soroban_sdk::testutils::Address as _;

    /// A distinct symbol per index, so a cap of 32 means 32 real flags.
    fn flag(env: &Env, i: u32) -> Symbol {
        Symbol::new(env, std::format!("f{}", i).as_str())
    }

    /// Instance storage is only reachable from inside a deployed contract.
    fn with_contract<T>(env: &Env, f: impl FnOnce() -> T) -> T {
        let contract_id = env.register_contract(None, crate::TestContract);
        env.as_contract(&contract_id, f)
    }

    #[test]
    fn flag_cap_defaults_to_the_ceiling() {
        let env = Env::default();
        with_contract(&env, || {
            assert_eq!(flag_cap(&env), MAX_FEATURE_FLAGS);
            assert_eq!(get_state(&env).flag_cap, MAX_FEATURE_FLAGS);
        });
    }

    #[test]
    fn set_flag_cap_rejects_zero_and_anything_above_the_ceiling() {
        let env = Env::default();
        with_contract(&env, || {
            assert_eq!(set_flag_cap(&env, 0), Err(RolloutError::InvalidFlagCap));
            assert_eq!(
                set_flag_cap(&env, MAX_FEATURE_FLAGS + 1),
                Err(RolloutError::InvalidFlagCap)
            );
            // A lowering within the ceiling is accepted and reported.
            assert_eq!(set_flag_cap(&env, 4), Ok(()));
            assert_eq!(flag_cap(&env), 4);
        });
    }

    #[test]
    fn exactly_the_cap_is_accepted() {
        let env = Env::default();
        with_contract(&env, || {
            for i in 0..MAX_FEATURE_FLAGS {
                set_feature_flag(&env, &flag(&env, i), true);
            }
            assert_eq!(get_state(&env).flag_count, MAX_FEATURE_FLAGS);
        });
    }

    #[test]
    fn one_past_the_cap_is_rejected() {
        let env = Env::default();
        let id = env.register_contract(None, crate::TestContract);
        for i in 0..MAX_FEATURE_FLAGS {
            env.as_contract(&id, || set_feature_flag(&env, &flag(&env, i), true));
        }
        // `catch_unwind` wraps the whole frame: the host re-raises a contract
        // error when the frame exits.
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            env.as_contract(&id, || {
                set_feature_flag(&env, &flag(&env, MAX_FEATURE_FLAGS), true)
            });
        }));
        assert!(res.is_err(), "flag {} must be refused", MAX_FEATURE_FLAGS);
        // The refused flag was not recorded.
        assert_eq!(
            env.as_contract(&id, || get_state(&env).flag_count),
            MAX_FEATURE_FLAGS
        );
    }

    #[test]
    fn toggling_an_existing_flag_is_allowed_at_the_cap() {
        let env = Env::default();
        with_contract(&env, || {
            for i in 0..MAX_FEATURE_FLAGS {
                set_feature_flag(&env, &flag(&env, i), true);
            }
            // Re-setting an existing flag does not grow the index, so it is fine.
            set_feature_flag(&env, &flag(&env, 0), false);
            assert!(!is_feature_enabled(&env, &flag(&env, 0)));
            assert_eq!(get_state(&env).flag_count, MAX_FEATURE_FLAGS);
        });
    }

    #[test]
    fn a_lowered_cap_blocks_new_flags_but_leaves_existing_ones_working() {
        let env = Env::default();
        let id = env.register_contract(None, crate::TestContract);
        env.as_contract(&id, || {
            for i in 0..4 {
                set_feature_flag(&env, &flag(&env, i), true);
            }
            set_flag_cap(&env, 2).unwrap();
            // Existing flags keep their value...
            assert!(is_feature_enabled(&env, &flag(&env, 1)));
        });
        // ...but a new one is refused.
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            env.as_contract(&id, || set_feature_flag(&env, &flag(&env, 9), true));
        }));
        assert!(res.is_err());
    }

    #[test]
    fn error_codes_are_stable() {
        assert_eq!(RolloutError::FlagLimitReached as u32, 1);
        assert_eq!(RolloutError::InvalidFlagCap as u32, 2);
        assert_eq!(MAX_FEATURE_FLAGS, 32);
    }

    #[test]
    fn rejected_flag_preserves_rollout_state_and_all_registered_flags() {
        for cap in [1, 4, MAX_FEATURE_FLAGS] {
            let env = Env::default();
            let id = env.register_contract(None, crate::TestContract);
            let (before, list_before) = env.as_contract(&id, || {
                set_canary_deployment(
                    &env,
                    Address::generate(&env),
                    Address::generate(&env),
                    2_500,
                );
                set_rollback_trigger(&env, 750);
                set_flag_cap(&env, cap).unwrap();
                for i in 0..cap {
                    set_feature_flag(&env, &flag(&env, i), i % 2 == 0);
                }
                (get_state(&env), flag_list(&env))
            });

            for enabled in [true, false] {
                let rejected = flag(&env, cap);
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    env.as_contract(&id, || set_feature_flag(&env, &rejected, enabled));
                }));
                assert!(result.is_err());
                env.as_contract(&id, || {
                    assert_eq!(get_state(&env), before);
                    assert_eq!(flag_list(&env), list_before);
                    assert!(!env
                        .storage()
                        .instance()
                        .has(&RolloutKey::Flag(rejected.clone())));
                    assert!(!env
                        .storage()
                        .instance()
                        .has(&RolloutKey::FlagPresent(rejected)));
                    for i in 0..cap {
                        assert_eq!(is_feature_enabled(&env, &flag(&env, i)), i % 2 == 0);
                        assert_eq!(
                            env.storage()
                                .instance()
                                .get::<_, bool>(&RolloutKey::FlagPresent(flag(&env, i))),
                            Some(true)
                        );
                    }
                });
            }
        }
    }

    #[test]
    fn full_tracking_list_cannot_bypass_capacity_validation() {
        let env = Env::default();
        let id = env.register_contract(None, crate::TestContract);
        // Exercise the old saturated-list branch, which silently skipped the
        // append and its capacity check. Such stored data must fail closed.
        let before = env.as_contract(&id, || {
            let mut list = Vec::new(&env);
            for i in 0..FLAG_LIST_CAP {
                list.push_back(flag(&env, i));
            }
            env.storage().instance().set(&RolloutKey::FlagList, &list);
            (get_state(&env), list)
        });
        let rejected = flag(&env, FLAG_LIST_CAP);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            env.as_contract(&id, || set_feature_flag(&env, &rejected, true));
        }));
        assert!(result.is_err());
        env.as_contract(&id, || {
            assert_eq!(get_state(&env), before.0);
            assert_eq!(flag_list(&env), before.1);
            assert!(!env
                .storage()
                .instance()
                .has(&RolloutKey::Flag(rejected.clone())));
            assert!(!env
                .storage()
                .instance()
                .has(&RolloutKey::FlagPresent(rejected)));
        });
    }
}
