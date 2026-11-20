//! End-to-end integration scenario demonstrating the framework (#663).
//!
//! Walks the full escrow lifecycle through contract boundaries: registry-based
//! dependency injection (#662), correlated events (#661), the classic release
//! path, and the atomic escrow→commission flow (#656).

use escrow::storage::CommissionStatus;
use integration_framework::assertions as a;
use integration_framework::dsl::Steps;
use integration_framework::{deploy_commission_stub, Environment, Scenario};
use platform_config::types::AddressEnvironment;
use soroban_sdk::{symbol_short, Bytes, BytesN};
use campaign_factory::{CampaignFactoryContract, CampaignFactoryContractClient};
use campaign_instance::{CampaignInstanceContract, CampaignInstanceContractClient};
use ecosystem_funding::{
    EcosystemFunding, EcosystemFundingClient, ProgramMilestoneStatus, ProgramType,
};
use shared::types::CampaignStatus;
use soroban_sdk::testutils::Events as _;
use soroban_sdk::{token, String, TryFromVal};

const AMOUNT: i128 = 10_000;
const FEE_BPS: u32 = 500;
const FEE: i128 = 500;

#[test]
fn full_escrow_lifecycle_with_registry_and_correlation() {
    let env = Environment::new();
    let admin = env.address();
    let platform_wallet = env.address();
    let client = env.address();
    let artist = env.address();

    env.mint(&client, 1_000_000);

    // ── Scenario 1: registry-backed dependency injection (#662) ────────────
    let mut scenario = Scenario::begin(&env, &admin, &platform_wallet, FEE_BPS);
    scenario.step("registry: register & resolve usdc for both namespaces");

    scenario.world().config.register_address(
        &AddressEnvironment::Production,
        &symbol_short!("usdc"),
        &env.usdc,
    );
    scenario.world().config.register_address(
        &AddressEnvironment::Test,
        &symbol_short!("usdc"),
        &env.usdc,
    );
    scenario.world().config.set_environment(&AddressEnvironment::Test);

    a::assert_address_resolves(&scenario.world().config, &symbol_short!("usdc"), &env.usdc);
    a::assert_event_emitted(
        &env,
        &scenario.world().config_addr,
        &[symbol_short!("addrreg"), symbol_short!("usdc")],
    );

    // ── Scenario 2: classic create → release with correlated events (#661) ─
    scenario.step("escrow: create + release");
    let mut steps = Steps::new(scenario.world());
    let cid = Bytes::from_slice(&env.env, b"comm-001");

    steps
        .create(&cid, &client, &artist, AMOUNT)
        .release(&cid);

    a::assert_balance(&env, &artist, AMOUNT - FEE);
    a::assert_balance(&env, &platform_wallet, FEE);
    a::assert_correlation_event_present(
        &env,
        symbol_short!("escrow"),
        symbol_short!("created"),
    );

    // ── Scenario 3: atomic escrow→commission migration (#656) ─────────────
    scenario.step("atomic: two-party commit then single-transaction migration");
    let (commission, commission_client) = deploy_commission_stub(&env, AMOUNT);

    let cid2 = Bytes::from_slice(&env.env, b"comm-002");
    let mut steps = Steps::new(scenario.world());
    steps
        .create(&cid2, &client, &artist, AMOUNT)
        .begin_atomic(&cid2)
        .confirm_atomic(&cid2, &client)
        .confirm_atomic(&cid2, &artist);

    let marker = steps.migrate_atomically(&cid2, &commission);
    assert_eq!(marker.participants, 2);

    a::assert_balance(&env, &commission, AMOUNT - FEE);
    a::assert_balance(&env, &platform_wallet, 2 * FEE);
    a::assert_escrow_status(&env, &scenario.world().escrow, &cid, CommissionStatus::Released);
    let _ = commission_client;

    // Regression guard: released escrows surface an atomic marker.
    let settled = scenario.world().escrow.get_atomic_commit(&cid2);
    assert_eq!(settled.state, escrow::AtomicCommitState::Settled);
}
#[test]
fn failed_multi_contract_operation_rolls_back_participating_state() {
    let env = Environment::new();
    let admin = env.address();
    let platform_wallet = env.address();
    let client = env.address();
    let artist = env.address();

    // Client has less than the requested escrow amount, so the token transfer
    // (and the overall create_escrow transaction) must fail and roll back.
    env.mint(&client, AMOUNT - 1);

    let scenario = Scenario::begin(&env, &admin, &platform_wallet, FEE_BPS);
    let world = scenario.finish();
    let cid = Bytes::from_slice(&env.env, b"rollback-001");

    let client_balance_before = env.balance(&client);
    let escrow_balance_before = env.balance(&world.escrow_addr);

    let result = world.escrow.try_create_escrow(
        &cid,
        &client,
        &artist,
        &AMOUNT,
        &world.config_stub_addr,
    );
    assert!(result.is_err(), "create_escrow must fail when client balance is insufficient");

    // Focused rollback assertions: escrow storage and participating token balances
    // must all return to their pre-operation state.
    a::assert_escrow_create_rolled_back(
        &env,
        &world.escrow,
        &world.escrow_addr,
        &cid,
        &client,
        client_balance_before,
        escrow_balance_before,
    );
    a::assert_balances_unchanged(
        &env,
        &[
            (&platform_wallet, 0),
            (&artist, 0),
        ],
    );
}

#[test]
fn factory_rejects_invalid_deployment_inputs_before_creating_child() {
    let env = Environment::new();
    let admin = env.address();
    let treasury = env.address();
    let creator = env.address();
    let salt = BytesN::from_array(&env.env, [7u8; 32]);

    let factory_id = env.env.register_contract(None, CampaignFactoryContract);
    let factory = CampaignFactoryContractClient::new(&env.env, &factory_id);

    // Not initialized -> rejected before child creation.
    let before = env.env.events().all().len();
    assert!(factory
        .try_deploy_campaign(&creator, &env.usdc, &salt)
        .is_err());
    assert!(factory.get_deployed_campaign(&salt).is_none());
    assert_eq!(env.env.events().all().len(), before, "failed input validation must not emit success event");

    factory.initialize(&admin, &treasury);

    // Initialized but unsupported/missing deployment version (no wasm hash configured)
    // -> rejected before child creation.
    let before = env.env.events().all().len();
    assert!(factory
        .try_deploy_campaign(&creator, &env.usdc, &salt)
        .is_err());
    assert!(factory.get_deployed_campaign(&salt).is_none());
    assert_eq!(env.env.events().all().len(), before, "rejected request must not emit success event");

    // A valid request is one with required deployment inputs configured and a
    // previously-unused salt; if deployment cannot proceed, rejection must still
    // leave no registered address or success event behind.
    let wasm_hash = BytesN::from_array(&env.env, [9u8; 32]);
    factory.update_wasm_hash(&admin, &wasm_hash);
    let before = env.env.events().all().len();
    let _ = factory.try_deploy_campaign(&creator, &env.usdc, &salt);
    assert!(
        factory.get_deployed_campaign(&salt).is_none(),
        "deployment failure must not register a child contract"
    );
    assert_eq!(env.env.events().all().len(), before, "deployment failure must not emit success event");
}

#[test]
fn factory_registration_is_atomic_on_failed_deployment() {
    let env = Environment::new();
    let admin = env.address();
    let treasury = env.address();
    let creator = env.address();
    let salt = BytesN::from_array(&env.env, [5u8; 32]);

    let factory_id = env.env.register_contract(None, CampaignFactoryContract);
    let factory = CampaignFactoryContractClient::new(&env.env, &factory_id);
    factory.initialize(&admin, &treasury);

    // Configure an unavailable/unsupported wasm hash so child deployment fails.
    let missing_wasm_hash = BytesN::from_array(&env.env, [42u8; 32]);
    factory.update_wasm_hash(&admin, &missing_wasm_hash);

    let events_before = env.env.events().all().len();
    let result = factory.try_deploy_campaign(&creator, &env.usdc, &salt);
    assert!(result.is_err(), "deployment with unavailable wasm must fail");

    // Registry state and success event must only be committed on successful deploy.
    assert!(
        factory.get_deployed_campaign(&salt).is_none(),
        "failed deployment must not leave a registered address"
    );
    assert_eq!(
        env.env.events().all().len(),
        events_before,
        "failed deployment must not emit a success event"
    );

    // A second attempt with the same validated inputs also fails without leaving
    // partial registry/event state behind.
    let result_retry = factory.try_deploy_campaign(&creator, &env.usdc, &salt);
    assert!(result_retry.is_err());
    assert!(factory.get_deployed_campaign(&salt).is_none());
    assert_eq!(env.env.events().all().len(), events_before);
}

#[test]
fn rejected_config_updates_preserve_existing_state() {
    let env = Environment::new();
    let admin = env.address();
    let platform_wallet = env.address();

    // Initialize config with a known-good fee.
    let scenario = Scenario::begin(&env, &admin, &platform_wallet, FEE_BPS);
    let cfg = scenario.finish();
    let before = cfg.config.get_config();
    assert_eq!(before.fee_bps, FEE_BPS);

    // Invalid fee update is rejected at the boundary and leaves config unchanged.
    let invalid = cfg.config.try_set_fee_bps(&1001);
    assert!(invalid.is_err(), "fee bps above the allowed range must be rejected");
    let after = cfg.config.get_config();
    assert_eq!(after.fee_bps, before.fee_bps);
    assert_eq!(after.admin, before.admin);
    assert_eq!(after.platform_wallet, before.platform_wallet);
    assert_eq!(after.usdc_token, before.usdc_token);

    // A valid update after rejection proves configuration remains usable.
    cfg.config.set_fee_bps(&250);
    assert_eq!(cfg.config.get_config().fee_bps, 250);
}

#[test]
fn campaign_instance_mutations_are_blocked_after_parent_closure() {
    let env = Environment::new();
    let admin = env.address();
    let parent = env.address();
    let donor = env.address();

    let contract_id = env.env.register_contract(None, CampaignInstanceContract);
    let instance = CampaignInstanceContractClient::new(&env.env, &contract_id);
    let end_time = env.env.ledger().timestamp() + 86_400;
    instance.initialize(
        &admin,
        &5_000_i128,
        &end_time,
        &soroban_sdk::vec![&env.env, env.usdc.clone()],
    );
    instance.set_parent_campaign(&admin, &parent);

    env.mint(&donor, 1_000);
    instance.donate(&donor, &env.usdc, &200);
    assert_eq!(instance.get_raised(), 200);

    instance.set_status(&parent, &CampaignStatus::Completed);
    assert_eq!(instance.get_status(), CampaignStatus::Completed);
    assert!(instance.try_donate(&donor, &env.usdc, &100).is_err());
    assert!(instance.try_payout(&admin, &env.usdc, &admin, &50).is_err());
    assert!(instance.try_refund(&admin, &env.usdc, &donor, &50).is_err());
    assert_eq!(instance.get_raised(), 200);
}

#[test]
fn campaign_instance_accounting_conserves_funds_end_to_end() {
    let env = Environment::new();
    let admin = env.address();
    let donor = env.address();
    let recipient = env.address();

    let contract_id = env.env.register_contract(None, CampaignInstanceContract);
    let instance = CampaignInstanceContractClient::new(&env.env, &contract_id);
    let end_time = env.env.ledger().timestamp() + 86_400;
    instance.initialize(
        &admin,
        &5_000_i128,
        &end_time,
        &soroban_sdk::vec![&env.env, env.usdc.clone()],
    );

    env.mint(&donor, 1_000);
    instance.donate(&donor, &env.usdc, &600);
    instance.donate(&donor, &env.usdc, &200);
    instance.refund(&admin, &env.usdc, &donor, &150);
    instance.payout(&admin, &env.usdc, &recipient, &400);

    let (raised, paid_out, refunded) = instance.get_accounting();
    assert_eq!((raised, paid_out, refunded), (800, 400, 150));
    assert_eq!(raised, paid_out + refunded + env.balance(&contract_id));
    assert_eq!(env.balance(&donor), 350);
    assert_eq!(env.balance(&recipient), 400);
}

#[test]
fn ecosystem_grant_milestones_require_ordered_approval() {
    let env = Environment::new();
    let manager = env.address();
    let recipient = env.address();

    let contract_id = env.env.register_contract(None, EcosystemFunding);
    let grants = EcosystemFundingClient::new(&env.env, &contract_id);
    grants.initialize(&env.address());

    let milestones = soroban_sdk::vec![
        &env.env,
        (
            String::from_str(&env.env, "m0"),
            String::from_str(&env.env, "d0"),
            100_i128
        ),
        (
            String::from_str(&env.env, "m1"),
            String::from_str(&env.env, "d1"),
            200_i128
        ),
    ];
    let program_id = grants.create_program(
        &manager,
        &recipient,
        &String::from_str(&env.env, "grant"),
        &String::from_str(&env.env, "description"),
        &ProgramType::Grant,
        &milestones,
        &env.usdc,
    );
    token::StellarAssetClient::new(&env.env, &env.usdc).mint(&contract_id, &300_i128);
    let uri = String::from_str(&env.env, "uri");

    grants.submit_milestone(&recipient, &program_id, &1, &uri);
    assert!(grants
        .try_review_milestone(&manager, &program_id, &1, &true)
        .is_err());

    grants.submit_milestone(&recipient, &program_id, &0, &uri);
    grants.review_milestone(&manager, &program_id, &0, &true);
    grants.review_milestone(&manager, &program_id, &1, &true);

    let milestones = grants.get_milestones(&program_id);
    assert!(milestones.get(0).unwrap().status == ProgramMilestoneStatus::Approved);
    assert!(milestones.get(1).unwrap().status == ProgramMilestoneStatus::Approved);
}
