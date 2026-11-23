//! Custom asset support and balance query helpers for Soroban campaign contract.

use soroban_sdk::{token, Address, Env, Symbol};

use crate::errors::Error;

/// Helper struct for managing custom asset validations and balances.
pub struct AssetHelper;

impl AssetHelper {
    /// Validates if an asset contract address is allowed for campaign contributions.
    pub fn is_asset_supported(env: &Env, asset_address: &Address) -> bool {
        env.storage()
            .instance()
            .get(&(Symbol::new(env, "supported_asset"), asset_address.clone()))
            .unwrap_or(false)
    }

    pub fn set_asset_supported(env: &Env, asset_address: &Address, supported: bool) {
        env.storage().instance().set(
            &(Symbol::new(env, "supported_asset"), asset_address.clone()),
            &supported,
        );
    }

    pub fn require_asset_supported(env: &Env, asset_address: &Address) -> Result<(), Error> {
        if Self::is_asset_supported(env, asset_address) {
            Ok(())
        } else {
            Err(Error::AssetNotAccepted)
        }
    }

    pub fn transfer_contribution(
        env: &Env,
        asset: &Address,
        from: &Address,
        to: &Address,
        amount: i128,
    ) -> Result<(), Error> {
        Self::require_asset_supported(env, asset)?;
        if amount <= 0 {
            return Err(Error::DonationTooSmall);
        }
        token::Client::new(env, asset).transfer(from, to, &amount);
        Ok(())
    }

    /// Queries the campaign asset balance for a specific participant address.
    #[allow(dead_code)]
    pub fn get_participant_asset_balance(
        env: &Env,
        participant: &Address,
        asset: &Address,
    ) -> i128 {
        env.storage()
            .persistent()
            .get(&(Symbol::new(env, "bal"), participant, asset))
            .unwrap_or(0i128)
    }
}
