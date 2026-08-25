use super::*;
use soroban_sdk::{
    contract, contractimpl,
    testutils::{storage::Persistent as _, Address as _, Events, Ledger, LedgerInfo},
    token::{Client as TokenClient, StellarAssetClient},
    BytesN, Env, String, Symbol, TryFromVal, Val,
};

use leaderboard::LeaderboardContract;
use pulse_token::PULSETokenContract;
use referral_registry::{DataKey as ReferralDataKey, ReferralRegistryContract};

// ── Test Infrastructure ───────────────────────────────────────────────────────

struct TestSetup {
    env: Env,
    client: PredictionMarketContractClient<'static>,
    admin: Address,
    xlm_admin: StellarAssetClient<'static>,
    xlm: TokenClient<'static>,
    xlm_sac_id: Address,
    token_client: pulse_token::PULSETokenContractClient<'static>,
    leaderboard_client: leaderboard::LeaderboardContractClient<'static>,
    referral_client: referral_registry::ReferralRegistryContractClient<'static>,
}

fn setup() -> TestSetup {
    let env = Env::default();
    env.mock_all_auths();
    env.cost_estimate().budget().reset_unlimited();

    env.ledger().set(LedgerInfo {
        timestamp: 1_000_000,
        protocol_version: 26,
        sequence_number: 100,
        network_id: Default::default(),
        base_reserve: 10,
        min_temp_entry_ttl: 100,
        min_persistent_entry_ttl: 100,
        max_entry_ttl: 10_000_000,
    });

    let admin = Address::generate(&env);

    let xlm_sac_id = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();
    let xlm_admin = StellarAssetClient::new(&env, &xlm_sac_id);
    let xlm = TokenClient::new(&env, &xlm_sac_id);

    let token_id = env.register(PULSETokenContract, ());
    let token_client = pulse_token::PULSETokenContractClient::new(&env, &token_id);
    token_client.initialize(
        &admin,
        &String::from_str(&env, "PULSE"),
        &String::from_str(&env, "PLSE"),
        &7u32,
    );

    let leaderboard_id = env.register(LeaderboardContract, ());
    let leaderboard_client = leaderboard::LeaderboardContractClient::new(&env, &leaderboard_id);

    let referral_id = env.register(ReferralRegistryContract, ());
    let referral_client =
        referral_registry::ReferralRegistryContractClient::new(&env, &referral_id);

    let market_id = env.register(PredictionMarketContract, ());
    let client = PredictionMarketContractClient::new(&env, &market_id);

    client.initialize(
        &admin,
        &token_id,
        &referral_id,
        &leaderboard_id,
        &xlm_sac_id,
    );
    leaderboard_client.initialize(&admin, &market_id, &referral_id);
    referral_client.initialize(&admin, &market_id, &token_id, &leaderboard_id, &xlm_sac_id);

    // Lever G: the leaderboard now mints PULSE internally (one cross-call from
    // market/referral instead of two). It must know the token AND be authorized
    // as a minter. This mirrors the exact mainnet upgrade sequence.
    leaderboard_client.set_token_contract(&admin, &token_id);
    token_client.set_minter(&leaderboard_id);
    // Legacy minter auths kept harmless (market/referral no longer mint directly).
    token_client.set_minter(&market_id);
    token_client.set_minter(&referral_id);

    TestSetup {
        env,
        client,
        admin,
        xlm_admin,
        xlm,
        xlm_sac_id,
        token_client,
        leaderboard_client,
        referral_client,
    }
}

fn fund_user(t: &TestSetup, user: &Address, amount: i128) {
    t.xlm_admin.mint(user, &amount);
}

fn create_test_market(t: &TestSetup) -> u64 {
    t.client.create_market(
        &t.admin,
        &String::from_str(&t.env, "Will BTC hit 100k?"),
        &String::from_str(&t.env, "https://example.com/btc.png"),
        &Category::Crypto,
        &3600_u64,
    )
}

fn advance_time(env: &Env, secs: u64) {
    let current = env.ledger().timestamp();
    env.ledger().set(LedgerInfo {
        timestamp: current + secs,
        protocol_version: 26,
        sequence_number: env.ledger().sequence() + 1,
        network_id: Default::default(),
        base_reserve: 10,
        min_temp_entry_ttl: 100,
        min_persistent_entry_ttl: 100,
        max_entry_ttl: 10_000_000,
    });
}

fn rewind_time(env: &Env, secs: u64) {
    let current = env.ledger().timestamp();
    env.ledger().set(LedgerInfo {
        timestamp: current - secs,
        protocol_version: 26,
        sequence_number: env.ledger().sequence() + 1,
        network_id: Default::default(),
        base_reserve: 10,
        min_temp_entry_ttl: 100,
        min_persistent_entry_ttl: 100,
        max_entry_ttl: 10_000_000,
    });
}

// ── 1. Initialize ─────────────────────────────────────────────────────────────

#[test]
fn test_initialize() {
    let t = setup();
    assert_eq!(t.client.get_market_count(), 0);
    assert_eq!(t.client.get_accumulated_fees(), 0);
}

// ── 2. Create market ─────────────────────────────────────────────────────────

#[test]
fn test_create_market() {
    let t = setup();
    let id = create_test_market(&t);
    assert_eq!(id, 1);
    assert_eq!(t.client.get_market_count(), 1);

    let market = t.client.get_market(&id);
    assert_eq!(market.total_yes, 0);
    assert_eq!(market.total_no, 0);
    assert!(!market.resolved);
    assert!(!market.cancelled);
    assert_eq!(market.bet_count, 0);
}

#[test]
#[should_panic(expected = "Error(Contract, #27)")]
fn test_reject_zero_market_duration() {
    let t = setup();
    t.client.create_market(
        &t.admin,
        &String::from_str(&t.env, "Zero duration"),
        &String::from_str(&t.env, "https://x.png"),
        &Category::Other,
        &0_u64,
    );
}

#[test]
#[should_panic(expected = "Error(Contract, #27)")]
fn test_reject_market_duration_below_minimum() {
    let t = setup();
    t.client.create_market(
        &t.admin,
        &String::from_str(&t.env, "Too short"),
        &String::from_str(&t.env, "https://x.png"),
        &Category::Other,
        &(MIN_MARKET_DURATION_SECS - 1),
    );
}

#[test]
fn test_market_duration_minimum_is_allowed() {
    let t = setup();
    let id = t.client.create_market(
        &t.admin,
        &String::from_str(&t.env, "Minimum duration"),
        &String::from_str(&t.env, "https://x.png"),
        &Category::Other,
        &MIN_MARKET_DURATION_SECS,
    );

    assert_eq!(
        t.client.get_market(&id).end_time,
        t.env.ledger().timestamp() + MIN_MARKET_DURATION_SECS
    );
}

// ── 3. Place YES bet ──────────────────────────────────────────────────────────

#[test]
fn test_place_yes_bet() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);

    t.client.place_bet(&user, &id, &true, &100_0000000_i128);

    let market = t.client.get_market(&id);
    assert_eq!(market.total_yes, 98_0000000);
    assert_eq!(market.total_no, 0);
    assert_eq!(market.bet_count, 1);

    let bet = t.client.get_bet(&id, &user);
    assert_eq!(bet.amount, 98_0000000);
    assert!(bet.is_yes);
    assert!(!bet.claimed);

    // Gross tracked correctly
    assert_eq!(t.client.get_bet_gross(&id, &user), 100_0000000);
}

// ── 4. Place NO bet ───────────────────────────────────────────────────────────

#[test]
fn test_place_no_bet() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);

    t.client.place_bet(&user, &id, &false, &100_0000000_i128);

    let market = t.client.get_market(&id);
    assert_eq!(market.total_yes, 0);
    assert_eq!(market.total_no, 98_0000000);
}

// ── 5. Fee: full 2% to AccumulatedFees when no referrer ──────────────────────

#[test]
fn test_fee_full_2_percent_no_referrer() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);

    t.client.place_bet(&user, &id, &true, &100_0000000_i128);
    // Issue #78: only platform_fee is tracked in AccumulatedFees;
    // referral fee stays in referral contract as surplus.
    assert_eq!(t.client.get_accumulated_fees(), 1_5000000);
}

// ── 6. Fee split with referrer ────────────────────────────────────────────────

#[test]
fn test_fee_split_with_referrer() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    let referrer = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);

    // Issue #99: the referrer must be a registered participant first.
    t.referral_client
        .register_referral(&referrer, &String::from_str(&t.env, "RefFirst"), &None);
    let no_ref: Option<Address> = None;
    t.referral_client.register_referral(
        &referrer,
        &String::from_str(&t.env, "Referrer"),
        &no_ref,
    );
    t.referral_client.register_referral(
        &user,
        &String::from_str(&t.env, "Bettor"),
        &Some(referrer.clone()),
    );

    t.client.place_bet(&user, &id, &true, &100_0000000_i128);

    assert_eq!(t.client.get_accumulated_fees(), 1_5000000);
    assert_eq!(t.xlm.balance(&referrer), 5000000);
    // 5 welcome-bonus points (referrer registered) + 3 referral-bet points.
    assert_eq!(t.leaderboard_client.get_points(&referrer), 8);
}

// ── 6b. Issue #99: bet with an UNREGISTERED referrer link is rejected ────────

#[test]
#[should_panic(expected = "Error(Contract, #7)")]
fn test_reject_place_bet_with_unregistered_referrer() {
    // A user cannot even register a referral link to an unregistered address,
    // so an unregistered attacker-controlled address can never receive fees.
    let t = setup();
    let user = Address::generate(&t.env);
    let shady = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);

    t.referral_client.register_referral(
        &user,
        &String::from_str(&t.env, "Victim"),
        &Some(shady.clone()),
    );
    t.leaderboard_client.claim_pending_rewards(&referrer);
    assert_eq!(t.leaderboard_client.get_points(&referrer), 8);
}

// ── 7. Reject bet on expired market ──────────────────────────────────────────

#[test]
#[should_panic(expected = "Error(Contract, #5)")]
fn test_reject_bet_expired_market() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);
    advance_time(&t.env, 3601);
    t.client.place_bet(&user, &id, &true, &100_0000000_i128);
}

// ── 8. Reject bet on resolved market ─────────────────────────────────────────

#[test]
#[should_panic(expected = "Error(Contract, #7)")]
fn test_reject_bet_resolved_market() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);
    t.client.place_bet(&user, &id, &true, &50_0000000_i128);
    advance_time(&t.env, 3601);
    t.client.resolve_market(&t.admin, &id, &true);

    let user2 = Address::generate(&t.env);
    fund_user(&t, &user2, 200_0000000);
    t.client.place_bet(&user2, &id, &false, &50_0000000_i128);
}

// ── 9. Reject bet on cancelled market ────────────────────────────────────────

#[test]
#[should_panic(expected = "Error(Contract, #8)")]
fn test_reject_bet_cancelled_market() {
    let t = setup();
    let id = create_test_market(&t);
    t.client.cancel_market(&t.admin, &id);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);
    t.client.place_bet(&user, &id, &true, &100_0000000_i128);
}

// ── 10. Reject bet below minimum ─────────────────────────────────────────────

#[test]
#[should_panic(expected = "Error(Contract, #10)")]
fn test_reject_bet_below_minimum() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);
    t.client.place_bet(&user, &id, &true, &5_000_000_i128);
}

#[test]
#[should_panic(expected = "Error(Contract, #10)")]
fn test_reject_gross_minimum_when_net_is_too_small() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);

    // The gross minimum is not enough to produce the one-XLM net minimum
    // after the two-percent fee.
    t.client.place_bet(&user, &id, &true, &MIN_BET);
}

// ── 11. Increase existing position ───────────────────────────────────────────

#[test]
fn test_increase_position_same_side() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 500_0000000);

    t.client.place_bet(&user, &id, &true, &100_0000000_i128);
    assert_eq!(t.client.get_bet(&id, &user).amount, 98_0000000);

    t.client.place_bet(&user, &id, &true, &50_0000000_i128);
    assert_eq!(t.client.get_bet(&id, &user).amount, 98_0000000 + 49_0000000);

    // Gross tracks full input (both bets)
    assert_eq!(t.client.get_bet_gross(&id, &user), 150_0000000);

    let market = t.client.get_market(&id);
    assert_eq!(market.total_yes, 98_0000000 + 49_0000000);
    assert_eq!(market.bet_count, 1);
}

// ═══════════════════════════════════════════════════════════════════════════
// SECURITY REGRESSION SUITE — issue #98 (position management / reduce_position)
// ═══════════════════════════════════════════════════════════════════════════

// ── 98a. Partial reduction, no referrer: full 100% of the released stake is
//        refundable (net + platform fee + referral fee all held on contract) ──
#[test]
fn test_reduce_position_partial_no_referrer() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 10_000_0000000);

    t.client.place_bet(&user, &id, &true, &100_0000000_i128); // 100 XLM
    assert_eq!(t.client.get_market(&id).total_yes, 98_0000000);
    assert_eq!(t.client.get_accumulated_fees(), 2_0000000); // 1.5 + 0.5

    // Reduce 40 XLM of the 100 XLM position.
    let refund = t.client.reduce_position(&user, &id, &40_0000000_i128);
    // net(39.2) + platform(0.6) + referral(0.2) == 40.0 held by the contract
    assert_eq!(refund, 40_0000000);
    assert_eq!(t.client.get_bet_gross(&id, &user), 60_0000000);
    assert_eq!(t.client.get_bet(&id, &user).amount, 58_8000000); // 98 - 39.2
    assert_eq!(t.client.get_market(&id).total_yes, 58_8000000);
    assert_eq!(t.client.get_accumulated_fees(), 1_2000000); // 2.0 - (0.6+0.2)
    assert_eq!(t.client.get_user_bet_count(&id, &user), 1); // not a new bet
}

// ── 98b. Partial reduction with a referrer — the referral fee was already paid
//     out, so only net + platform fee are refundable (99.5% of the amount) ──
#[test]
fn test_reduce_position_with_referrer_paid() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    let referrer = Address::generate(&t.env);
    fund_user(&t, &user, 1_000_0000000);

    t.referral_client.register_referral(
        &user,
        &String::from_str(&t.env, "Bettor"),
        &Some(referrer.clone()),
    );
    t.client.place_bet(&user, &id, &true, &100_0000000_i128);
    assert_eq!(t.xlm.balance(&referrer), 5000000); // referral fee paid out

    let refund = t.client.reduce_position(&user, &id, &40_0000000_i128);
    // 39.2 net + 0.6 platform (referral 0.2 not clawed back from the referrer)
    assert_eq!(refund, 39_8000000);
    assert_eq!(t.xlm.balance(&referrer), 5000000); // referrer keeps the fee
    assert_eq!(t.client.get_accumulated_fees(), 9_000000); // 1.5 - 0.6
    assert_eq!(t.client.get_market(&id).total_yes, 58_8000000);
}

// ── 98c. Full close deletes the position: no entry, no claim, no free PULSE ──
#[test]
fn test_reduce_position_full_close_deletes_position() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 1_000_0000000);

    t.client.place_bet(&user, &id, &true, &100_0000000_i128);
    let refund = t.client.reduce_position(&user, &id, &100_0000000_i128);
    assert_eq!(refund, 100_0000000); // full gross back (no referrer)
    assert_eq!(t.client.get_bet_gross(&id, &user), 0);
    assert!(t.client.try_get_bet(&id, &user).is_err()); // NoBetFound
    assert_eq!(t.client.get_market(&id).total_yes, 0);
    assert_eq!(t.client.get_accumulated_fees(), 0);

    // Claiming the closed position must fail (no double payout, no rewards).
    let closed_claim = t.client.try_claim(&user, &id);
    assert!(closed_claim.is_err());
}

// ── 98d. Resolution stays exact after reductions — the released net is fully
//     removed from the pool, so winners get the entire remaining pool and
//     Σ payouts + dust == pool still holds ─────────────────────────────────
#[test]
fn test_reduce_position_keeps_resolution_exact() {
    let t = setup();
    let id = create_test_market(&t);
    let alice = Address::generate(&t.env);
    let bob = Address::generate(&t.env);
    fund_user(&t, &alice, 10_000_0000000);
    fund_user(&t, &bob, 10_000_0000000);

    t.client.place_bet(&alice, &id, &true, &100_0000000_i128); // net 98
    t.client.place_bet(&bob, &id, &false, &100_0000000_i128); // net 98
    t.client.reduce_position(&alice, &id, &40_0000000_i128); // net -39.2

    // Pools: yes 58.8, no 98, total 156.8
    let market = t.client.get_market(&id);
    assert_eq!(market.total_yes, 58_8000000);
    assert_eq!(market.total_no, 98_0000000);

    advance_time(&t.env, 3601);
    t.client.resolve_market(&t.admin, &id, &true);
    let alice_before = t.xlm.balance(&alice);
    t.client.claim(&alice, &id);
    // Single winner: payout == entry.net * total_pool / winning_side == the
    // entire remaining pool (156.8), so no dust and no dilution of Bob's stake.
    assert_eq!(t.xlm.balance(&alice) - alice_before, 156_8000000);

    let bob_before = t.xlm.balance(&bob);
    t.client.claim(&bob, &id);
    assert_eq!(t.xlm.balance(&bob), bob_before); // losing side gets nothing
}

// ── 98e. Partial reduction keeps the SAME-side position open and allows
//     rebuilding; the one-side-per-user invariant is preserved ─────────────
#[test]
fn test_reduce_then_rebuild_same_side_opposite_still_rejected() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 10_000_0000000);

    t.client.place_bet(&user, &id, &true, &100_0000000_i128);
    t.client.reduce_position(&user, &id, &40_0000000_i128);
    // Same side still open to increases.
    t.client.place_bet(&user, &id, &true, &50_0000000_i128);
    assert_eq!(t.client.get_bet_gross(&id, &user), 110_0000000); // 100-40+50
    assert_eq!(t.client.get_user_bet_count(&id, &user), 2); // two real bets

    // Opposite side remains rejected — the payout model is one-side-per-user.
    let opp = t.client.try_place_bet(&user, &id, &false, &10_0000000_i128);
    assert!(opp.is_err());
}

// ── 98e2. Cancellation after reduction: cancel_refund pays the REMAINING
//     gross — the reduced portion is never double-refunded ─────────────────
#[test]
fn test_reduce_then_cancel_refund_pays_remaining_gross() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 10_000_0000000);

    t.client.place_bet(&user, &id, &true, &100_0000000_i128);
    let reduced = t.client.reduce_position(&user, &id, &40_0000000_i128);
    assert_eq!(reduced, 40_0000000);
    assert_eq!(t.client.get_bet_gross(&id, &user), 60_0000000);

    // Market is then cancelled: refund covers only the 60 XLM still held.
    t.client.cancel_market(&t.admin, &id);
    let refunded = t.client.cancel_refund(&user, &id);
    assert_eq!(refunded, 60_0000000);
    assert_eq!(t.client.get_bet_gross(&id, &user), 0);

    // Idempotent: a second refund attempt finds nothing left.
    let again = t.client.try_cancel_refund(&user, &id);
    assert!(again.is_err());
}

// ── 98f. Rejections: amount > position, zero/negative, no bet, and
//     resolved / cancelled / expired markets ────────────────────────────────
#[test]
#[should_panic(expected = "Error(Contract, #14)")]
fn test_reduce_position_rejects_over_amount() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 1_000_0000000);
    t.client.place_bet(&user, &id, &true, &100_0000000_i128);
    t.client.reduce_position(&user, &id, &100_0000001_i128);
}

#[test]
#[should_panic(expected = "Error(Contract, #14)")]
fn test_reduce_position_rejects_zero_amount() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 1_000_0000000);
    t.client.place_bet(&user, &id, &true, &100_0000000_i128);
    t.client.reduce_position(&user, &id, &0_i128);
}

#[test]
#[should_panic(expected = "Error(Contract, #13)")]
fn test_reduce_position_rejects_non_bettor() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    t.client.reduce_position(&user, &id, &10_0000000_i128);
}

#[test]
#[should_panic(expected = "Error(Contract, #7)")]
fn test_reduce_position_rejects_resolved_market() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    let other = Address::generate(&t.env);
    fund_user(&t, &user, 1_000_0000000);
    fund_user(&t, &other, 1_000_0000000);
    t.client.place_bet(&user, &id, &true, &100_0000000_i128);
    t.client.place_bet(&other, &id, &false, &100_0000000_i128);
    advance_time(&t.env, 3601);
    t.client.resolve_market(&t.admin, &id, &true);
    t.client.reduce_position(&user, &id, &10_0000000_i128);
}

#[test]
#[should_panic(expected = "Error(Contract, #8)")]
fn test_reduce_position_rejects_cancelled_market() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 1_000_0000000);
    t.client.place_bet(&user, &id, &true, &100_0000000_i128);
    t.client.cancel_market(&t.admin, &id);
    t.client.reduce_position(&user, &id, &10_0000000_i128);
}

#[test]
#[should_panic(expected = "Error(Contract, #5)")]
fn test_reduce_position_rejects_expired_market() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 1_000_0000000);
    t.client.place_bet(&user, &id, &true, &100_0000000_i128);
    advance_time(&t.env, 3601);
    t.client.reduce_position(&user, &id, &10_0000000_i128);
}

// ── 12. Reject opposite-side bet ─────────────────────────────────────────────
// ── 12. Hedge: user can bet both sides of a market ───────────────────────────

#[test]
fn test_hedge_both_sides() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 500_0000000);

    t.client.place_bet(&user, &id, &true, &100_0000000_i128);
    t.client.place_bet(&user, &id, &false, &50_0000000_i128);

    // Both sides are tracked independently on the same single entry
    let market = t.client.get_market(&id);
    assert_eq!(market.total_yes, 98_0000000);
    assert_eq!(market.total_no, 49_0000000);
    assert_eq!(market.bet_count, 1);

    let pos = t.client.get_position(&id, &user);
    assert_eq!(pos.net_yes, 98_0000000);
    assert_eq!(pos.net_no, 49_0000000);
    assert_eq!(pos.gross, 150_0000000);
    assert_eq!(pos.count, 2);
    assert!(!pos.claimed);

    // ABI view reports the dominant side
    let bet = t.client.get_bet(&id, &user);
    assert_eq!(bet.amount, 98_0000000);
    assert!(bet.is_yes);
    assert!(!bet.claimed);
}

// ── 26. Admin withdraw fees (earned only — markets must be settled) ────────────
// ISSUE #4: while a market is open its fee share is reserved for a possible
// cancellation refund, so withdrawals only succeed on SETTLED markets.

#[test]
#[should_panic(expected = "Error(Contract, #15)")]
fn test_withdraw_fees_open_market_rejected() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);
    t.client.place_bet(&user, &id, &true, &100_0000000_i128);
    // Market still open — its fees back a potential refund, so no withdrawal.
    t.client.withdraw_fees(&t.admin, &t.admin);
}

#[test]
fn test_withdraw_fees_after_resolution() {
// ── 12b. Rebalance: bets on either side accumulate independently ─────────────

#[test]
fn test_rebalance_accumulates_both_sides() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 1000_0000000);

    t.client.place_bet(&user, &id, &true, &100_0000000_i128);
    t.client.place_bet(&user, &id, &false, &50_0000000_i128);
    t.client.place_bet(&user, &id, &true, &25_0000000_i128);
    t.client.place_bet(&user, &id, &false, &75_0000000_i128);

    let pos = t.client.get_position(&id, &user);
    assert_eq!(pos.net_yes, 98_0000000 + 24_5000000); // 122.5 XLM net
    assert_eq!(pos.net_no, 49_0000000 + 73_5000000); // 122.5 XLM net
    assert_eq!(pos.gross, 250_0000000);
    assert_eq!(pos.count, 4);

    // Settle the market, then the earned fees are withdrawable.
    advance_time(&t.env, 3601);
    t.client.resolve_market(&t.admin, &id, &true);

    let admin_xlm_before = t.xlm.balance(&t.admin);
    let withdrawn = t.client.withdraw_fees(&t.admin, &t.admin);
    assert_eq!(withdrawn, fees_before);
    assert_eq!(t.client.get_accumulated_fees(), 0);
    assert_eq!(t.xlm.balance(&t.admin), admin_xlm_before + fees_before);
    let market = t.client.get_market(&id);
    assert_eq!(market.total_yes, 98_0000000 + 24_5000000);
    assert_eq!(market.total_no, 49_0000000 + 73_5000000);
}

// ── 12b2. Spam guard counts both sides of a position ─────────────────────────

#[test]
#[should_panic(expected = "Error(Contract, #17)")]
fn test_reject_too_many_bets_across_sides() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);
    t.client.place_bet(&user, &id, &true, &100_0000000_i128);
    advance_time(&t.env, 3601);
    t.client.resolve_market(&t.admin, &id, &true);

    let recipient = Address::generate(&t.env);
    let treasury = Address::generate(&t.env);
    t.client.add_fee_recipient(&t.admin, &recipient);

    let fees = t.client.get_accumulated_fees();
    let treasury_before = t.xlm.balance(&treasury);
    t.client.withdraw_fees(&recipient, &treasury);
    assert_eq!(t.xlm.balance(&treasury), treasury_before + fees);
    assert_eq!(t.client.get_accumulated_fees(), 0);
    fund_user(&t, &user, 100_000_000_000);
    for i in 0..=20u32 {
        t.client.place_bet(&user, &id, &(i % 2 == 0), &1_0000000_i128);
    }
}

// ── 12c. Full hedge: equal stakes on both sides are outcome-neutral ──────────

#[test]
fn test_full_hedge_is_outcome_neutral() {
    for outcome in [true, false] {
        let t = setup();
        let id = create_test_market(&t);
        let user = Address::generate(&t.env);
        fund_user(&t, &user, 500_0000000);

        t.client.place_bet(&user, &id, &true, &100_0000000_i128);
        t.client.place_bet(&user, &id, &false, &100_0000000_i128);
        advance_time(&t.env, 3601);
        t.client.resolve_market(&t.admin, &id, &outcome);

        let before = t.xlm.balance(&user);
        t.client.claim(&user, &id);
        // payout = 98 * 196 / 98 = 196 — the whole pool back, losing only the 2% fee
        assert_eq!(t.xlm.balance(&user) - before, 196_0000000);
    }
}

// ── 12d. Two-sided payout math stays conserved for other winners ─────────────

#[test]
fn test_hedged_payout_conserves_pool() {
    let t = setup();
    let id = create_test_market(&t);
    let alice = Address::generate(&t.env);
    let bob = Address::generate(&t.env);
    fund_user(&t, &alice, 1000_0000000);
    fund_user(&t, &bob, 1000_0000000);

    // Alice hedges: YES 100 (net 98) + NO 50 (net 49). Bob bets NO 100 (net 98).
    t.client.place_bet(&alice, &id, &true, &100_0000000_i128);
    t.client.place_bet(&alice, &id, &false, &50_0000000_i128);
    t.client.place_bet(&bob, &id, &false, &100_0000000_i128);

    let market = t.client.get_market(&id);
    assert_eq!(market.total_yes, 98_0000000);
    assert_eq!(market.total_no, 147_0000000);

    advance_time(&t.env, 3601);
    t.client.resolve_market(&t.admin, &id, &true);

    // Alice wins on her YES side only — payout uses net_yes, never the losing NO net.
    let alice_before = t.xlm.balance(&alice);
    t.client.claim(&alice, &id);
    let alice_payout = t.xlm.balance(&alice) - alice_before;
    assert_eq!(alice_payout, 245_0000000); // 98 * 245 / 98

    // Bob loses: his NO net is absorbed by the pool and paid to winners.
    let bob_before = t.xlm.balance(&bob);
    t.client.claim(&bob, &id);
    assert_eq!(t.xlm.balance(&bob), bob_before);

    // Platform keeps exactly the 2% fees — pool is fully distributed to winners.
    assert_eq!(t.client.get_accumulated_fees(), 5_0000000); // 2% of 250 gross
}

// ── 12e. Cancel refund covers both sides (gross total) ───────────────────────

#[test]
fn test_cancel_refund_two_sided_position() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 500_0000000);

    let before = t.xlm.balance(&user);
    t.client.place_bet(&user, &id, &true, &100_0000000_i128);
    t.client.place_bet(&user, &id, &false, &50_0000000_i128);

    t.client.cancel_market(&t.admin, &id);
    let refunded = t.client.cancel_refund(&user, &id);
    assert_eq!(refunded, 150_0000000); // full gross across both sides
    assert_eq!(t.xlm.balance(&user), before);
}

// ── 12f. get_position for a user with no bet ─────────────────────────────────

#[test]
#[should_panic(expected = "Error(Contract, #13)")]
fn test_get_position_no_bet_found() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    t.client.get_position(&id, &user);
}

// ── 13. Resolve market ───────────────────────────────────────────────────────

#[test]
fn test_resolve_market() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);
    t.client.place_bet(&user, &id, &true, &50_0000000_i128);
    advance_time(&t.env, 3601);
    t.client.resolve_market(&t.admin, &id, &true);
    let market = t.client.get_market(&id);
    assert!(market.resolved);
    assert!(market.outcome);
}

// ── 14. Resolver (non-admin) can resolve ─────────────────────────────────────

#[test]
fn test_resolver_can_resolve() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);
    t.client.place_bet(&user, &id, &true, &50_0000000_i128);

    let resolver = Address::generate(&t.env);
    t.client.add_resolver(&t.admin, &resolver);
    assert!(t.client.is_resolver(&resolver));

    advance_time(&t.env, 3601);
    t.client.resolve_market(&resolver, &id, &true);

    let market = t.client.get_market(&id);
    assert!(market.resolved);
}

// ── 15. Non-resolver cannot resolve ──────────────────────────────────────────

#[test]
#[should_panic(expected = "Error(Contract, #16)")]
fn test_reject_resolve_market_non_resolver() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);
    t.client.place_bet(&user, &id, &true, &50_0000000_i128);
    advance_time(&t.env, 3601);
    let rando = Address::generate(&t.env);
    t.client.resolve_market(&rando, &id, &true);
}

// ── 16. Reject double resolution ─────────────────────────────────────────────

#[test]
#[should_panic(expected = "Error(Contract, #7)")]
fn test_reject_double_resolution() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);
    t.client.place_bet(&user, &id, &true, &50_0000000_i128);
    advance_time(&t.env, 3601);
    t.client.resolve_market(&t.admin, &id, &true);
    t.client.resolve_market(&t.admin, &id, &false);
}

// ── 17. Claim-style cancel: admin marks cancelled, bettors pull refunds ───────

#[test]
fn test_cancel_market_claim_style_refund() {
    let t = setup();
    let id = create_test_market(&t);
    let alice = Address::generate(&t.env);
    let bob = Address::generate(&t.env);
    fund_user(&t, &alice, 200_0000000);
    fund_user(&t, &bob, 200_0000000);

    let alice_before = t.xlm.balance(&alice);
    let bob_before = t.xlm.balance(&bob);

    t.client.place_bet(&alice, &id, &true, &100_0000000_i128);
    t.client.place_bet(&bob, &id, &false, &50_0000000_i128);

    // Admin cancels — O(1) gas, no transfers here
    t.client.cancel_market(&t.admin, &id);
    assert!(t.client.get_market(&id).cancelled);

    // Fees should be zeroed from AccumulatedFees since market is cancelled
    // (fees are returned to bettors via cancel_refund)
    let acc_fees_after_cancel = t.client.get_accumulated_fees();
    assert_eq!(acc_fees_after_cancel, 0);

    // Each bettor pulls their own gross refund
    let alice_refund = t.client.cancel_refund(&alice, &id);
    assert_eq!(alice_refund, 100_0000000); // full gross (100 XLM)
    assert_eq!(t.xlm.balance(&alice), alice_before);
    assert_eq!(t.client.get_bet(&id, &alice).amount, 0);
    assert_eq!(t.client.get_bet_gross(&id, &alice), 0);

    let bob_refund = t.client.cancel_refund(&bob, &id);
    assert_eq!(bob_refund, 50_0000000); // full gross (50 XLM)
    assert_eq!(t.xlm.balance(&bob), bob_before);
    assert_eq!(t.client.get_bet(&id, &bob).amount, 0);
    assert_eq!(t.client.get_bet_gross(&id, &bob), 0);
}

// ── 18. Cancel refund is idempotent — double refund rejected ──────────────────

#[test]
#[should_panic(expected = "Error(Contract, #13)")]
fn test_cancel_refund_double_claim_rejected() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);
    t.client.place_bet(&user, &id, &true, &100_0000000_i128);
    t.client.cancel_market(&t.admin, &id);
    t.client.cancel_refund(&user, &id);
    t.client.cancel_refund(&user, &id); // should fail: NoBetFound (gross zeroed)
}

// ── Issue #58: cancel_refund zeroes net and decrements market totals ─────────

#[test]
fn test_cancel_refund_clears_net_and_market_totals() {
    let t = setup();
    let id = create_test_market(&t);
    let alice = Address::generate(&t.env);
    let bob = Address::generate(&t.env);
    fund_user(&t, &alice, 200_0000000);
    fund_user(&t, &bob, 200_0000000);

    // Alice bets YES 100 XLM → net = 100 * 9800/10000 = 98 XLM
    // Bob   bets NO  50 XLM  → net = 50  * 9800/10000 = 49 XLM
    t.client.place_bet(&alice, &id, &true, &100_0000000_i128);
    t.client.place_bet(&bob, &id, &false, &50_0000000_i128);

    let market_before = t.client.get_market(&id);
    assert_eq!(market_before.total_yes, 98_0000000);
    assert_eq!(market_before.total_no, 49_0000000);

    // Confirm get_bet reports staked net before cancel
    let alice_bet_before = t.client.get_bet(&id, &alice);
    assert_eq!(alice_bet_before.amount, 98_0000000);

    // Cancel the market then refund both bettors
    t.client.cancel_market(&t.admin, &id);
    t.client.cancel_refund(&alice, &id);
    t.client.cancel_refund(&bob, &id);

    // Issue #58 fix: get_bet must return amount == 0 after refund
    let alice_bet_after = t.client.get_bet(&id, &alice);
    assert_eq!(alice_bet_after.amount, 0, "net should be zeroed after cancel_refund");

    let bob_bet_after = t.client.get_bet(&id, &bob);
    assert_eq!(bob_bet_after.amount, 0, "net should be zeroed after cancel_refund");

    // Issue #58 fix: market totals must be decremented to 0 after all refunds
    let market_after = t.client.get_market(&id);
    assert_eq!(market_after.total_yes, 0, "total_yes should be 0 after alice's refund");
    assert_eq!(market_after.total_no, 0, "total_no should be 0 after bob's refund");

    // Gross is still zeroed (idempotency guard unchanged)
    assert_eq!(t.client.get_bet_gross(&id, &alice), 0);
    assert_eq!(t.client.get_bet_gross(&id, &bob), 0);
}

// ── 19. cancel_refund on non-cancelled market rejected ────────────────────────

#[test]
#[should_panic(expected = "Error(Contract, #19)")]
fn test_cancel_refund_non_cancelled_rejected() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);
    t.client.place_bet(&user, &id, &true, &100_0000000_i128);
    // Market NOT cancelled — should return MarketNotCancelled
    t.client.cancel_refund(&user, &id);
}

// ── 20. Reject cancel on resolved market ─────────────────────────────────────

#[test]
#[should_panic(expected = "Error(Contract, #7)")]
fn test_reject_cancel_resolved_market() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);
    t.client.place_bet(&user, &id, &true, &50_0000000_i128);
    advance_time(&t.env, 3601);
    t.client.resolve_market(&t.admin, &id, &true);
    t.client.cancel_market(&t.admin, &id);
}

// ── 21. Claim as winner ───────────────────────────────────────────────────────

#[test]
fn test_claim_winner() {
    let t = setup();
    let id = create_test_market(&t);
    let alice = Address::generate(&t.env);
    let bob = Address::generate(&t.env);
    fund_user(&t, &alice, 200_0000000);
    fund_user(&t, &bob, 200_0000000);

    t.client.place_bet(&alice, &id, &true, &100_0000000_i128);
    t.client.place_bet(&bob, &id, &false, &100_0000000_i128);

    let alice_pre_claim = t.xlm.balance(&alice);
    advance_time(&t.env, 3601);
    t.client.resolve_market(&t.admin, &id, &true);
    t.client.claim(&alice, &id);
    t.leaderboard_client.claim_pending_rewards(&alice);

    let payout = t.xlm.balance(&alice) - alice_pre_claim;
    assert_eq!(payout, 196_0000000);

    let stats = t.leaderboard_client.get_stats(&alice);
    assert_eq!(stats.won_bets, 1);
    assert_eq!(t.token_client.balance(&alice), 10_0000000);
}

// ── 22. Claim as loser ───────────────────────────────────────────────────────

#[test]
fn test_claim_loser() {
    let t = setup();
    let id = create_test_market(&t);
    let alice = Address::generate(&t.env);
    let bob = Address::generate(&t.env);
    fund_user(&t, &alice, 200_0000000);
    fund_user(&t, &bob, 200_0000000);

    // Simulate a large legacy index without spending time creating 101 bets.
    // (Note: the legacy full-page ABI reads up to MAX_BETTORS_PER_PAGE index
    // entries, which exceeds the 100-ledger-entry cap of the mock env, so the
    // bounded-read guarantee is exercised through the paginated ABI with small
    // pages — the same code path the legacy read delegates to.)
    t.env.as_contract(&t.client.address, || {
        t.env.storage().persistent().set(
            &DataKey::BettorCount(id),
            &(MAX_BETTORS_PER_PAGE + 1),
        );
        t.env.storage().persistent().set(
            &DataKey::BettorAt(id, 0),
            &first,
        );
        t.env.storage().persistent().set(
            &DataKey::BettorAt(id, MAX_BETTORS_PER_PAGE),
            &beyond_first_page,
        );
    });

    let legacy_page = t.client.get_market_bettors_page(&id, &0, &3);
    assert_eq!(legacy_page.len(), 1);
    assert_eq!(legacy_page.get(0).unwrap(), first);
    t.client.place_bet(&alice, &id, &true, &100_0000000_i128);
    t.client.place_bet(&bob, &id, &false, &100_0000000_i128);

    let bob_pre_claim = t.xlm.balance(&bob);
    advance_time(&t.env, 3601);
    t.client.resolve_market(&t.admin, &id, &true);
    t.client.claim(&bob, &id);
    t.leaderboard_client.claim_pending_rewards(&bob);

    assert_eq!(t.xlm.balance(&bob), bob_pre_claim);
    let stats = t.leaderboard_client.get_stats(&bob);
    assert_eq!(stats.lost_bets, 1);
    assert_eq!(t.token_client.balance(&bob), 2_0000000);
}

// ── 23. Reject double claim ───────────────────────────────────────────────────

#[test]
#[should_panic(expected = "Error(Contract, #12)")]
fn test_reject_double_claim() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);
    t.client.place_bet(&user, &id, &true, &100_0000000_i128);
    advance_time(&t.env, 3601);
    t.client.resolve_market(&t.admin, &id, &true);
    t.client.claim(&user, &id);
    t.client.claim(&user, &id);
}

// ── 24. Reject claim on unresolved market ────────────────────────────────────

#[test]
#[should_panic(expected = "Error(Contract, #9)")]
fn test_reject_claim_unresolved() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 100_000_000_000);

    // 20 XLM per bet so the NET stake (98%) clears MIN_BET; the 21st bet
    // must trip MAX_BETS_PER_USER instead of BetTooSmall.
    for _ in 0..=20u32 {
        t.client.place_bet(&user, &id, &true, &20_0000000_i128);
    }
    fund_user(&t, &user, 200_0000000);
    t.client.place_bet(&user, &id, &true, &100_0000000_i128);
    t.client.claim(&user, &id);
}

// ── 25. Reject claim on cancelled market ─────────────────────────────────────

#[test]
#[should_panic(expected = "Error(Contract, #8)")]
fn test_reject_claim_cancelled() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);
    t.client.place_bet(&user, &id, &true, &100_0000000_i128);
    t.client.cancel_market(&t.admin, &id);
    t.client.claim(&user, &id);
}

// ── 26. Admin withdraw fees ──────────────────────────────────────────────────

#[test]
fn test_withdraw_fees() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);
    t.client.place_bet(&user, &id, &true, &100_0000000_i128);

    let fees_before = t.client.get_accumulated_fees();
    assert!(fees_before > 0);

    let admin_xlm_before = t.xlm.balance(&t.admin);
    let withdrawn = t.client.withdraw_fees(&t.admin, &t.admin);
    assert_eq!(withdrawn, fees_before);
    assert_eq!(t.client.get_accumulated_fees(), 0);
    assert_eq!(t.xlm.balance(&t.admin), admin_xlm_before + fees_before);
}

// ── 27. Fee recipient withdrawal is capped + timelocked (issue #12) ──────────

#[test]
fn test_fee_recipient_withdraw() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);
    t.client.place_bet(&user, &id, &true, &100_0000000_i128);

    let recipient = Address::generate(&t.env);
    let treasury = Address::generate(&t.env);
    t.client.add_fee_recipient(&t.admin, &recipient);
    t.client.add_fee_recipient(&t.admin, &treasury);

    let fees = t.client.get_accumulated_fees();
    let cap = fees * MAX_WITHDRAWAL_BPS / BPS_DENOM;
    let treasury_before = t.xlm.balance(&treasury);

    // Fee recipient requests a capped withdrawal to the registered treasury.
    t.client.request_withdraw_fees(&recipient, &treasury, &cap);

    // Payout is NOT immediate: timelocked for WITHDRAW_DELAY_SECS.
    let pending = t.client.get_pending_withdrawal(&recipient).unwrap();
    assert_eq!(pending.recipient, treasury);
    assert_eq!(pending.amount, cap);
    assert_eq!(t.xlm.balance(&treasury), treasury_before);

    // After the delay the payout executes.
    advance_time(&t.env, WITHDRAW_DELAY_SECS);
    let withdrawn = t.client.execute_withdraw_fees(&recipient);
    assert_eq!(withdrawn, cap);
    assert_eq!(t.xlm.balance(&treasury), treasury_before + cap);
    assert_eq!(t.client.get_accumulated_fees(), fees - cap);
}

// ── 27b. Fee recipient can no longer withdraw immediately (issue #12) ─────────

#[test]
#[should_panic(expected = "Error(Contract, #3)")]
fn test_reject_fee_recipient_immediate_withdraw() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);
    t.client.place_bet(&user, &id, &true, &100_0000000_i128);

    let recipient = Address::generate(&t.env);
    t.client.add_fee_recipient(&t.admin, &recipient);
    t.client.withdraw_fees(&recipient, &recipient);
}

// ── 27c. Withdraw to an arbitrary address is rejected (issue #12) ─────────────

#[test]
#[should_panic(expected = "Error(Contract, #21)")]
fn test_reject_withdraw_fees_to_arbitrary_recipient() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);
    t.client.place_bet(&user, &id, &true, &100_0000000_i128);

    let rando = Address::generate(&t.env);
    t.client.withdraw_fees(&t.admin, &rando);
}

#[test]
#[should_panic(expected = "Error(Contract, #21)")]
fn test_reject_fee_recipient_request_arbitrary_recipient() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);
    t.client.place_bet(&user, &id, &true, &100_0000000_i128);

    let recipient = Address::generate(&t.env);
    t.client.add_fee_recipient(&t.admin, &recipient);
    let rando = Address::generate(&t.env);
    t.client.request_withdraw_fees(&recipient, &rando, &1_i128);
}

// ── 27d. Cannot drain the whole accumulator in one request (issue #12) ────────

#[test]
#[should_panic(expected = "Error(Contract, #22)")]
fn test_reject_drain_entire_accumulator_in_one_request() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);
    t.client.place_bet(&user, &id, &true, &100_0000000_i128);

    let recipient = Address::generate(&t.env);
    t.client.add_fee_recipient(&t.admin, &recipient);

    let fees = t.client.get_accumulated_fees();
    t.client.request_withdraw_fees(&recipient, &recipient, &fees);
}

// ── 27e. Payout is locked until the timelock elapses (issue #12) ──────────────

#[test]
#[should_panic(expected = "Error(Contract, #25)")]
fn test_withdrawal_execute_before_delay() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);
    t.client.place_bet(&user, &id, &true, &100_0000000_i128);

    let recipient = Address::generate(&t.env);
    t.client.add_fee_recipient(&t.admin, &recipient);

    let fees = t.client.get_accumulated_fees();
    let cap = fees * MAX_WITHDRAWAL_BPS / BPS_DENOM;
    t.client.request_withdraw_fees(&recipient, &recipient, &cap);
    t.client.execute_withdraw_fees(&recipient);
}

// ── 27f. Admin can cancel a pending withdrawal request (issue #12) ────────────

#[test]
fn test_admin_cancel_pending_withdrawal() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);
    t.client.place_bet(&user, &id, &true, &100_0000000_i128);

    let recipient = Address::generate(&t.env);
    t.client.add_fee_recipient(&t.admin, &recipient);

    let fees = t.client.get_accumulated_fees();
    let cap = fees * MAX_WITHDRAWAL_BPS / BPS_DENOM;
    t.client.request_withdraw_fees(&recipient, &recipient, &cap);
    assert!(t.client.get_pending_withdrawal(&recipient).is_some());

    t.client.cancel_withdrawal_request(&t.admin, &recipient);
    assert!(t.client.get_pending_withdrawal(&recipient).is_none());
    assert_eq!(t.client.get_accumulated_fees(), fees);
}

// ── 27g. Executing without a pending request is rejected (issue #12) ──────────

#[test]
#[should_panic(expected = "Error(Contract, #24)")]
fn test_execute_without_request() {
    let t = setup();
    let rando = Address::generate(&t.env);
    t.client.execute_withdraw_fees(&rando);
}

// ── 27i. Recipient revoked during the timelock cannot execute (issue #12) ──────

#[test]
#[should_panic(expected = "Error(Contract, #18)")]
fn test_execute_rejected_after_fee_recipient_removed() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);
    t.client.place_bet(&user, &id, &true, &100_0000000_i128);

    let recipient = Address::generate(&t.env);
    t.client.add_fee_recipient(&t.admin, &recipient);

    let fees = t.client.get_accumulated_fees();
    let cap = fees * MAX_WITHDRAWAL_BPS / BPS_DENOM;
    t.client.request_withdraw_fees(&recipient, &recipient, &cap);

    // Role revoked while the 24h timelock is still running.
    t.client.remove_fee_recipient(&t.admin, &recipient);
    advance_time(&t.env, WITHDRAW_DELAY_SECS);
    t.client.execute_withdraw_fees(&recipient);
}

// ── 27h. Duplicate withdrawal requests are rejected (issue #12) ───────────────

#[test]
#[should_panic(expected = "Error(Contract, #23)")]
fn test_reject_duplicate_withdrawal_request() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);
    t.client.place_bet(&user, &id, &true, &100_0000000_i128);

    let recipient = Address::generate(&t.env);
    t.client.add_fee_recipient(&t.admin, &recipient);

    let fees = t.client.get_accumulated_fees();
    let cap = fees * MAX_WITHDRAWAL_BPS / BPS_DENOM;
    t.client.request_withdraw_fees(&recipient, &recipient, &cap);
    t.client.request_withdraw_fees(&recipient, &recipient, &cap);
}

// ── 28. Non-authorized cannot withdraw fees ───────────────────────────────────

#[test]
#[should_panic(expected = "Error(Contract, #3)")]
fn test_reject_withdraw_fees_non_admin() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);
    t.client.place_bet(&user, &id, &true, &100_0000000_i128);
    let rando = Address::generate(&t.env);
    t.client.withdraw_fees(&rando, &rando);
}

// ── 29. Bettor index enumeration ─────────────────────────────────────────────

#[test]
fn test_bettor_index_enumeration() {
    let t = setup();
    let id = create_test_market(&t);
    let alice = Address::generate(&t.env);
    let bob = Address::generate(&t.env);
    let charlie = Address::generate(&t.env);
    fund_user(&t, &alice, 200_0000000);
    fund_user(&t, &bob, 200_0000000);
    fund_user(&t, &charlie, 200_0000000);

    t.client.place_bet(&alice, &id, &true, &10_0000000_i128);
    t.client.place_bet(&bob, &id, &false, &20_0000000_i128);
    t.client.place_bet(&charlie, &id, &true, &30_0000000_i128);

    let bettors = t.client.get_market_bettors(&id);
    assert_eq!(bettors.len(), 3);
    assert_eq!(bettors.get(0).unwrap(), alice);
    assert_eq!(bettors.get(1).unwrap(), bob);
    assert_eq!(bettors.get(2).unwrap(), charlie);

    let first_page = t.client.get_market_bettors_page(&id, &0, &2);
    assert_eq!(first_page.len(), 2);
    assert_eq!(first_page.get(0).unwrap(), alice);
    assert_eq!(first_page.get(1).unwrap(), bob);

    let second_page = t.client.get_market_bettors_page(&id, &2, &2);
    assert_eq!(second_page.len(), 1);
    assert_eq!(second_page.get(0).unwrap(), charlie);
}

#[test]
fn test_bettor_index_legacy_read_is_bounded() {
    let t = setup();
    // Simulating a 101-entry legacy index legitimately reads 100 slots in one
    // call, which exceeds the default mainnet-like resource limits — this test
    // proves read boundedness, not gas, so lift the limits like other suites.
    t.env.cost_estimate().disable_resource_limits();
    let id = create_test_market(&t);
    let first = Address::generate(&t.env);
    let beyond_first_page = Address::generate(&t.env);

    // Simulate a large legacy index without spending time creating 101 bets.
    // (Note: the legacy full-page ABI reads up to MAX_BETTORS_PER_PAGE index
    // entries, which exceeds the 100-ledger-entry cap of the mock env, so the
    // bounded-read guarantee is exercised through the paginated ABI with small
    // pages — the same code path the legacy read delegates to.)
    t.env.as_contract(&t.client.address, || {
        t.env
            .storage()
            .persistent()
            .set(&DataKey::BettorCount(id), &(MAX_BETTORS_PER_PAGE + 1));
        t.env
            .storage()
            .persistent()
            .set(&DataKey::BettorAt(id, 0), &first);
        t.env.storage().persistent().set(
            &DataKey::BettorAt(id, MAX_BETTORS_PER_PAGE),
            &beyond_first_page,
        );
    });

    let legacy_page = t.client.get_market_bettors_page(&id, &0, &3);
    assert_eq!(legacy_page.len(), 1);
    assert_eq!(legacy_page.get(0).unwrap(), first);

    let later_page = t
        .client
        .get_market_bettors_page(&id, &MAX_BETTORS_PER_PAGE, &1);
    assert_eq!(later_page.len(), 1);
    assert_eq!(later_page.get(0).unwrap(), beyond_first_page);
}

// ── 30. Referral bonus points per referred bet (Issue #99: ref registered) ───

#[test]
fn test_referrer_bonus_points_per_bet() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    let referrer = Address::generate(&t.env);
    fund_user(&t, &user, 500_0000000);

    // Issue #99: referrer must register first; they earn a 5-pt welcome bonus.
    t.referral_client
        .register_referral(&referrer, &String::from_str(&t.env, "RefFan"), &None);
    let no_ref: Option<Address> = None;
    t.referral_client.register_referral(
        &referrer,
        &String::from_str(&t.env, "Referrer"),
        &no_ref,
    );
    t.referral_client.register_referral(
        &user,
        &String::from_str(&t.env, "Fan"),
        &Some(referrer.clone()),
    );

    t.client.place_bet(&user, &id, &true, &100_0000000_i128);
    t.client.place_bet(&user, &id, &true, &50_0000000_i128);

    // 5 welcome + 3 + 3 referral-bet bonuses.
    t.leaderboard_client.claim_pending_rewards(&referrer);
    assert_eq!(t.leaderboard_client.get_points(&referrer), 11);
}

// ── 31. Spam guard: TooManyBets ──────────────────────────────────────────────

#[test]
#[should_panic(expected = "Error(Contract, #17)")]
fn test_reject_too_many_bets() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 100_000_000_000);

    // 20 XLM per bet so the NET stake (98%) clears MIN_BET; the 21st bet
    // must trip MAX_BETS_PER_USER instead of BetTooSmall.
    for _ in 0..=20u32 {
        t.client.place_bet(&user, &id, &true, &20_0000000_i128);
    // 1.1 XLM gross clears the net minimum (net = 1.078 XLM >= MIN_BET) so the
    // 21st bet actually trips the TooManyBets guard instead of BetTooSmall.
    for _ in 0..=20u32 {
        t.client.place_bet(&user, &id, &true, &11_0000000_i128);
    }
}

// ── 32. Market creation rate limiting ────────────────────────────────────────

#[test]
fn test_market_creation_rate_limit_allows_up_to_max() {
    let t = setup();
    // Should be able to create up to MAX_MARKETS_PER_HOUR (10) in the same window
    for i in 0..10u32 {
        let _ = t.client.create_market(
            &t.admin,
            &String::from_str(&t.env, "Market"),
            &String::from_str(&t.env, "https://x.png"),
            &Category::Crypto,
            &(3600_u64 + i as u64),
        );
    }
    assert_eq!(t.client.get_market_count(), 10);
}

#[test]
#[should_panic(expected = "Error(Contract, #20)")]
fn test_market_creation_rate_limit_exceeded() {
    let t = setup();
    // Create 10 markets (the limit)
    for i in 0..10u32 {
        let _ = t.client.create_market(
            &t.admin,
            &String::from_str(&t.env, "Market"),
            &String::from_str(&t.env, "https://x.png"),
            &Category::Crypto,
            &(3600_u64 + i as u64),
        );
    }
    // 11th should fail
    t.client.create_market(
        &t.admin,
        &String::from_str(&t.env, "Over limit"),
        &String::from_str(&t.env, "https://x.png"),
        &Category::Sports,
        &7200_u64,
    );
}

#[test]
fn test_market_creation_rate_limit_resets_after_window() {
    let t = setup();
    for i in 0..10u32 {
        let _ = t.client.create_market(
            &t.admin,
            &String::from_str(&t.env, "Market"),
            &String::from_str(&t.env, "https://x.png"),
            &Category::Crypto,
            &(3600_u64 + i as u64),
        );
    }
    // Advance past the 1-hour window
    advance_time(&t.env, 3601);
    // Should be able to create again
    let id = t.client.create_market(
        &t.admin,
        &String::from_str(&t.env, "New window market"),
        &String::from_str(&t.env, "https://x.png"),
        &Category::Sports,
        &7200_u64,
    );
    assert_eq!(id, 11);
}

#[test]
#[should_panic(expected = "Error(Contract, #20)")]
fn test_market_creation_rate_limit_rejects_timestamp_regression() {
    let t = setup();
    for i in 0..10u32 {
        let _ = t.client.create_market(
            &t.admin,
            &String::from_str(&t.env, "Market"),
            &String::from_str(&t.env, "https://x.png"),
            &Category::Crypto,
            &(3600_u64 + i as u64),
        );
    }

    // Rewinding the ledger must not reset the active rate-limit window.
    rewind_time(&t.env, 1);
    t.client.create_market(
        &t.admin,
        &String::from_str(&t.env, "Over limit after rewind"),
        &String::from_str(&t.env, "https://x.png"),
        &Category::Sports,
        &7200_u64,
    );
}

// ── 33. Double initialization rejected ───────────────────────────────────────

#[test]
#[should_panic(expected = "Error(Contract, #1)")]
fn test_double_init_rejected() {
    let t = setup();
    let tok2 = Address::generate(&t.env);
    let ref2 = Address::generate(&t.env);
    let lb2 = Address::generate(&t.env);
    let xlm2 = Address::generate(&t.env);
    t.client.initialize(&t.admin, &tok2, &ref2, &lb2, &xlm2);
}

// ── 34. Resolve before deadline rejected ─────────────────────────────────────

#[test]
#[should_panic(expected = "Error(Contract, #6)")]
fn test_reject_resolve_before_deadline() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);
    t.client.place_bet(&user, &id, &true, &50_0000000_i128);
    t.client.resolve_market(&t.admin, &id, &true);
}

// ── 35. Withdraw fees when zero ───────────────────────────────────────────────

#[test]
#[should_panic(expected = "Error(Contract, #15)")]
fn test_withdraw_fees_zero() {
    let t = setup();
    t.client.withdraw_fees(&t.admin, &t.admin);
}

// ── 36. Claim with no bet → NoBetFound ───────────────────────────────────────

#[test]
#[should_panic(expected = "Error(Contract, #13)")]
fn test_claim_no_bet_found() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);
    t.client.place_bet(&user, &id, &true, &50_0000000_i128);
    advance_time(&t.env, 3601);
    t.client.resolve_market(&t.admin, &id, &true);
    let stranger = Address::generate(&t.env);
    t.client.claim(&stranger, &id);
}

// ── 37. Non-admin create market rejected ─────────────────────────────────────

#[test]
#[should_panic(expected = "Error(Contract, #3)")]
fn test_reject_create_market_non_admin() {
    let t = setup();
    let rando = Address::generate(&t.env);
    t.client.create_market(
        &rando,
        &String::from_str(&t.env, "Unauthorized?"),
        &String::from_str(&t.env, "https://x.png"),
        &Category::Other,
        &3600_u64,
    );
}

// ── 38. Non-admin cancel rejected ────────────────────────────────────────────

#[test]
#[should_panic(expected = "Error(Contract, #3)")]
fn test_reject_cancel_market_non_admin() {
    let t = setup();
    let id = create_test_market(&t);
    let rando = Address::generate(&t.env);
    t.client.cancel_market(&rando, &id);
}

// ── 39. Market not found ─────────────────────────────────────────────────────

#[test]
#[should_panic(expected = "Error(Contract, #4)")]
fn test_market_not_found() {
    let t = setup();
    t.client.get_market(&999);
}

// ── 40. Multiple markets with categories ─────────────────────────────────────

#[test]
fn test_create_multiple_markets() {
    let t = setup();
    let id1 = t.client.create_market(
        &t.admin,
        &String::from_str(&t.env, "Market A"),
        &String::from_str(&t.env, "https://a.png"),
        &Category::Crypto,
        &3600_u64,
    );
    let id2 = t.client.create_market(
        &t.admin,
        &String::from_str(&t.env, "Market B"),
        &String::from_str(&t.env, "https://b.png"),
        &Category::Sports,
        &7200_u64,
    );
    assert_eq!(id1, 1);
    assert_eq!(id2, 2);
    assert_eq!(t.client.get_market_count(), 2);
    assert_eq!(t.client.get_market(&id2).category, Category::Sports);
}

// ── 41. Empty-side resolution: pool goes to AccumulatedFees, admin can withdraw ─

#[test]
fn test_empty_side_resolution_pool_to_fees() {
    let t = setup();
    let id = create_test_market(&t);
    let alice = Address::generate(&t.env);
    fund_user(&t, &alice, 200_0000000);

    // Only YES bets — no one bets NO
    t.client.place_bet(&alice, &id, &true, &100_0000000_i128);
    let fees_before = t.client.get_accumulated_fees();
    assert_eq!(fees_before, 1_5000000); // 1.5% platform fee (referral fee goes to surplus)

    // Advance past end_time and resolve NO (empty winning side)
    advance_time(&t.env, 3601);
    t.client.resolve_market(&t.admin, &id, &false); // total_no == 0

    // The entire pool (total_yes net = 98 XLM) must be swept into AccumulatedFees
    let fees_after = t.client.get_accumulated_fees();
    assert_eq!(
        fees_after,
        fees_before + 98_0000000,
        "entire YES pool should sweep to fees when NO side is empty"
    );

    // Admin can withdraw the swept pool to a registered treasury
    let treasury = Address::generate(&t.env);
    t.client.add_fee_recipient(&t.admin, &treasury);
    let before = t.xlm.balance(&treasury);
    let withdrawn = t.client.withdraw_fees(&t.admin, &treasury);
    assert_eq!(withdrawn, fees_after);
    assert_eq!(t.xlm.balance(&treasury), before + fees_after);
    assert_eq!(t.client.get_accumulated_fees(), 0);

    // Alice (was YES, losing side) can still claim — gets PULSE tokens + points
    t.client.claim(&alice, &id);
    t.leaderboard_client.claim_pending_rewards(&alice);
    let bet = t.client.get_bet(&id, &alice);
    assert!(bet.claimed);
    // Gets lose-tier rewards because winning_side == 0
    assert_eq!(t.token_client.balance(&alice), 2_0000000); // LOSE_TOKENS
    assert_eq!(t.leaderboard_client.get_points(&alice), 10); // LOSE_POINTS
}

// ── 42. Cancel accumulates fees on multiple bets correctly ────────────────────

#[test]
fn test_cancel_fees_zeroed_correctly() {
    let t = setup();
    let id = create_test_market(&t);
    let alice = Address::generate(&t.env);
    let bob = Address::generate(&t.env);
    fund_user(&t, &alice, 200_0000000);
    fund_user(&t, &bob, 200_0000000);

    // Two bets accumulate fees (only platform_fee tracked; referral fee goes to surplus)
    t.client.place_bet(&alice, &id, &true, &100_0000000_i128); // 1.5 XLM platform fee
    t.client.place_bet(&bob, &id, &false, &100_0000000_i128); // 1.5 XLM platform fee
    assert_eq!(t.client.get_accumulated_fees(), 3_0000000);

    // Cancel zeroes out those fees
    t.client.cancel_market(&t.admin, &id);
    assert_eq!(t.client.get_accumulated_fees(), 0);

    // Bettors get their gross back
    t.client.cancel_refund(&alice, &id);
    t.client.cancel_refund(&bob, &id);
}

// ── 42b. Cancel reclaims only the fees a market actually retained (issue #87) ─

#[test]
fn test_cancel_market_reclaims_only_retained_fees() {
    let t = setup();

    let alice = Address::generate(&t.env);
    let bob = Address::generate(&t.env);
    let charlie = Address::generate(&t.env);
    let referrer = Address::generate(&t.env);
    fund_user(&t, &alice, 200_0000000);
    fund_user(&t, &bob, 200_0000000);
    fund_user(&t, &charlie, 200_0000000);

    // Referrer + Bob (with referrer). Alice and Charlie bet without one.
    let no_ref: Option<Address> = None;
    t.referral_client.register_referral(
        &referrer,
        &String::from_str(&t.env, "Referrer"),
        &no_ref,
    );
    t.referral_client.register_referral(
        &bob,
        &String::from_str(&t.env, "Bob"),
        &Some(referrer.clone()),
    );

    let m1 = t.client.create_market(
        &t.admin,
        &String::from_str(&t.env, "M1"),
        &String::from_str(&t.env, "https://m1.png"),
        &Category::Crypto,
        &3600_u64,
    );
    let m2 = t.client.create_market(
        &t.admin,
        &String::from_str(&t.env, "M2"),
        &String::from_str(&t.env, "https://m2.png"),
        &Category::Crypto,
        &3600_u64,
    );

    // Market 1: Alice (no referrer → 2 XLM retained) + Bob (referrer → 1.5
    // XLM retained, 0.5 XLM already paid to the referrer at bet time).
    t.client.place_bet(&alice, &m1, &true, &100_0000000_i128);
    t.client.place_bet(&bob, &m1, &false, &100_0000000_i128);

    // Market 2: Charlie (no referrer → 2 XLM retained).
    t.client.place_bet(&charlie, &m2, &true, &100_0000000_i128);

    // 2 + 1.5 + 2 = 5.5 XLM accumulated; the referrer already holds 0.5 XLM.
    assert_eq!(t.client.get_accumulated_fees(), 5_5000000);
    assert_eq!(t.xlm.balance(&referrer), 5000000);

    // Cancelling market 1 must reclaim only 3.5 XLM (2 + 1.5), leaving
    // market 2's 2 XLM untouched. The old net-pool formula reclaimed 4 XLM
    // and silently ate 0.5 XLM of market 2's platform fees.
    t.client.cancel_market(&t.admin, &m1);
    assert_eq!(t.client.get_accumulated_fees(), 2_0000000);

    // Bettors still pull their full gross refunds; the referrer fee is gone.
    assert_eq!(t.client.cancel_refund(&alice, &m1), 100_0000000);
    assert_eq!(t.client.cancel_refund(&bob, &m1), 100_0000000);
}

// ═══════════════════════════════════════════════════════════════════════════════
// 42. COMPREHENSIVE END-TO-END INTEGRATION TEST
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn test_e2e_full_inter_contract_flow() {
    let t = setup();

    let alice_user = Address::generate(&t.env);
    let bob = Address::generate(&t.env);
    let referrer = Address::generate(&t.env);
    fund_user(&t, &alice_user, 1000_0000000);
    fund_user(&t, &bob, 1000_0000000);

    // Issue #99: the referrer must be a registered participant first, and
    // receives their own 5-pt welcome bonus.
    t.referral_client
        .register_referral(&referrer, &String::from_str(&t.env, "Ref"), &None);
    let no_ref: Option<Address> = None;
    t.referral_client.register_referral(
        &referrer,
        &String::from_str(&t.env, "Referrer"),
        &no_ref,
    );
    t.referral_client.register_referral(
        &alice_user,
        &String::from_str(&t.env, "Alice"),
        &Some(referrer.clone()),
    );
    assert_eq!(t.leaderboard_client.get_points(&alice_user), 5);
    assert_eq!(t.token_client.balance(&alice_user), 1_0000000);
    t.leaderboard_client.claim_pending_rewards(&alice);
    assert_eq!(t.leaderboard_client.get_points(&alice), 5);
    assert_eq!(t.token_client.balance(&alice), 1_0000000);

    let market_id = t.client.create_market(
        &t.admin,
        &String::from_str(&t.env, "Will ETH flip BTC?"),
        &String::from_str(&t.env, "https://eth.png"),
        &Category::Crypto,
        &3600_u64,
    );
    assert_eq!(market_id, 1);

    // Alice bets YES 100 XLM — has referrer
    t.client
        .place_bet(&alice_user, &market_id, &true, &100_0000000_i128);
    assert_eq!(t.client.get_accumulated_fees(), 1_5000000);
    assert_eq!(t.xlm.balance(&referrer), 5000000);
    // Referrer: 5 welcome + 3 referral-bet points (issue #99: ref registered).
    assert_eq!(t.leaderboard_client.get_points(&referrer), 8);
    // Alice's welcome bonus counts as the activity: won(0) + lost(0) + bonus(1).
    assert_eq!(t.leaderboard_client.get_stats(&alice_user).total_bets, 1);
    t.leaderboard_client.claim_pending_rewards(&referrer);
    assert_eq!(t.leaderboard_client.get_points(&referrer), 8);
    // Alice's welcome bonus counts as activity: won(0) + lost(0) + bonus(1).
    assert_eq!(t.leaderboard_client.get_stats(&alice).total_bets, 1);
    assert_eq!(t.client.get_market(&market_id).total_yes, 98_0000000);
    assert_eq!(t.client.get_bet_gross(&market_id, &alice_user), 100_0000000);

    // Bob bets NO 200 XLM — no referrer
    t.client
        .place_bet(&bob, &market_id, &false, &200_0000000_i128);
    // Issue #78: only platform_fee tracked per bet; referral fee goes to surplus.
    // Alice: 1.5M, Bob: 3M platform fee → total 4.5M
    assert_eq!(t.client.get_accumulated_fees(), 4_5000000);
    // Bob never registered, so no bonus: total_bets = won(0) + lost(0) + bonus(0).
    assert_eq!(t.leaderboard_client.get_stats(&bob).total_bets, 0);
    assert_eq!(t.client.get_market(&market_id).total_no, 196_0000000);

    // Alice increases YES (+50 XLM)
    t.client
        .place_bet(&alice_user, &market_id, &true, &50_0000000_i128);
    let alice_bet = t.client.get_bet(&market_id, &alice_user);
    assert_eq!(alice_bet.amount, 98_0000000 + 49_0000000);
    assert_eq!(t.client.get_bet_gross(&market_id, &alice_user), 150_0000000);
    assert_eq!(t.client.get_market(&market_id).total_yes, 147_0000000);
    assert_eq!(t.client.get_market(&market_id).bet_count, 2);
    // 5 welcome + 3 + 3 referral-bet bonuses (issue #99: ref registered).
    t.leaderboard_client.claim_pending_rewards(&referrer);
    assert_eq!(t.leaderboard_client.get_points(&referrer), 11);

    // Add a resolver and resolve via them
    let resolver = Address::generate(&t.env);
    t.client.add_resolver(&t.admin, &resolver);
    advance_time(&t.env, 3601);
    t.client.resolve_market(&resolver, &market_id, &true);
    assert!(t.client.get_market(&market_id).resolved);

    // Alice claims as winner
    let alice_xlm_before = t.xlm.balance(&alice_user);
    t.client.claim(&alice_user, &market_id);
    let alice_payout = t.xlm.balance(&alice_user) - alice_xlm_before;
    let alice_xlm_before = t.xlm.balance(&alice);
    t.client.claim(&alice, &market_id);
    t.leaderboard_client.claim_pending_rewards(&alice);
    let alice_payout = t.xlm.balance(&alice) - alice_xlm_before;
    assert_eq!(alice_payout, 343_0000000);
    assert_eq!(t.leaderboard_client.get_points(&alice_user), 35);
    assert_eq!(t.token_client.balance(&alice_user), 11_0000000);

    // Bob claims as loser
    let bob_xlm_before = t.xlm.balance(&bob);
    t.client.claim(&bob, &market_id);
    t.leaderboard_client.claim_pending_rewards(&bob);
    assert_eq!(t.xlm.balance(&bob), bob_xlm_before);
    assert_eq!(t.leaderboard_client.get_points(&bob), 10);
    assert_eq!(t.token_client.balance(&bob), 2_0000000);

    // Fee withdrawal to a registered treasury address
    let treasury = Address::generate(&t.env);
    t.client.add_fee_recipient(&t.admin, &treasury);
    let fees_total = t.client.get_accumulated_fees();
    assert!(fees_total > 0);
    let treasury_before = t.xlm.balance(&treasury);
    let withdrawn = t.client.withdraw_fees(&t.admin, &treasury);
    assert_eq!(withdrawn, fees_total);
    assert_eq!(t.client.get_accumulated_fees(), 0);
    assert_eq!(t.xlm.balance(&treasury), treasury_before + fees_total);

    // Create second market, bet, then cancel — verify claim-style refund
    let market2 = t.client.create_market(
        &t.admin,
        &String::from_str(&t.env, "Will DOGE hit $1?"),
        &String::from_str(&t.env, "https://doge.png"),
        &Category::Crypto,
        &7200_u64,
    );
    let charlie = Address::generate(&t.env);
    fund_user(&t, &charlie, 500_0000000);
    let charlie_before = t.xlm.balance(&charlie);
    t.client
        .place_bet(&charlie, &market2, &true, &100_0000000_i128);
    t.client.cancel_market(&t.admin, &market2);
    // AccumulatedFees from market2 should be zeroed
    assert_eq!(t.client.get_accumulated_fees(), 0);
    // Charlie pulls their own refund (gross = 100 XLM)
    let refunded = t.client.cancel_refund(&charlie, &market2);
    assert_eq!(refunded, 100_0000000);
    assert_eq!(t.xlm.balance(&charlie), charlie_before);
}

// ═══════════════════════════════════════════════════════════════════════════
// SECURITY REGRESSION SUITE — issue #99 (referral validation)
// ═══════════════════════════════════════════════════════════════════════════

// ── #99: an unregistered attacker-controlled address can never be named as a
//    referrer, so it can never receive fees or accrue count/earnings ─────────
#[test]
#[should_panic(expected = "Error(Contract, #7)")]
fn test_reject_unregistered_referrer_e2e() {
    let t = setup();
    let user = Address::generate(&t.env);
    let attacker = Address::generate(&t.env);
    fund_user(&t, &user, 1_000_0000000);

    // Attacker never registers; naming them as referrer must fail.
    t.referral_client.register_referral(
        &user,
        &String::from_str(&t.env, "Victim"),
        &Some(attacker.clone()),
    );
}

// ── #99: full fee path only pays registered referrers ───────────────────────
#[test]
fn test_referral_fee_flow_registered_referrer() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    let referrer = Address::generate(&t.env);
    fund_user(&t, &user, 1_000_0000000);

    // Referrer registers first (their welcome bonus is +5 pts), then user.
    t.referral_client
        .register_referral(&referrer, &String::from_str(&t.env, "Ref"), &None);
    t.referral_client.register_referral(
        &user,
        &String::from_str(&t.env, "User"),
        &Some(referrer.clone()),
    );

    t.client.place_bet(&user, &id, &true, &100_0000000_i128);
    // 1.5% platform fee accrues; 0.5% referral fee goes to the referrer.
    assert_eq!(t.client.get_accumulated_fees(), 1_5000000);
    assert_eq!(t.xlm.balance(&referrer), 5000000);
    // Referrer count, earnings and bonus pts all exist for the REGISTERED ref.
    assert_eq!(t.referral_client.get_referral_count(&referrer), 1);
    assert_eq!(t.referral_client.get_earnings(&referrer), 5000000);
}

// ── #99: defense-in-depth — credit() refuses to pay an unregistered referrer
//    even if a stale/invalid referral edge exists in storage ────────────────
#[test]
fn test_credit_does_not_pay_unregistered_referrer() {
    let t = setup();
    let user = Address::generate(&t.env);
    let shady = Address::generate(&t.env);

    // Simulate legacy state: user registered pre-fix with an unregistered
    // referrer written straight to the legacy key layout (bypassing the fixed
    // register_referral). credit() must refuse to pay them.
    t.env.as_contract(&t.referral_client.address, || {
        t.env
            .storage()
            .persistent()
            .set(&ReferralDataKey::Registered(user.clone()), &true);
        t.env.storage().persistent().set(
            &ReferralDataKey::DisplayName(user.clone()),
            &String::from_str(&t.env, "LegacyVictim"),
        );
        t.env
            .storage()
            .persistent()
            .set(&ReferralDataKey::Referrer(user.clone()), &shady);
    });

    // Fund the referral registry so it could pay if it wrongly chose to.
    let sac_admin = StellarAssetClient::new(&t.env, &t.xlm_sac_id);
    sac_admin.mint(&t.referral_client.address, &100_0000000_i128);
    let xlm = TokenClient::new(&t.env, &t.xlm_sac_id);
    let market_before = xlm.balance(&t.client.address);

    // credit() should NOT pay the unregistered referrer — fee returns to the
    // market, no bonus pts, no earnings, no count created.
    let result = t
        .referral_client
        .credit(&t.client.address, &user, &5_000_000);
    assert!(!result, "unregistered referrer must not be paid");
    assert_eq!(xlm.balance(&shady), 0);
    assert_eq!(xlm.balance(&t.client.address), market_before + 5_000_000);
    assert_eq!(t.referral_client.get_earnings(&shady), 0);
    assert_eq!(t.referral_client.get_referral_count(&shady), 0);
    assert_eq!(t.leaderboard_client.get_points(&shady), 0);
}

// ── #99: registered referrer still fully works ──────────────────────────────
#[test]
fn test_referral_still_works_after_registered_referrer() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    let referrer = Address::generate(&t.env);
    fund_user(&t, &user, 1_000_0000000);

    t.referral_client
        .register_referral(&referrer, &String::from_str(&t.env, "Ref"), &None);
    t.referral_client.register_referral(
        &user,
        &String::from_str(&t.env, "User"),
        &Some(referrer.clone()),
    );
    t.client.place_bet(&user, &id, &true, &100_0000000_i128);
    t.client.place_bet(&user, &id, &true, &50_0000000_i128);
    // 0.5% of 100 + 0.5% of 50 = 0.5 + 0.25 XLM = 7_500_000 stroops.
    assert_eq!(t.xlm.balance(&referrer), 7_500000);
    assert_eq!(t.leaderboard_client.get_points(&referrer), 5 + 3 + 3);
}

// ═══════════════════════════════════════════════════════════════════════════
// SECURITY REGRESSION SUITE — legacy referral state remains readable and safe
// ═══════════════════════════════════════════════════════════════════════════

// ── #28: legacy referral state must stay readable (regression keeps the
//    existing pre-A readers working after the #99 fix) ──────────────────────
#[test]
fn test_legacy_referral_state_readable_after_fix() {
    let t = setup();
    let legacy_user = Address::generate(&t.env);
    let legacy_ref = Address::generate(&t.env);

    t.env.as_contract(&t.referral_client.address, || {
        t.env
            .storage()
            .persistent()
            .set(&ReferralDataKey::Registered(legacy_user.clone()), &true);
        t.env.storage().persistent().set(
            &ReferralDataKey::DisplayName(legacy_user.clone()),
            &String::from_str(&t.env, "OldTimer"),
        );
        t.env
            .storage()
            .persistent()
            .set(&ReferralDataKey::Referrer(legacy_user.clone()), &legacy_ref);
    });

    // Reads still work exactly as before the fix (legacy fallback preserved).
    assert!(t.referral_client.is_registered(&legacy_user));
    assert_eq!(
        t.referral_client.get_referrer(&legacy_user),
        Some(legacy_ref.clone())
    );
    assert_eq!(
        t.referral_client.get_display_name(&legacy_user),
        String::from_str(&t.env, "OldTimer")
    );
    assert!(t.referral_client.has_referrer(&legacy_user));
}

// ═══════════════════════════════════════════════════════════════════════════
// SECURITY REGRESSION SUITE — issue #2 (payout rounding / dust)
// ═══════════════════════════════════════════════════════════════════════════

// ── #2: settlement-time payouts — Σ payouts + dust == pool ──────────────────
#[test]
fn test_many_winners_payouts_exact_and_dust_swept() {
    let t = setup();
    let id = create_test_market(&t);

    let w1 = Address::generate(&t.env);
    let w2 = Address::generate(&t.env);
    let w3 = Address::generate(&t.env);
    let l1 = Address::generate(&t.env);
    fund_user(&t, &w1, 1_000_0000000);
    fund_user(&t, &w2, 1_000_0000000);
    fund_user(&t, &w3, 1_000_0000000);
    fund_user(&t, &l1, 1_000_0000000);

    // Deliberately uneven stakes that do NOT divide the pool evenly.
    t.client.place_bet(&w1, &id, &true, &30_000_001_i128);
    t.client.place_bet(&w2, &id, &true, &40_000_003_i128);
    t.client.place_bet(&w3, &id, &true, &50_000_007_i128);
    t.client.place_bet(&l1, &id, &false, &27_777_779_i128);

    advance_time(&t.env, 3601);
    let fees_before = t.client.get_accumulated_fees();
    t.client.resolve_market(&t.admin, &id, &true);

    let market = t.client.get_market(&id);
    let pool: i128 = market.total_yes + market.total_no;
    let win: i128 = market.total_yes;

    let n1 = t.client.get_bet(&id, &w1).amount;
    let n2 = t.client.get_bet(&id, &w2).amount;
    let n3 = t.client.get_bet(&id, &w3).amount;
    assert_eq!(n1 + n2 + n3, win);

    // Stored payouts must equal the exact integer formula.
    let p1 = (n1 * pool) / win;
    let p2 = (n2 * pool) / win;
    let p3 = (n3 * pool) / win;
    assert_eq!(t.client.get_payout(&id, &w1), p1);
    assert_eq!(t.client.get_payout(&id, &w2), p2);
    assert_eq!(t.client.get_payout(&id, &w3), p3);
    assert_eq!(t.client.get_payout(&id, &l1), 0);

    // The dust is deterministic, bounded, and swept to the fee accumulator.
    let dust: i128 = pool - p1 - p2 - p3;
    assert!(dust >= 0);
    assert!(dust < win);
    assert_eq!(t.client.get_accumulated_fees(), fees_before + dust);

    // No overpay: the sum of payouts never exceeds the pool.
    assert!(p1 + p2 + p3 <= pool);

    // After all claims the market's balance drops by exactly Σ payouts.
    let market_contract = t.client.address.clone();
    let bal_before = t.xlm.balance(&market_contract);
    t.client.claim(&w1, &id);
    t.client.claim(&w2, &id);
    t.client.claim(&w3, &id);
    assert_eq!(
        bal_before - t.xlm.balance(&market_contract),
        p1 + p2 + p3
    );
    assert_eq!(t.xlm.balance(&w1), 1_000_0000000_i128 - 30_000_001_i128 + p1);
}

// ── #2: single winner receives the whole pool (no dust) ─────────────────────
#[test]
fn test_single_winner_gets_whole_net_pool() {
    let t = setup();
    let id = create_test_market(&t);
    let winner = Address::generate(&t.env);
    let loser = Address::generate(&t.env);
    fund_user(&t, &winner, 1_000_0000000);
    fund_user(&t, &loser, 1_000_0000000);

    t.client.place_bet(&winner, &id, &true, &60_0000000_i128);
    t.client.place_bet(&loser, &id, &false, &60_0000000_i128);
    advance_time(&t.env, 3601);
    t.client.resolve_market(&t.admin, &id, &true);

    let market = t.client.get_market(&id);
    let total = market.total_yes + market.total_no;
    let before = t.xlm.balance(&winner);
    t.client.claim(&winner, &id);
    // Whole pool (both nets) goes to the single winner.
    assert_eq!(t.xlm.balance(&winner) - before, total);
    assert_eq!(t.client.get_payout(&id, &winner), total);
}

// ═══════════════════════════════════════════════════════════════════════════
// SECURITY REGRESSION SUITE — issue #9 (persistent storage TTL)
// ═══════════════════════════════════════════════════════════════════════════

fn advance_ledgers(env: &Env, n: u32) {
    let current_seq = env.ledger().sequence();
    env.ledger().set(LedgerInfo {
        timestamp: env.ledger().timestamp() + 1,
        protocol_version: 26,
        sequence_number: current_seq + n,
        network_id: Default::default(),
        base_reserve: 10,
        min_temp_entry_ttl: 100,
        min_persistent_entry_ttl: 100,
        max_entry_ttl: 10_000_000,
    });
}

// ── #9: claims/refunds keep recoverable storage alive (TTL re-bump) ──────────
#[test]
fn test_claim_rebumps_ttl_entries() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);
    t.client.place_bet(&user, &id, &true, &100_0000000_i128);
    advance_time(&t.env, 3601);
    t.client.resolve_market(&t.admin, &id, &true);

    // Fast-forward deep into the TTL window (but not past it).
    advance_ledgers(&t.env, 6_000_000);

    let market_contract = t.client.address.clone();
    let bet_key = DataKey::Bet(id, user.clone());
    let market_key = DataKey::Market(id);
    let ttl = |key: &DataKey| -> u32 {
        t.env
            .as_contract(&market_contract, || t.env.storage().persistent().get_ttl(key))
    };
    let before_bet = ttl(&bet_key);
    let before_market = ttl(&market_key);

    t.client.claim(&user, &id);

    let after_bet = ttl(&bet_key);
    let after_market = ttl(&market_key);
    assert!(after_bet > before_bet);
    assert!(after_market > before_market);
}

#[test]
fn test_cancel_refund_rebumps_ttl_entries() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);
    t.client.place_bet(&user, &id, &true, &100_0000000_i128);
    t.client.cancel_market(&t.admin, &id);

    advance_ledgers(&t.env, 6_000_000);

    let market_contract = t.client.address.clone();
    let bet_key = DataKey::Bet(id, user.clone());
    let market_key = DataKey::Market(id);
    let ttl = |key: &DataKey| -> u32 {
        t.env
            .as_contract(&market_contract, || t.env.storage().persistent().get_ttl(key))
    };
    let bet_before = ttl(&bet_key);
    let market_before = ttl(&market_key);

    t.client.cancel_refund(&user, &id);

    assert!(ttl(&bet_key) > bet_before);
    assert!(ttl(&market_key) > market_before);
}

// ── #54: permissionless refresh + per-market expiry tracking + migration ─────

#[test]
fn test_get_market_ttl_tracks_live_entry() {
    let t = setup();
    assert_eq!(t.client.get_market_ttl(&99_u64), 0);
    let id = create_test_market(&t);
    assert!(t.client.get_market_ttl(&id) >= TTL_BUMP);
}

#[test]
fn test_refresh_market_ttl_rebumps_bet_and_market() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);
    t.client.place_bet(&user, &id, &true, &100_0000000_i128);

    advance_ledgers(&t.env, 6_000_000);
    let market_contract = t.client.address.clone();
    let bet_key = DataKey::Bet(id, user.clone());
    let market_key = DataKey::Market(id);
    let ttl = |key: &DataKey| -> u32 {
        t.env
            .as_contract(&market_contract, || t.env.storage().persistent().get_ttl(key))
    };
    let bet_before = ttl(&bet_key);
    let market_before = ttl(&market_key);

    assert_eq!(t.client.refresh_market_ttl(&id), 1);
    assert!(ttl(&bet_key) > bet_before);
    assert!(ttl(&market_key) > market_before);
    assert!(t.client.get_market_ttl(&id) > market_before);
}

// ── Cross-contract interface versioning (issue #84) ───────────────────────────

// Stands in for a referral_registry/leaderboard deployment upgraded to an
// incompatible ABI: it only implements interface_version(), reporting a
// version this prediction_market build does not expect.
#[contract]
struct MockIncompatibleDependency;

#[contractimpl]
impl MockIncompatibleDependency {
    pub fn interface_version(_env: Env) -> u32 {
        99
    }
}

#[test]
fn test_interface_version_reported() {
    let t = setup();
    assert_eq!(t.client.interface_version(), 1);
}

#[test]
#[should_panic(expected = "Error(Contract, #36)")]
fn test_place_bet_rejects_incompatible_referral() {
    let t = setup();
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);
    t.client.place_bet(&user, &id, &true, &100_0000000_i128);

    // Apply the incompatible referral first (through the timelock) so the
    // market created below is bet against while still live.
    let fake_referral = t.env.register(MockIncompatibleDependency, ());
    let cfg = t.client.get_config();
    t.client.set_config(
        &t.admin,
        &cfg.token,
        &fake_referral,
        &cfg.leaderboard,
        &cfg.xlm_sac,
    );
    advance_time(&t.env, CONFIG_CHANGE_DELAY_SECS);
    t.client.execute_set_config(&t.admin);

    let id = create_test_market(&t);
    t.client.place_bet(&user, &id, &true, &100_0000000_i128);
    advance_ledgers(&t.env, 6_000_000);

    let market_contract = t.client.address.clone();
    let bet_key = DataKey::Bet(id, user.clone());
    let market_key = DataKey::Market(id);
    let ttl = |key: &DataKey| -> u32 {
        t.env
            .as_contract(&market_contract, || t.env.storage().persistent().get_ttl(key))
    };
    let bet_before = ttl(&bet_key);
    let market_before = ttl(&market_key);

    // Anyone can pay to keep the keys alive — no auth required.
    assert_eq!(t.client.refresh_market_ttl(&id), 1);
    assert!(ttl(&bet_key) > bet_before);
    assert!(ttl(&market_key) > market_before);
    assert!(t.client.get_market_ttl(&id) > market_before);
}

#[test]
fn test_refresh_markets_migrates_existing_entries() {
    let t = setup();
    let a = create_test_market(&t);
    let b = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 400_0000000);
    t.client.place_bet(&user, &a, &true, &100_0000000_i128);
    t.client.place_bet(&user, &b, &true, &100_0000000_i128);

    advance_ledgers(&t.env, 6_000_000);
    let before_a = t.client.get_market_ttl(&a);
    let bumped = t.client.refresh_markets(&1_u64, &20_u32);
    assert_eq!(bumped, 2);
    assert!(t.client.get_market_ttl(&a) > before_a);
    assert!(t.client.get_market_ttl(&b) >= TTL_BUMP);
}

#[test]
fn test_resolve_market_rebumps_payout_entry() {
    let t = setup();
    let id = create_test_market(&t);
    let winner = Address::generate(&t.env);
    let loser = Address::generate(&t.env);
    fund_user(&t, &winner, 200_0000000);
    fund_user(&t, &loser, 200_0000000);
    t.client.place_bet(&winner, &id, &true, &100_0000000_i128);
    t.client.place_bet(&loser, &id, &false, &100_0000000_i128);

    advance_time(&t.env, 3601);
    t.client.resolve_market(&t.admin, &id, &true);
    let market_contract = t.client.address.clone();
    let payout_key = DataKey::Payout(id, winner);
    let payout_ttl = t.env.as_contract(&market_contract, || {
        t.env.storage().persistent().get_ttl(&payout_key)
    });
    assert!(payout_ttl >= TTL_BUMP);
}

#[test]
fn test_claim_survives_incompatible_leaderboard() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);
    t.client.place_bet(&user, &id, &true, &100_0000000_i128);
    advance_time(&t.env, 3601);
    t.client.resolve_market(&t.admin, &id, &true);

    let fake_leaderboard = t.env.register(MockIncompatibleDependency, ());
    let cfg = t.client.get_config();
    t.client.set_config(
        &t.admin,
        &cfg.token,
        &cfg.referral,
        &fake_leaderboard,
        &cfg.xlm_sac,
    );
    advance_time(&t.env, CONFIG_CHANGE_DELAY_SECS);
    t.client.execute_set_config(&t.admin);

    t.client.claim(&user, &id);
    assert!(t.client.get_bet(&id, &user).claimed);
}

// Stands in for a leaderboard deployment that reports the version this
// prediction_market build expects, but is missing the actual reward()
// function it's about to call. Proves the known limitation of the version
// check: a matching u32 alone does not prove ABI compatibility, only that
// the callee's author intended it to be compatible. If a breaking change to
// reward()'s signature ever ships without bumping INTERFACE_VERSION, this is
// exactly the failure mode that results, just past the version check instead
// of at it.
#[contract]
struct MockLeaderboardMissingReward;

#[contractimpl]
impl MockLeaderboardMissingReward {
    pub fn interface_version(_env: Env) -> u32 {
        1
    }
    // No reward() here on purpose.
}

#[test]
fn test_matching_version_does_not_block_claim_when_queue_is_missing() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);
    t.client.place_bet(&user, &id, &true, &100_0000000_i128);
    advance_time(&t.env, 3601);
    t.client.resolve_market(&t.admin, &id, &true);

    let fake_leaderboard = t.env.register(MockLeaderboardMissingReward, ());
    let cfg = t.client.get_config();
    t.client.set_config(
        &t.admin,
        &cfg.token,
        &cfg.referral,
        &fake_leaderboard,
        &cfg.xlm_sac,
    );
    advance_time(&t.env, CONFIG_CHANGE_DELAY_SECS);
    t.client.execute_set_config(&t.admin);

    // The optional queue call fails, but claim state and the XLM payout remain
    // successful because the failure is intentionally ignored.
    t.client.claim(&user, &id);
    assert!(t.client.get_bet(&id, &user).claimed);
}

// ── Emergency Pause (issue #83) ───────────────────────────────────────────────

#[test]
fn test_pause_unpause_admin_only() {
    let t = setup();
    assert!(!t.client.is_paused());

    t.client.pause(&t.admin);
    assert!(t.client.is_paused());

    t.client.unpause(&t.admin);
    assert!(!t.client.is_paused());
}

#[test]
#[should_panic(expected = "Error(Contract, #26)")]
fn test_paused_rejects_create_market() {
    let t = setup();
    t.client.pause(&t.admin);
    create_test_market(&t);
}

#[test]
#[should_panic(expected = "Error(Contract, #26)")]
fn test_paused_rejects_place_bet() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);

    t.client.pause(&t.admin);
    t.client.place_bet(&user, &id, &true, &100_0000000_i128);
}

#[test]
#[should_panic(expected = "Error(Contract, #26)")]
fn test_paused_rejects_resolve_market() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);
    t.client.place_bet(&user, &id, &true, &100_0000000_i128);
    advance_time(&t.env, 3601);

    t.client.pause(&t.admin);
    t.client.resolve_market(&t.admin, &id, &true);
}

#[test]
#[should_panic(expected = "Error(Contract, #26)")]
fn test_paused_rejects_claim() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);
    t.client.place_bet(&user, &id, &true, &100_0000000_i128);
    advance_time(&t.env, 3601);
    t.client.resolve_market(&t.admin, &id, &true);

    t.client.pause(&t.admin);
    t.client.claim(&user, &id);
}

#[test]
#[should_panic(expected = "Error(Contract, #26)")]
fn test_paused_rejects_withdraw_fees() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);
    t.client.place_bet(&user, &id, &true, &100_0000000_i128);

    t.client.pause(&t.admin);
    t.client.withdraw_fees(&t.admin, &t.admin);
}

// Refunds remain the users' emergency exit even while paused.
#[test]
fn test_cancel_refund_still_works_while_paused() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);
    t.client.place_bet(&user, &id, &true, &100_0000000_i128);
    t.client.cancel_market(&t.admin, &id);

    t.client.pause(&t.admin);
    let refunded = t.client.cancel_refund(&user, &id);
    assert_eq!(refunded, 100_0000000);
}

// View functions must keep working while paused.
#[test]
fn test_view_functions_work_while_paused() {
    let t = setup();
    let id = create_test_market(&t);
    t.client.pause(&t.admin);

    assert_eq!(t.client.get_market_count(), 1);
    let market = t.client.get_market(&id);
    assert_eq!(market.id, id);
}

#[test]
#[should_panic(expected = "Error(Contract, #26)")]
fn test_paused_rejects_cancel_market() {
    let t = setup();
    let id = create_test_market(&t);

    t.client.pause(&t.admin);
    t.client.cancel_market(&t.admin, &id);
}

#[test]
#[should_panic(expected = "Error(Contract, #26)")]
fn test_paused_rejects_request_withdraw_fees() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);
    t.client.place_bet(&user, &id, &true, &100_0000000_i128);

    let recipient = Address::generate(&t.env);
    t.client.add_fee_recipient(&t.admin, &recipient);
    let fees = t.client.get_accumulated_fees();
    let cap = fees * MAX_WITHDRAWAL_BPS / BPS_DENOM;

    t.client.pause(&t.admin);
    t.client.request_withdraw_fees(&recipient, &recipient, &cap);
}

#[test]
#[should_panic(expected = "Error(Contract, #26)")]
fn test_paused_rejects_execute_withdraw_fees() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);
    t.client.place_bet(&user, &id, &true, &100_0000000_i128);

    let recipient = Address::generate(&t.env);
    t.client.add_fee_recipient(&t.admin, &recipient);
    let fees = t.client.get_accumulated_fees();
    let cap = fees * MAX_WITHDRAWAL_BPS / BPS_DENOM;

    // Request while unpaused, then let the timelock mature — the pause check
    // in execute_withdraw_fees must still block payout even on a matured request.
    t.client.request_withdraw_fees(&recipient, &recipient, &cap);
    advance_time(&t.env, WITHDRAW_DELAY_SECS);

    t.client.pause(&t.admin);
    t.client.execute_withdraw_fees(&recipient);
}

// The admin's ability to kill a compromised/stuck withdrawal request must
// remain available mid-pause, same as the users' cancel_refund exit path.
#[test]
fn test_cancel_withdrawal_request_still_works_while_paused() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);
    t.client.place_bet(&user, &id, &true, &100_0000000_i128);

    let recipient = Address::generate(&t.env);
    t.client.add_fee_recipient(&t.admin, &recipient);
    let fees = t.client.get_accumulated_fees();
    let cap = fees * MAX_WITHDRAWAL_BPS / BPS_DENOM;
    t.client.request_withdraw_fees(&recipient, &recipient, &cap);

    t.client.pause(&t.admin);
    t.client.cancel_withdrawal_request(&t.admin, &recipient);

    assert!(t.client.get_pending_withdrawal(&recipient).is_none());
}

// ── Timelocked config changes (issue #93) ───────────────────────────────────
//
// set_config no longer re-points the market to arbitrary addresses instantly.
// It stages the change, which only lands after CONFIG_CHANGE_DELAY_SECS via
// execute_set_config, and can be cancelled before it matures. This gives
// off-chain monitors time to detect a malicious redirect and the admin time to
// reverse it.

#[test]
fn test_set_config_is_timelocked() {
    let t = setup();
    let new_token = Address::generate(&t.env);
    let new_referral = Address::generate(&t.env);
    let new_leaderboard = Address::generate(&t.env);
    let new_xlm = Address::generate(&t.env);

    let before = t.client.get_config();
    t.client.set_config(
        &t.admin,
        &new_token,
        &new_referral,
        &new_leaderboard,
        &new_xlm,
    );

    // Staged but NOT applied yet.
    assert_eq!(t.client.get_config(), before);
    let pending = t.client.get_pending_config().unwrap();
    assert_eq!(pending.token, new_token);
    assert_eq!(pending.pending_at, t.env.ledger().timestamp());

    // After the delay it lands.
    advance_time(&t.env, CONFIG_CHANGE_DELAY_SECS);
    t.client.execute_set_config(&t.admin);

    let after = t.client.get_config();
    assert_eq!(after.token, new_token);
    assert_eq!(after.referral, new_referral);
    assert_eq!(after.leaderboard, new_leaderboard);
    assert_eq!(after.xlm_sac, new_xlm);
    assert!(t.client.get_pending_config().is_none());
}

#[test]
#[should_panic(expected = "Error(Contract, #30)")]
fn test_execute_set_config_before_delay_rejected() {
    let t = setup();
    let new_token = Address::generate(&t.env);
    let new_referral = Address::generate(&t.env);
    let new_leaderboard = Address::generate(&t.env);
    let new_xlm = Address::generate(&t.env);
    t.client.set_config(
        &t.admin,
        &new_token,
        &new_referral,
        &new_leaderboard,
        &new_xlm,
    );
    // Too soon — the timelock has not matured.
    t.client.execute_set_config(&t.admin);
}

#[test]
#[should_panic(expected = "Error(Contract, #29)")]
fn test_execute_set_config_without_pending_rejected() {
    let t = setup();
// ═══════════════════════════════════════════════════════════════════════════
// SECURITY REGRESSION — issue #51 (set_config pinning / governance)
// ═══════════════════════════════════════════════════════════════════════════

fn second_leaderboard(t: &TestSetup) -> Address {
    let id = t.env.register(LeaderboardContract, ());
    let client = leaderboard::LeaderboardContractClient::new(&t.env, &id);
    client.initialize(&t.admin, &t.client.address, &t.referral_client.address);
    id
}

#[test]
fn test_set_config_does_not_apply_immediately() {
    let t = setup();
    let cfg = t.client.get_config();
    let new_lb = second_leaderboard(&t);

    t.client.set_config(
        &t.admin,
        &cfg.token,
        &cfg.referral,
        &new_lb,
        &cfg.xlm_sac,
    );

    // Live config is unchanged until execute_set_config after the delay.
    assert_eq!(t.client.get_config().leaderboard, cfg.leaderboard);
    let pending = t.client.get_pending_config().expect("pending change");
    assert_eq!(pending.cfg.leaderboard, new_lb);
    assert_eq!(pending.approvers.len(), 1);
}

#[test]
#[should_panic(expected = "Error(Contract, #28)")]
fn test_set_config_rejects_arbitrary_address() {
    let t = setup();
    let cfg = t.client.get_config();
    let attacker = Address::generate(&t.env);
    t.client.set_config(
        &t.admin,
        &cfg.token,
        &attacker,
        &cfg.leaderboard,
        &cfg.xlm_sac,
    );
}

#[test]
#[should_panic(expected = "Error(Contract, #28)")]
fn test_set_config_rejects_wasm_as_xlm_sac() {
    let t = setup();
    let cfg = t.client.get_config();
    // A WASM/native contract must not be installable as the XLM SAC.
    t.client.set_config(
        &t.admin,
        &cfg.token,
        &cfg.referral,
        &cfg.leaderboard,
        &cfg.token,
    );
}

#[test]
#[should_panic(expected = "Error(Contract, #32)")]
fn test_set_config_execute_before_delay() {
    let t = setup();
    let cfg = t.client.get_config();
    let new_lb = second_leaderboard(&t);
    t.client.set_config(
        &t.admin,
        &cfg.token,
        &cfg.referral,
        &new_lb,
        &cfg.xlm_sac,
    );
    t.client.execute_set_config(&t.admin);
}

#[test]
fn test_set_config_execute_after_delay_and_pin() {
    let t = setup();
    let cfg = t.client.get_config();
    let new_lb = second_leaderboard(&t);
    t.client.set_config(
        &t.admin,
        &cfg.token,
        &cfg.referral,
        &new_lb,
        &cfg.xlm_sac,
    );
    advance_time(&t.env, CONFIG_DELAY_SECS);
    t.client.execute_set_config(&t.admin);

    assert_eq!(t.client.get_config().leaderboard, new_lb);
    assert!(t.client.get_pending_config().is_none());
    let pins = t.client.get_pinned_hashes().expect("pins");
    assert_eq!(pins.xlm_sac, BytesN::from_array(&t.env, &[0u8; 32]));
}

#[test]
fn test_cancel_set_config_during_dispute_window() {
    let t = setup();
    let cfg = t.client.get_config();
    let new_lb = second_leaderboard(&t);
    t.client.set_config(
        &t.admin,
        &cfg.token,
        &cfg.referral,
        &new_lb,
        &cfg.xlm_sac,
    );
    t.client.cancel_set_config(&t.admin);
    assert!(t.client.get_pending_config().is_none());
    assert_eq!(t.client.get_config().leaderboard, cfg.leaderboard);
}

#[test]
#[should_panic(expected = "Error(Contract, #33)")]
fn test_set_config_multisig_requires_threshold() {
    let t = setup();
    let g2 = Address::generate(&t.env);
    t.client.add_governor(&t.admin, &g2);
    t.client.set_governor_threshold(&t.admin, &2_u32);

    let cfg = t.client.get_config();
    let new_lb = second_leaderboard(&t);
    t.client.set_config(
        &t.admin,
        &cfg.token,
        &cfg.referral,
        &new_lb,
        &cfg.xlm_sac,
    );
    advance_time(&t.env, CONFIG_DELAY_SECS);
    // Only the proposer approved (1 of 2).
    t.client.execute_set_config(&t.admin);
}

#[test]
fn test_cancel_set_config_removes_pending() {
    let t = setup();
    let before = t.client.get_config();
    let new_token = Address::generate(&t.env);
    let new_referral = Address::generate(&t.env);
    let new_leaderboard = Address::generate(&t.env);
    let new_xlm = Address::generate(&t.env);
    t.client.set_config(
        &t.admin,
        &new_token,
        &new_referral,
        &new_leaderboard,
        &new_xlm,
    );
    assert!(t.client.get_pending_config().is_some());

    t.client.cancel_set_config(&t.admin);

    assert!(t.client.get_pending_config().is_none());
    assert_eq!(t.client.get_config(), before);
}

#[test]
#[should_panic(expected = "Error(Contract, #3)")]
fn test_set_config_rejects_non_admin() {
    let t = setup();
    let rando = Address::generate(&t.env);
    let new_token = Address::generate(&t.env);
    let new_referral = Address::generate(&t.env);
    let new_leaderboard = Address::generate(&t.env);
    let new_xlm = Address::generate(&t.env);
    t.client.set_config(
        &rando,
        &new_token,
        &new_referral,
        &new_leaderboard,
        &new_xlm,
    );
fn test_set_config_multisig_execute_with_second_approval() {
    let t = setup();
    let g2 = Address::generate(&t.env);
    t.client.add_governor(&t.admin, &g2);
    t.client.set_governor_threshold(&t.admin, &2_u32);

    let cfg = t.client.get_config();
    let new_lb = second_leaderboard(&t);
    t.client.set_config(
        &t.admin,
        &cfg.token,
        &cfg.referral,
        &new_lb,
        &cfg.xlm_sac,
    );
    t.client.approve_set_config(&g2);
    advance_time(&t.env, CONFIG_DELAY_SECS);
    t.client.execute_set_config(&g2);

    assert_eq!(t.client.get_config().leaderboard, new_lb);
}

#[test]
#[should_panic(expected = "Error(Contract, #18)")]
fn test_set_config_non_governor_rejected() {
    let t = setup();
    let cfg = t.client.get_config();
    let stranger = Address::generate(&t.env);
    t.client.set_config(
        &stranger,
        &cfg.token,
        &cfg.referral,
        &cfg.leaderboard,
        &cfg.xlm_sac,
    );
fn last_event_name(env: &Env) -> Symbol {
    // `env.events().all()` returns a `ContractEvents` in soroban-sdk 26, which
    // exposes its entries as an XDR slice rather than an indexable Vec of
    // (address, topics, data) tuples.
    let events = env.events().all();
    let emitted = events.events();
    assert!(!emitted.is_empty(), "no event was emitted");
    let soroban_sdk::xdr::ContractEventBody::V0(body) = &emitted.last().unwrap().body;
    let topic0 = Val::try_from_val(env, &body.topics[0]).unwrap();
    Symbol::try_from_val(env, &topic0).unwrap()
}

#[test]
fn test_create_market_emits_event() {
    let t = setup();
    let _id = create_test_market(&t);
    assert_eq!(last_event_name(&t.env), Symbol::new(&t.env, "market_created"));
}

#[test]
fn test_place_bet_emits_event() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 200_0000000);
    t.client.place_bet(&user, &id, &true, &100_0000000_i128);
    assert_eq!(last_event_name(&t.env), Symbol::new(&t.env, "bet_placed"));
}

#[test]
fn test_resolve_market_emits_event() {
    let t = setup();
    let id = create_test_market(&t);
    let alice = Address::generate(&t.env);
    let bob = Address::generate(&t.env);
    fund_user(&t, &alice, 200_0000000);
    fund_user(&t, &bob, 200_0000000);
    t.client.place_bet(&alice, &id, &true, &100_0000000_i128);
    t.client.place_bet(&bob, &id, &false, &100_0000000_i128);
    advance_time(&t.env, 3601);
    t.client.resolve_market(&t.admin, &id, &true);
    assert_eq!(last_event_name(&t.env), Symbol::new(&t.env, "market_resolved"));
}

// ═══════════════════════════════════════════════════════════════════════════
// SECURITY REGRESSION SUITE — issue #95 (pause / circuit breaker)
// ═══════════════════════════════════════════════════════════════════════════

// ── 95a. Only the admin may pause/resume ────────────────────────────────────
#[test]
#[should_panic(expected = "Error(Contract, #3)")]
fn test_pause_rejects_non_admin() {
    let t = setup();
    let rando = Address::generate(&t.env);
    t.client.set_paused(&rando, &true);
}

#[test]
#[should_panic(expected = "Error(Contract, #3)")]
fn test_resume_rejects_non_admin() {
    let t = setup();
    t.client.set_paused(&t.admin, &true);
    let rando = Address::generate(&t.env);
    t.client.set_paused(&rando, &false);
}

// ── 95. Pause blocks new exposure, settlement and withdrawals; resume works ──
#[test]
fn test_pause_blocks_place_bet_then_resume() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 1_000_0000000);

    t.client.set_paused(&t.admin, &true);
    assert!(t.client.paused());
    assert!(t.client.try_place_bet(&user, &id, &true, &100_0000000_i128).is_err());

    t.client.set_paused(&t.admin, &false);
    assert!(!t.client.paused());
    t.client.place_bet(&user, &id, &true, &100_0000000_i128); // works again
    assert_eq!(t.client.get_market(&id).total_yes, 98_0000000);
}

#[test]
fn test_pause_blocks_create_market() {
    let t = setup();
    t.client.set_paused(&t.admin, &true);
    let res = t.client.try_create_market(
        &t.admin,
        &String::from_str(&t.env, "Paused?"),
        &String::from_str(&t.env, "https://x.png"),
        &Category::Crypto,
        &3600_u64,
    );
    assert!(res.is_err()); // Paused
}

#[test]
fn test_pause_blocks_resolve_and_cancel() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    let other = Address::generate(&t.env);
    fund_user(&t, &user, 1_000_0000000);
    fund_user(&t, &other, 1_000_0000000);
    t.client.place_bet(&user, &id, &true, &100_0000000_i128);
    t.client.place_bet(&other, &id, &false, &100_0000000_i128);
    advance_time(&t.env, 3601);

    t.client.set_paused(&t.admin, &true);
    assert!(t.client.try_resolve_market(&t.admin, &id, &true).is_err());
    assert!(t.client.try_cancel_market(&t.admin, &id).is_err());
}

#[test]
fn test_pause_blocks_all_withdrawal_paths() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    let recipient = Address::generate(&t.env);
    fund_user(&t, &user, 1_000_0000000);
    t.client.place_bet(&user, &id, &true, &100_0000000_i128); // fees accrue
    t.client.add_fee_recipient(&t.admin, &recipient);

    t.client.set_paused(&t.admin, &true);
    assert!(t.client.try_withdraw_fees(&t.admin, &recipient).is_err());
    assert!(t
        .client
        .try_request_withdraw_fees(&recipient, &recipient, &1000000)
        .is_err());
    assert!(t.client.try_execute_withdraw_fees(&recipient).is_err());
}

// ── 95. Recovery paths stay OPEN while paused (no fund lock-in) ─────────────
#[test]
fn test_pause_keeps_claim_available() {
    let t = setup();
    let id = create_test_market(&t);
    let alice = Address::generate(&t.env);
    let bob = Address::generate(&t.env);
    fund_user(&t, &alice, 1_000_0000000);
    fund_user(&t, &bob, 1_000_0000000);
    t.client.place_bet(&alice, &id, &true, &100_0000000_i128);
    t.client.place_bet(&bob, &id, &false, &100_0000000_i128);
    advance_time(&t.env, 3601);
    t.client.resolve_market(&t.admin, &id, &true);

    t.client.set_paused(&t.admin, &true);
    let before = t.xlm.balance(&alice);
    t.client.claim(&alice, &id); // recovery MUST keep working while paused
    assert!(t.xlm.balance(&alice) > before);
}

#[test]
fn test_pause_keeps_cancel_refund_available() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 1_000_0000000);
    t.client.place_bet(&user, &id, &true, &100_0000000_i128);
    t.client.cancel_market(&t.admin, &id); // cancelled BEFORE the pause

    t.client.set_paused(&t.admin, &true);
    let before = t.xlm.balance(&user);
    let refunded = t.client.cancel_refund(&user, &id);
    assert_eq!(refunded, 100_0000000);
    assert_eq!(t.xlm.balance(&user), before + 100_0000000);
}

// ── 95. Repeated pause/resume is idempotent ─────────────────────────────────
#[test]
fn test_repeated_pause_resume_idempotent() {
    let t = setup();
    t.client.set_paused(&t.admin, &true);
    t.client.set_paused(&t.admin, &true);
    assert!(t.client.paused());
    t.client.set_paused(&t.admin, &false);
    t.client.set_paused(&t.admin, &false);
    assert!(!t.client.paused());
}

// ═══════════════════════════════════════════════════════════════════════════
// SECURITY REGRESSION SUITE — issue #4 (fee provenance / withdraw gating)
// ═══════════════════════════════════════════════════════════════════════════

// ── #4: cancelling market A must NOT erase market B's fees ────────────────
#[test]
fn test_cancel_preserves_unrelated_market_fees() {
    let t = setup();
    let id_a = create_test_market(&t);
    let id_b = create_test_market(&t);
    let user_a = Address::generate(&t.env);
    let user_b = Address::generate(&t.env);
    fund_user(&t, &user_a, 200_0000000);
    fund_user(&t, &user_b, 200_0000000);

    t.client.place_bet(&user_a, &id_a, &true, &100_0000000_i128); // 2% fee
    t.client.place_bet(&user_b, &id_b, &true, &50_0000000_i128); // 2% fee
    assert_eq!(t.client.get_accumulated_fees(), 3_0000000);

    t.client.cancel_market(&t.admin, &id_a);

    // ONLY market A's fee share leaves the accumulator.
    assert_eq!(t.client.get_accumulated_fees(), 1_0000000);
    assert_eq!(t.client.get_market_fee_ledger(&id_a), 0);
    assert_eq!(t.client.get_open_fees(), 1_0000000);

    // Full refund for A's user.
    assert_eq!(t.client.cancel_refund(&user_a, &id_a), 100_0000000);

    // Market B is untouched and can still resolve/withdraw normally.
    advance_time(&t.env, 3601);
    t.client.resolve_market(&t.admin, &id_b, &true);
    let withdrawn = t.client.withdraw_fees(&t.admin, &t.admin);
    assert_eq!(withdrawn, 1_0000000);
}

// ── #4: exact stroop-level fee accounting (amount not a multiple of 50) ──────
#[test]
fn test_stroop_exact_fee_accounting() {
    let t = setup();
    let id = create_test_market(&t);
    let user = Address::generate(&t.env);
    fund_user(&t, &user, 1_000_0000000);

    let amount: i128 = 10_000_001; // odd stroop count — exercises floor/ceil
    t.client.place_bet(&user, &id, &true, &amount);

    let market = t.client.get_market(&id);
    let fees = t.client.get_accumulated_fees();
    // net + fees == amount ALWAYS — no stroop can get stranded.
    assert_eq!(market.total_yes + fees, amount);
    assert_eq!(t.client.get_bet_gross(&id, &user), amount);
}

// ── #4: withdrawal BEFORE cancellation is blocked while open; after refunds ──
//      everything reconciles.
#[test]
fn test_withdraw_rejected_while_open_then_cancel_refunds_work() {
    let t = setup();
    let id = create_test_market(&t);
    let alice = Address::generate(&t.env);
    let bob = Address::generate(&t.env);
    fund_user(&t, &alice, 200_0000000);
    fund_user(&t, &bob, 200_0000000);

    t.client.place_bet(&alice, &id, &true, &100_0000000_i128);
    t.client.place_bet(&bob, &id, &false, &100_0000000_i128);

    // Fees exist but are reserved for a possible cancellation -> no withdraw.
    assert!(t.client.get_accumulated_fees() > 0);
    let res = t.client.try_withdraw_fees(&t.admin, &t.admin);
    assert!(res.is_err());

    // Cancellation refunds must still be fully payable.
    t.client.cancel_market(&t.admin, &id);
    assert_eq!(t.client.cancel_refund(&alice, &id), 100_0000000);
    assert_eq!(t.client.cancel_refund(&bob, &id), 100_0000000);
    assert_eq!(t.client.get_accumulated_fees(), 0);
}

// ── #4: resolving a market must NOT turn user principal into withdrawable fees ─
#[test]
fn test_resolve_does_not_make_principal_withdrawable() {
    let t = setup();
    let id = create_test_market(&t);
    let alice = Address::generate(&t.env);
    let bob = Address::generate(&t.env);
    fund_user(&t, &alice, 500_0000000);
    fund_user(&t, &bob, 500_0000000);

    // Both sides funded: 100 XLM YES + 100 XLM NO \u2192 196 XLM of user principal.
    t.client.place_bet(&alice, &id, &true, &100_0000000_i128);
    t.client.place_bet(&bob, &id, &false, &100_0000000_i128);
    assert_eq!(t.client.get_accumulated_fees(), 4_0000000);

    // Resolve to YES (winning side non-empty) \u2014 principal must stay in the
    // contract for claims and must NOT become withdrawable as fees.
    advance_time(&t.env, 3601);
    t.client.resolve_market(&t.admin, &id, &true);

    let market = t.client.get_market(&id);
    let principal: i128 = market.total_yes + market.total_no; // 196_0000000
    let contract = t.client.address.clone();
    let contract_before = t.xlm.balance(&contract);

    let withdrawn = t.client.withdraw_fees(&t.admin, &t.admin);
    // Only the earned platform fees (4 XLM) are withdrawable \u2014 not the pool.
    assert_eq!(withdrawn, 4_0000000);
    assert_eq!(t.client.get_accumulated_fees(), 0);
    // The user principal is untouched and still sits in the contract.
    assert_eq!(t.xlm.balance(&contract), contract_before - 4_0000000);
    assert_eq!(t.xlm.balance(&contract), principal);
}

// ── #4: empty-side sweep is fully accounted \u2014 the invariant holds ────────────
// The empty-side sweep-to-fees is the protocol's pre-existing design (issue #3,
// tracked separately). #4 guarantees the accounting invariant: what is
// withdrawable is EXACTLY AccumulatedFees \u2212 OpenFees, with no double-counting.
#[test]
fn test_empty_side_sweep_withdrawable_is_fully_accounted() {
    let t = setup();
    let id = create_test_market(&t);
    let alice = Address::generate(&t.env);
    fund_user(&t, &alice, 500_0000000);
    t.client.place_bet(&alice, &id, &true, &100_0000000_i128);
    assert_eq!(t.client.get_accumulated_fees(), 2_0000000);
    assert_eq!(t.client.get_open_fees(), 2_0000000);

    advance_time(&t.env, 3601);
    t.client.resolve_market(&t.admin, &id, &false); // empty winning side

    // After settlement the fee ledger is earned (open fees released).
    assert_eq!(t.client.get_open_fees(), 0);
    let market = t.client.get_market(&id);
    let swept_pool: i128 = market.total_yes; // 98_0000000
    // withdrawable == AccumulatedFees \u2212 OpenFees == fee + swept pool (protocol).
    assert_eq!(t.client.get_accumulated_fees(), 2_0000000 + swept_pool);
    let withdrawn = t.client.withdraw_fees(&t.admin, &t.admin);
    assert_eq!(withdrawn, 2_0000000 + swept_pool);
    assert_eq!(t.client.get_accumulated_fees(), 0);
}

// ── #4: multi-market withdrawal regression ───────────────────────────────────
#[test]
fn test_multi_market_withdrawal_regression() {
    let t = setup();
    let id_a = create_test_market(&t);
    let id_b = create_test_market(&t);
    let user_a = Address::generate(&t.env);
    let user_b = Address::generate(&t.env);
    fund_user(&t, &user_a, 500_0000000);
    fund_user(&t, &user_b, 500_0000000);

    t.client.place_bet(&user_a, &id_a, &true, &100_0000000_i128); // fee 2 XLM
    t.client.place_bet(&user_b, &id_b, &true, &50_0000000_i128); // fee 1 XLM
    assert_eq!(t.client.get_accumulated_fees(), 3_0000000);
    assert_eq!(t.client.get_open_fees(), 3_0000000);

    // While either market is open, nothing is withdrawable.
    let res = t.client.try_withdraw_fees(&t.admin, &t.admin);
    assert!(res.is_err());

    // Cancel A: releases ONLY A's fee share (2 XLM); B's ledger untouched.
    t.client.cancel_market(&t.admin, &id_a);
    assert_eq!(t.client.get_accumulated_fees(), 1_0000000);
    assert_eq!(t.client.get_open_fees(), 1_0000000);
    assert_eq!(t.client.get_market_fee_ledger(&id_a), 0);
    assert_eq!(t.client.get_market_fee_ledger(&id_b), 1_0000000);

    // B is still open \u2192 its fees are reserved, still nothing withdrawable.
    let res = t.client.try_withdraw_fees(&t.admin, &t.admin);
    assert!(res.is_err());

    // Refund A's bettor in full.
    assert_eq!(t.client.cancel_refund(&user_a, &id_a), 100_0000000);

    // Resolve B \u2192 its fee becomes earned and is the ONLY withdrawable amount.
    advance_time(&t.env, 3601);
    t.client.resolve_market(&t.admin, &id_b, &true);
    assert_eq!(t.client.get_open_fees(), 0);
    let withdrawn = t.client.withdraw_fees(&t.admin, &t.admin);
    assert_eq!(withdrawn, 1_0000000);
    assert_eq!(t.client.get_accumulated_fees(), 0);
}

// ═══════════════════════════════════════════════════════════════════════════
// SECURITY REGRESSION SUITE — issue #6 (config governance / code-hash pinning)
// ═══════════════════════════════════════════════════════════════════════════

// ── 96. WasmHashMismatch: execute_set_config rejects when dependency swaps WASM during delay ──
#[test]
#[should_panic(expected = "Error(Contract, #29)")]
fn test_execute_set_config_rejects_wasm_hash_mismatch() {
    let t = setup();
    let cfg = t.client.get_config();
    let new_lb = second_leaderboard(&t);
    t.client.set_config(
        &t.admin,
        &cfg.token,
        &cfg.referral,
        &new_lb,
        &cfg.xlm_sac,
    );
    advance_time(&t.env, CONFIG_DELAY_SECS);
    let hashes = t.client.get_pinned_hashes();
    assert!(hashes.is_some());
}

// ── 97. approve_set_config: multi-sig threshold enforcement ──────────────────────
#[test]
#[should_panic(expected = "Error(Contract, #34)")]
fn test_approve_set_config_rejects_double_approval() {
    let t = setup();
    let cfg = t.client.get_config();
    let new_lb = second_leaderboard(&t);
    t.client.set_config(
        &t.admin,
        &cfg.token,
        &cfg.referral,
        &new_lb,
        &cfg.xlm_sac,
    );
    t.client.approve_set_config(&t.admin);
}

// ── 98. set_config rejects when a proposal already exists ────────────────────────
#[test]
#[should_panic(expected = "Error(Contract, #30)")]
fn test_set_config_rejects_duplicate_proposal() {
    let t = setup();
    let cfg = t.client.get_config();
    let new_lb = second_leaderboard(&t);
    t.client.set_config(
        &t.admin,
        &cfg.token,
        &cfg.referral,
        &new_lb,
        &cfg.xlm_sac,
    );
    t.client.set_config(
        &t.admin,
        &cfg.token,
        &cfg.referral,
        &new_lb,
        &cfg.xlm_sac,
    );
}

// ── 99. Non-governor cannot propose config change ────────────────────────────────
#[test]
#[should_panic(expected = "Error(Contract, #3)")]
fn test_set_config_rejects_non_governor() {
    let t = setup();
    let cfg = t.client.get_config();
    let stranger = Address::generate(&t.env);
    let new_lb = second_leaderboard(&t);
    t.client.set_config(
        &stranger,
        &cfg.token,
        &cfg.referral,
        &new_lb,
        &cfg.xlm_sac,
    );
}

// ── 100. Full governance flow: propose \u2192 approve \u2192 execute (happy path) ──────────
#[test]
fn test_config_governance_full_happy_path() {
    let t = setup();
    let before = t.client.get_config();
    let new_lb = second_leaderboard(&t);

    t.client.set_config(
        &t.admin,
        &before.token,
        &before.referral,
        &new_lb,
        &before.xlm_sac,
    );
    assert!(t.client.get_pending_config().is_some());
    let hashes = t.client.get_pinned_hashes();
    assert!(hashes.is_some());

    advance_time(&t.env, CONFIG_DELAY_SECS);

    t.client.execute_set_config(&t.admin);

    assert!(t.client.get_pending_config().is_none());
    assert_eq!(t.client.get_config().leaderboard, new_lb);
    let new_hashes = t.client.get_pinned_hashes().unwrap();
    assert_eq!(new_hashes.leaderboard, hashes.unwrap().leaderboard);
}
}
