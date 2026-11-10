//! Fixed-point math for exponential calculations
//!
//! Scale factor: 10^18 (WAD precision)
//! Uses range reduction and polynomial approximation for exp()

use crate::error::StakingError;
use uint::construct_uint;

construct_uint! {
    /// 256-bit unsigned integer for large intermediate values
    pub struct U256(4);
}

/// Scale factor: 10^18 (WAD)
pub const WAD: u128 = 1_000_000_000_000_000_000;
pub const WAD_U256: U256 = U256([WAD as u64, (WAD >> 64) as u64, 0, 0]);

/// ln(2) scaled by WAD: 0.693147180559945309...
pub const LN2_WAD: u128 = 693_147_180_559_945_309;

/// 1/ln(2) scaled by WAD: 1.442695040888963407...
pub const INV_LN2_WAD: u128 = 1_442_695_040_888_963_407;

/// e scaled by WAD: 2.718281828459045235...
pub const E_WAD: u128 = 2_718_281_828_459_045_235;

/// Maximum safe exponent input (scaled by WAD) to avoid overflow.
/// exp_wad overflows its u128 intermediate (2^int_part * WAD) around x ≈ 48 WAD,
/// so we cap at 42 WAD (matching EXP_NEG_ZERO_THRESHOLD) which is well within safe range.
pub const MAX_EXP_INPUT: u128 = 42_000_000_000_000_000_000;

/// Maximum (now - base_time) / tau ratio (scaled by WAD) at which NEW stake is
/// accepted (Stake / StakeOnBehalf); above it the caller must run the
/// permissionless SyncPool crank first.
///
/// Deliberately far below MAX_EXP_INPUT. A stake's weight is computed as
/// `amount * (WAD - exp_neg(ratio) * exp_start_factor / WAD)`, where
/// exp_start_factor ~ e^ratio * WAD is huge and exp_neg(ratio) ~ e^-ratio * WAD
/// has only a handful of significant digits left as the ratio approaches 42
/// (and is exactly 0 from ~41.45). The truncation overstates the weight of a
/// stake made at a high ratio: ~1% instant weight at ratio 38, ~22% at 40.5,
/// 100% at >= 41.5 (ideal: 0%), and the next rebase makes that permanent.
/// At ratio 20, exp_neg still carries ~9 significant digits, bounding the
/// instant-weight error to ~1e-9 of the staked amount.
///
/// Only the stake-time gate uses this; claim/unstake paths keep MAX_EXP_INPUT.
pub const MAX_STAKE_EXP_INPUT: u128 = 20_000_000_000_000_000_000;

/// Threshold for sum_stake_exp to trigger rebase (near U256 max / 2)
pub const REBASE_THRESHOLD: U256 = U256([u64::MAX / 2, u64::MAX, u64::MAX, u64::MAX / 2]);

impl U256 {
    /// Create U256 from u128
    pub const fn from_u128(val: u128) -> Self {
        U256([val as u64, (val >> 64) as u64, 0, 0])
    }

    /// Convert to u128, returning None if overflow
    pub fn to_u128(&self) -> Option<u128> {
        if self.0[2] != 0 || self.0[3] != 0 {
            return None;
        }
        Some((self.0[1] as u128) << 64 | self.0[0] as u128)
    }

    /// Convert to [u8; 32] for storage
    pub fn to_le_bytes(&self) -> [u8; 32] {
        let mut bytes = [0u8; 32];
        bytes[0..8].copy_from_slice(&self.0[0].to_le_bytes());
        bytes[8..16].copy_from_slice(&self.0[1].to_le_bytes());
        bytes[16..24].copy_from_slice(&self.0[2].to_le_bytes());
        bytes[24..32].copy_from_slice(&self.0[3].to_le_bytes());
        bytes
    }

    /// Create from [u8; 32] storage
    pub fn from_le_bytes(bytes: &[u8; 32]) -> Self {
        let w0 = u64::from_le_bytes(bytes[0..8].try_into().unwrap());
        let w1 = u64::from_le_bytes(bytes[8..16].try_into().unwrap());
        let w2 = u64::from_le_bytes(bytes[16..24].try_into().unwrap());
        let w3 = u64::from_le_bytes(bytes[24..32].try_into().unwrap());
        U256([w0, w1, w2, w3])
    }

    /// Check if value exceeds rebase threshold
    pub fn needs_rebase(&self) -> bool {
        *self > REBASE_THRESHOLD
    }
}

/// Multiply two WAD-scaled values, returning WAD-scaled result
pub fn wad_mul(a: u128, b: u128) -> Result<u128, StakingError> {
    let result = U256::from_u128(a)
        .checked_mul(U256::from_u128(b))
        .ok_or(StakingError::MathOverflow)?
        / WAD_U256;
    result.to_u128().ok_or(StakingError::MathOverflow)
}

/// Divide two WAD-scaled values, returning WAD-scaled result
pub fn wad_div(a: u128, b: u128) -> Result<u128, StakingError> {
    if b == 0 {
        return Err(StakingError::MathOverflow);
    }
    let result = U256::from_u128(a)
        .checked_mul(WAD_U256)
        .ok_or(StakingError::MathOverflow)?
        / U256::from_u128(b);
    result.to_u128().ok_or(StakingError::MathOverflow)
}

/// U256 version of wad_mul
pub fn wad_mul_u256(a: U256, b: U256) -> Result<U256, StakingError> {
    a.checked_mul(b)
        .ok_or(StakingError::MathOverflow)?
        .checked_div(WAD_U256)
        .ok_or(StakingError::MathOverflow)
}

/// Calculate e^x where x is WAD-scaled (x = actual_value * WAD)
/// Uses range reduction: e^x = 2^(x/ln(2)) = 2^n * 2^f
/// where n is integer part and f is fractional part
///
/// Returns WAD-scaled result
pub fn exp_wad(x: u128) -> Result<u128, StakingError> {
    if x == 0 {
        return Ok(WAD);
    }

    if x > MAX_EXP_INPUT {
        return Err(StakingError::MathOverflow);
    }

    // Convert x to base-2 exponent: x / ln(2)
    // x_div_ln2 = x * (1/ln2) / WAD
    let x_div_ln2 = wad_mul(x, INV_LN2_WAD)?;

    // Split into integer and fractional parts
    let int_part = x_div_ln2 / WAD;
    let frac_part = x_div_ln2 % WAD; // Already WAD-scaled fractional part

    // Calculate 2^frac using Taylor series for 2^x = e^(x*ln2)
    // 2^f = 1 + f*ln2 + (f*ln2)^2/2! + (f*ln2)^3/3! + ...
    let f_ln2 = wad_mul(frac_part, LN2_WAD)?;
    let two_pow_frac = exp_taylor(f_ln2)?;

    // Calculate 2^int by shifting (careful with overflow)
    if int_part > 127 {
        return Err(StakingError::MathOverflow);
    }

    // 2^int_part * two_pow_frac / WAD
    let two_pow_int = 1u128 << int_part;
    wad_mul(two_pow_int.checked_mul(WAD).ok_or(StakingError::MathOverflow)?, two_pow_frac)
}

/// Taylor series approximation for e^x where x is small (|x| < ln(2))
/// e^x = 1 + x + x^2/2! + x^3/3! + x^4/4! + x^5/5! + x^6/6!
/// x is WAD-scaled, returns WAD-scaled result
fn exp_taylor(x: u128) -> Result<u128, StakingError> {
    // Precomputed 1/n! values scaled by WAD
    const INV_FACTORIAL: [u128; 7] = [
        WAD,                           // 1/0! = 1
        WAD,                           // 1/1! = 1
        500_000_000_000_000_000,       // 1/2! = 0.5
        166_666_666_666_666_667,       // 1/3! ≈ 0.1667
        41_666_666_666_666_667,        // 1/4! ≈ 0.0417
        8_333_333_333_333_333,         // 1/5! ≈ 0.00833
        1_388_888_888_888_889,         // 1/6! ≈ 0.00139
    ];

    let mut result = WAD; // Start with 1
    let mut x_pow = x;    // x^1

    for i in 1..=6 {
        let term = wad_mul(x_pow, INV_FACTORIAL[i])?;
        result = result.checked_add(term).ok_or(StakingError::MathOverflow)?;
        if i < 6 {
            x_pow = wad_mul(x_pow, x)?;
        }
    }

    Ok(result)
}

/// Threshold above which e^(-x) rounds to 0 at WAD precision.
/// e^(-42) ≈ 5.75e-19, which is < 1/WAD, so WAD * e^(-42) < 1 and truncates to 0.
/// This also avoids calling exp_wad with values that overflow its u128 intermediates
/// (exp_wad overflows around x ≈ 48 WAD due to `2^int_part * WAD` exceeding u128).
pub const EXP_NEG_ZERO_THRESHOLD: u128 = 42_000_000_000_000_000_000; // 42 * WAD

/// Calculate e^(-x) where x is WAD-scaled
/// Uses e^(-x) = 1/e^x
/// For large x (>= EXP_NEG_ZERO_THRESHOLD), returns 0 since the result is below WAD precision
pub fn exp_neg_wad(x: u128) -> Result<u128, StakingError> {
    if x == 0 {
        return Ok(WAD);
    }

    // For large x, e^(-x) rounds to 0 at WAD (10^18) precision
    if x >= EXP_NEG_ZERO_THRESHOLD {
        return Ok(0);
    }

    let exp_x = exp_wad(x)?;
    wad_div(WAD, exp_x)
}

/// Calculate e^(t/tau) where t is time in seconds, tau is time constant in seconds
/// Returns WAD-scaled result
pub fn exp_time_ratio(t: i64, tau: u64) -> Result<u128, StakingError> {
    if t <= 0 {
        return Ok(WAD);
    }
    if tau == 0 {
        return Err(StakingError::InvalidTau);
    }

    // Calculate t/tau scaled by WAD
    // t_ratio = (t * WAD) / tau
    let t_wad = (t as u128).checked_mul(WAD).ok_or(StakingError::MathOverflow)?;
    let ratio = t_wad / (tau as u128);

    exp_wad(ratio)
}

/// Calculate e^(-t/tau) where t is time in seconds, tau is time constant in seconds
/// Returns WAD-scaled result
pub fn exp_neg_time_ratio(t: i64, tau: u64) -> Result<u128, StakingError> {
    if t <= 0 {
        return Ok(WAD);
    }
    if tau == 0 {
        return Err(StakingError::InvalidTau);
    }

    let t_wad = (t as u128).checked_mul(WAD).ok_or(StakingError::MathOverflow)?;
    let ratio = t_wad / (tau as u128);

    exp_neg_wad(ratio)
}

/// Calculate weight = amount * (1 - e^(-age/tau))
/// Returns WAD-scaled weight
pub fn calculate_weight(amount: u64, age_seconds: i64, tau: u64) -> Result<u128, StakingError> {
    if age_seconds <= 0 || amount == 0 {
        return Ok(0);
    }

    let exp_neg = exp_neg_time_ratio(age_seconds, tau)?;
    let one_minus_exp = WAD.checked_sub(exp_neg).ok_or(StakingError::MathUnderflow)?;

    // weight = amount * (1 - exp_neg)
    wad_mul((amount as u128).checked_mul(WAD).ok_or(StakingError::MathOverflow)?, one_minus_exp)
}

/// Calculate total weighted stake at time t
/// total_weighted = total_staked - exp(-t/tau) * sum_stake_exp
/// Both inputs should be in their native units (not WAD scaled for amounts)
pub fn calculate_total_weighted_stake(
    total_staked: u128,
    sum_stake_exp: &U256,
    current_time: i64,
    base_time: i64,
    tau: u64,
) -> Result<u128, StakingError> {
    if total_staked == 0 {
        return Ok(0);
    }

    // Calculate exp(-(current_time - base_time) / tau)
    let age = current_time.saturating_sub(base_time);
    let exp_neg = exp_neg_time_ratio(age, tau)?;

    // exp_neg * sum_stake_exp / WAD
    let exp_neg_u256 = U256::from_u128(exp_neg);
    let decay_term = wad_mul_u256(*sum_stake_exp, exp_neg_u256)?;

    // total_staked * WAD - decay_term
    // Use saturating_sub to handle accumulated rounding drift between
    // pool-level and user-level wad_mul operations after rebases.
    let total_staked_wad = U256::from_u128(total_staked)
        .checked_mul(WAD_U256)
        .ok_or(StakingError::MathOverflow)?;

    let weighted = total_staked_wad.saturating_sub(decay_term);

    // Convert back from U256 to u128
    weighted.to_u128().ok_or(StakingError::MathOverflow)
}

/// Calculate user's weighted stake
pub fn calculate_user_weighted_stake(
    amount: u64,
    exp_start_factor: u128,
    current_time: i64,
    base_time: i64,
    tau: u64,
) -> Result<u128, StakingError> {
    if amount == 0 {
        return Ok(0);
    }

    // User weight = amount - exp(-t/tau) * amount * exp(start_time/tau)
    //             = amount - exp(-(t - start_time)/tau) * amount
    //             = amount * (1 - exp(-age/tau))
    // But we track exp_start_factor = exp((start_time - base_time)/tau)
    // So: weight = amount * WAD - exp(-current_t/tau) * amount * exp_start_factor
    //            = amount * (WAD - exp(-(current_time - base_time)/tau) * exp_start_factor / WAD)

    let age = current_time.saturating_sub(base_time);
    let exp_neg_current = exp_neg_time_ratio(age, tau)?;

    // decay = exp_neg_current * exp_start_factor / WAD
    let decay = wad_mul(exp_neg_current, exp_start_factor)?;

    // weight = amount * (WAD - decay)
    let weight_factor = WAD.checked_sub(decay).ok_or(StakingError::MathUnderflow)?;
    wad_mul((amount as u128).checked_mul(WAD).ok_or(StakingError::MathOverflow)?, weight_factor)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_exp_zero() {
        assert_eq!(exp_wad(0).unwrap(), WAD);
    }

    #[test]
    fn test_exp_one() {
        let result = exp_wad(WAD).unwrap();
        // e^1 ≈ 2.718281828...
        let expected = E_WAD;
        let diff = if result > expected {
            result - expected
        } else {
            expected - result
        };
        // Allow 0.01% error
        assert!(diff < expected / 10000, "exp(1) = {} vs expected {}", result, expected);
    }

    #[test]
    fn test_exp_neg() {
        let result = exp_neg_wad(WAD).unwrap();
        // e^(-1) ≈ 0.367879441...
        let expected = 367_879_441_171_442_322u128;
        let diff = if result > expected {
            result - expected
        } else {
            expected - result
        };
        assert!(diff < expected / 10000, "exp(-1) = {} vs expected {}", result, expected);
    }

    #[test]
    fn test_weight_at_tau() {
        // At age = tau, weight should be about 63.2% of max
        let tau = 2_592_000u64; // 30 days
        let amount = 1_000_000u64;
        let weight = calculate_weight(amount, tau as i64, tau).unwrap();
        let max_weight = (amount as u128) * WAD;
        let ratio = weight * 100 / max_weight;
        // Should be ~63%
        assert!(ratio >= 62 && ratio <= 64, "Weight at tau = {}%", ratio);
    }

    #[test]
    fn test_u128_min_before_truncation() {
        // Verify that taking min() in u128 space before truncating to u64 is correct.
        // Bug scenario: pending_lamports overflows u64, truncation discards high bits.
        let pending_lamports: u128 = (u64::MAX as u128) + 1_000_000; // > u64::MAX
        let available_rewards: u64 = 500_000_000; // 0.5 SOL

        // CORRECT: min in u128 space, then truncate
        let correct = pending_lamports.min(available_rewards as u128) as u64;
        assert_eq!(correct, available_rewards);

        // INCORRECT (old bug): truncate first, then min
        let buggy = (pending_lamports as u64).min(available_rewards);
        // pending_lamports as u64 wraps around, giving a small number
        // which is less than available_rewards, so user gets less than they should
        assert_ne!(buggy, available_rewards, "truncation before min gives wrong result");

        // Also verify when pending_lamports fits in u64
        let small_pending: u128 = 100_000;
        let large_available: u64 = 500_000_000;
        let result = small_pending.min(large_available as u128) as u64;
        assert_eq!(result, 100_000);
    }

    #[test]
    fn test_exp_neg_large_input_returns_zero() {
        // e^(-100) is effectively 0 at WAD precision
        let result = exp_neg_wad(100 * WAD).unwrap();
        assert_eq!(result, 0);
    }

    #[test]
    fn test_exp_neg_at_threshold_boundary() {
        // Just below threshold should still compute (result is 0 or 1)
        let result = exp_neg_wad(EXP_NEG_ZERO_THRESHOLD - WAD).unwrap();
        // e^(-41) ≈ 1.56e-18, which at WAD scale is ~1-2
        assert!(result <= 2, "exp_neg(-41) should be 0 or 1 at WAD precision, got {}", result);
    }

    #[test]
    fn test_exp_neg_at_threshold() {
        // At threshold should return 0
        let result = exp_neg_wad(EXP_NEG_ZERO_THRESHOLD).unwrap();
        assert_eq!(result, 0);
    }

    #[test]
    fn test_exp_neg_just_above_threshold() {
        // Above threshold should return 0 (not error)
        let result = exp_neg_wad(EXP_NEG_ZERO_THRESHOLD + WAD).unwrap();
        assert_eq!(result, 0);
    }

    #[test]
    fn test_exp_neg_time_ratio_large_age() {
        // Pool running for 100*tau should not fail
        let tau = 2_592_000u64; // 30 days
        let age = 100 * tau as i64;
        let result = exp_neg_time_ratio(age, tau).unwrap();
        assert_eq!(result, 0);
    }

    #[test]
    fn test_calculate_weight_large_age() {
        // Weight should be fully matured (amount * WAD) for very old stakes
        let tau = 2_592_000u64;
        let amount = 1_000_000u64;
        let age = 100 * tau as i64;
        let weight = calculate_weight(amount, age, tau).unwrap();
        let max_weight = (amount as u128) * WAD;
        assert_eq!(weight, max_weight, "Weight should be fully matured for age >> tau");
    }

    #[test]
    fn test_u256_roundtrip() {
        let val = U256::from_u128(123456789012345678901234567890u128);
        let bytes = val.to_le_bytes();
        let restored = U256::from_le_bytes(&bytes);
        assert_eq!(val, restored);
    }

    // --- Property / invariant tests (audit-recommended) ---

    #[test]
    fn test_weight_monotonicity() {
        // Weight must never decrease as age increases
        let tau = 86_400u64; // 1 day
        let amount = 1_000_000u64;
        let mut prev = 0u128;
        for age in (0..=5 * tau as i64).step_by(3600) {
            let w = calculate_weight(amount, age, tau).unwrap();
            assert!(w >= prev, "weight decreased at age={}: {} < {}", age, w, prev);
            prev = w;
        }
    }

    #[test]
    fn test_rebase_invariance() {
        // calculate_total_weighted_stake must give the same answer before and
        // after a simulated rebase (shifting base_time forward and scaling
        // sum_stake_exp by exp(-delta/tau)).
        let tau = 86_400u64;
        let total_staked = 5_000u128;
        let base_time = 1_000_000i64;
        let current_time = base_time + 50_000;

        // Build sum_stake_exp: 3 users staking at different times
        let stakes: [(u64, i64); 3] = [
            (1_000, base_time + 1_000),
            (2_000, base_time + 10_000),
            (2_000, base_time + 20_000),
        ];
        let mut sum_exp = U256::zero();
        for (amt, start) in &stakes {
            let ratio = exp_time_ratio(*start - base_time, tau).unwrap();
            let contribution = U256::from_u128(*amt as u128)
                .checked_mul(U256::from_u128(ratio))
                .unwrap();
            sum_exp = sum_exp + contribution;
        }

        let w_before = calculate_total_weighted_stake(
            total_staked, &sum_exp, current_time, base_time, tau,
        )
        .unwrap();

        // Simulate rebase: shift base_time by +30_000s
        let delta = 30_000i64;
        let new_base = base_time + delta;
        let scale = exp_neg_time_ratio(delta, tau).unwrap();
        let new_sum_exp = wad_mul_u256(sum_exp, U256::from_u128(scale)).unwrap();

        let w_after = calculate_total_weighted_stake(
            total_staked, &new_sum_exp, current_time, new_base, tau,
        )
        .unwrap();

        // Allow 1 WAD of rounding error (< 1 lamport)
        let diff = if w_before > w_after {
            w_before - w_after
        } else {
            w_after - w_before
        };
        assert!(
            diff <= WAD,
            "rebase changed weighted stake: before={}, after={}, diff={}",
            w_before, w_after, diff
        );
    }

    #[test]
    fn test_user_weighted_stake_bounded() {
        // User weighted stake must always be <= amount * WAD
        let tau = 86_400u64;
        let base_time = 0i64;
        for amount in [1u64, 100, 1_000_000, u32::MAX as u64] {
            let exp_sf = WAD; // staked at base_time
            for age in [0i64, 1, 3600, 86_400, 864_000, 8_640_000] {
                let w = calculate_user_weighted_stake(
                    amount, exp_sf, age, base_time, tau,
                )
                .unwrap();
                let max = (amount as u128) * WAD;
                assert!(
                    w <= max,
                    "weighted {} > max {} for amount={}, age={}",
                    w, max, amount, age
                );
            }
        }
    }

    #[test]
    fn test_claimed_rewards_frequency_independence() {
        // Claiming N times must yield the same total as claiming once at the end.
        // Setup: user stakes `amount` at time 0 (base_time=0, tau=86400).
        // acc_rps increments through [1, 2, 3, 4, 5] * WAD at ages [1d, 2d, 3d, 4d, 5d].
        let tau = 86_400u64;
        let amount = 1_000u64;
        let base_time = 0i64;
        let exp_sf = WAD; // staked at base_time
        let snapshot_rps = 0u128; // reward_debt encodes snapshot = 0

        let steps: [(i64, u128); 5] = [
            (86_400, 1 * WAD),
            (2 * 86_400, 2 * WAD),
            (3 * 86_400, 3 * WAD),
            (4 * 86_400, 4 * WAD),
            (5 * 86_400, 5 * WAD),
        ];

        // Multi-claim: claim at each step
        let mut claimed_wad = 0u128;
        let mut total_lamports_multi = 0u64;
        for &(t, acc_rps) in &steps {
            let w = calculate_user_weighted_stake(amount, exp_sf, t, base_time, tau).unwrap();
            let delta_rps = acc_rps - snapshot_rps;
            let full_ent = wad_mul(w, delta_rps).unwrap();
            let pending = full_ent.saturating_sub(claimed_wad);
            let lam = (pending / WAD) as u64;
            total_lamports_multi += lam;
            claimed_wad += pending;
        }

        // Single claim at the end
        let &(t_final, acc_rps_final) = steps.last().unwrap();
        let w_final =
            calculate_user_weighted_stake(amount, exp_sf, t_final, base_time, tau).unwrap();
        let full_ent_single = wad_mul(w_final, acc_rps_final - snapshot_rps).unwrap();
        let total_lamports_single = (full_ent_single / WAD) as u64;

        // The multi-claim total may differ by at most N-1 lamports due to
        // per-step floor division (each `/WAD` can lose up to 1 lamport).
        let diff = if total_lamports_multi > total_lamports_single {
            total_lamports_multi - total_lamports_single
        } else {
            total_lamports_single - total_lamports_multi
        };
        assert!(
            diff <= (steps.len() as u64),
            "frequency dependence: multi={} vs single={}, diff={}",
            total_lamports_multi, total_lamports_single, diff
        );
    }

    #[test]
    fn test_reward_conservation() {
        // Total claimed rewards across all users must not exceed total deposited
        // rewards, within rounding tolerance.
        // Scenario: 2 users each stake 1000 tokens. Pool receives 10 SOL rewards.
        let tau = 86_400u64;
        let base_time = 0i64;
        let total_staked = 2_000u128;
        let reward_deposit = 10_000_000_000u64; // 10 SOL in lamports

        // Both users stake at base_time
        let amount = 1_000u64;
        let exp_sf = WAD;

        // After full maturity (age >> tau), each user has weight = amount * WAD
        let age = 100 * tau as i64;

        // acc_rps = reward_deposit * WAD / total_weighted
        // total_weighted = total_staked * WAD (fully mature)
        let total_weighted = total_staked * WAD;
        let acc_rps = wad_div(
            (reward_deposit as u128) * WAD,
            total_weighted,
        )
        .unwrap();

        // Each user's claim
        let w = calculate_user_weighted_stake(amount, exp_sf, age, base_time, tau).unwrap();
        let full_ent = wad_mul(w, acc_rps).unwrap();
        let per_user_lamports = (full_ent / WAD) as u64;
        let total_claimed = per_user_lamports * 2;

        // Conservation: total claimed <= deposited, within 2 lamport rounding
        assert!(
            total_claimed <= reward_deposit + 2,
            "over-payment: claimed {} > deposited {}",
            total_claimed, reward_deposit
        );
        assert!(
            total_claimed + 2 >= reward_deposit,
            "under-payment: claimed {} << deposited {}",
            total_claimed, reward_deposit
        );
    }

    #[test]
    fn test_exp_start_factor_weighted_average() {
        // When a user adds more stake, the new exp_start_factor must be a
        // weighted average that preserves each deposit's contribution.
        let tau = 86_400u64;
        let base_time = 0i64;

        // First deposit: 1000 tokens at time 10_000
        let amt1 = 1_000u64;
        let t1 = 10_000i64;
        let esf1 = exp_time_ratio(t1 - base_time, tau).unwrap();

        // Second deposit: 500 tokens at time 50_000
        let amt2 = 500u64;
        let t2 = 50_000i64;
        let esf2 = exp_time_ratio(t2 - base_time, tau).unwrap();

        // Combined weighted average: (amt1 * esf1 + amt2 * esf2) / (amt1 + amt2)
        let total_amt = amt1 + amt2;
        let numerator = (amt1 as u128) * esf1 + (amt2 as u128) * esf2;
        let combined_esf = numerator / (total_amt as u128);

        // Check: user_weighted_stake with combined must equal sum of individual
        // weighted stakes (within rounding)
        let eval_time = 100_000i64;
        let w_combined = calculate_user_weighted_stake(
            total_amt, combined_esf, eval_time, base_time, tau,
        )
        .unwrap();
        let w1 = calculate_user_weighted_stake(amt1, esf1, eval_time, base_time, tau).unwrap();
        let w2 = calculate_user_weighted_stake(amt2, esf2, eval_time, base_time, tau).unwrap();
        let w_sum = w1 + w2;

        let diff = if w_combined > w_sum {
            w_combined - w_sum
        } else {
            w_sum - w_combined
        };
        // Allow up to 1 WAD rounding per deposit
        assert!(
            diff <= 2 * WAD,
            "weighted average broke: combined={}, sum={}, diff={}",
            w_combined, w_sum, diff
        );
    }

    /// Instant weight (WAD-scaled fraction of the staked amount) of a stake
    /// made when (now - base_time) / tau == ratio, evaluated at the same
    /// instant. Ideal value: 0.
    fn instant_weight_fraction(ratio_wad: u128) -> u128 {
        let tau = 1_000_000u64;
        let now = (ratio_wad * tau as u128 / WAD) as i64;
        let exp_start_factor = exp_time_ratio(now, tau).unwrap();
        let amount = 1_000_000_000u64;
        let w = calculate_user_weighted_stake(amount, exp_start_factor, now, 0, tau).unwrap();
        w / amount as u128
    }

    #[test]
    fn test_stake_gate_bounds_instant_weight() {
        // C-2: everywhere at or below the stake-time gate, a brand-new stake
        // has (near) zero weight: less than 1e-8 of the staked amount.
        assert!(MAX_STAKE_EXP_INPUT < MAX_EXP_INPUT);
        let mut ratio = 0u128;
        while ratio <= MAX_STAKE_EXP_INPUT {
            let frac = instant_weight_fraction(ratio);
            assert!(
                frac < WAD / 100_000_000,
                "instant weight {} too large at ratio {}",
                frac,
                ratio
            );
            ratio += WAD / 4;
        }

        // ...whereas near MAX_EXP_INPUT precision collapses, which is why the
        // stake gate must not use MAX_EXP_INPUT.
        assert!(instant_weight_fraction(40 * WAD + WAD / 2) > WAD / 10);
        assert_eq!(instant_weight_fraction(MAX_EXP_INPUT), WAD);
    }
}
