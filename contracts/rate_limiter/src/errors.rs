//! Rate Limiter Errors
//!
//! Error definitions for the rate limiter module (closes #710).
//!
//! Follows the house pattern used by `contracts/escrow/src/errors.rs`: a
//! `#[contracterror]` enum of unit variants with explicit `= N` discriminants,
//! a `core::fmt::Display` impl, and a `symbol_short!` suggestion mapping.
//!
//! The old file here declared a `Diagnostic`-carrying struct (an API that does
//! not exist in `soroban-sdk`) and used it as the error type inside a
//! `#[contractimpl]` block, which cannot compile — a `#[contracterror]` enum is
//! required. Because a `#[contracterror]` enum cannot carry a payload, the
//! limit / window / count numbers that the old code tried to smuggle through
//! the error are now published in the `rl_check` and `rl_exceeded` events
//! instead.
//!
//! Discriminants are kept stable so that already-deployed error codes do not
//! change meaning.

use soroban_sdk::{contracterror, contracttype, symbol_short, Symbol};

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RateLimitExceeded {
    pub limit: u32,
    pub count: u32,
    pub retry_at_ledger: u32,
    pub retry_after_ledgers: u32,
}

pub fn limit_exceeded_response(
    limit: u32,
    count: u32,
    first_ledger: u32,
    window_ledgers: u32,
    current_ledger: u32,
) -> RateLimitExceeded {
    let retry_at_ledger = first_ledger
        .saturating_add(window_ledgers)
        .saturating_add(1);

    RateLimitExceeded {
        limit,
        count,
        retry_at_ledger,
        retry_after_ledgers: retry_at_ledger.saturating_sub(current_ledger),
    }
}

#[contracterror]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RateLimitError {
    /// No configuration exists for the requested rate limit key.
    NotInitialized = 1,
    /// `initialize` has already run; the limiter is single-shot.
    AlreadyInitialized = 2,
    /// A limit of zero would block every action, including recovery.
    InvalidLimit = 3,
    /// A zero-length window would expire on the very next ledger.
    InvalidWindow = 4,
    /// The account has used up its allowance for the current window.
    LimitExceeded = 5,
}

impl core::fmt::Display for RateLimitError {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        match self {
            Self::NotInitialized => write!(f, "no rate limit configured for this key"),
            Self::AlreadyInitialized => write!(f, "rate limiter already initialized"),
            Self::InvalidLimit => write!(f, "rate limit must be greater than zero"),
            Self::InvalidWindow => write!(f, "rate limit window must be greater than zero"),
            Self::LimitExceeded => write!(f, "rate limit exceeded for the current window"),
        }
    }
}

pub fn get_suggestion(error: RateLimitError) -> Symbol {
    match error {
        RateLimitError::NotInitialized => symbol_short!("NO_LIMIT"),
        RateLimitError::AlreadyInitialized => symbol_short!("DUP"),
        RateLimitError::InvalidLimit => symbol_short!("BAD_LIMIT"),
        RateLimitError::InvalidWindow => symbol_short!("NO_WINDOW"),
        RateLimitError::LimitExceeded => symbol_short!("LIMIT"),
    }
}
