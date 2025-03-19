use anchor_lang::prelude::*;
use anchor_spl::token::{self, Token, TokenAccount, Transfer};
use std::convert::TryFrom;

declare_id!("B2zxan5fmTqNuxuAe7hXTCAo2Qjy5B99tYMdb793dyrG");

// Constants
const MULTIPLIER: u64 = 1_000_000_000_000_000_000; // 1e18 equivalent
const LOCK_PERIOD: i64 = 180 * 24 * 60 * 60; // 180 days in seconds
const UNBONDING_PERIOD: i64 = 15 * 24 * 60 * 60; // 15 days in seconds

#[program]
pub mod discrete_staking_rewards {
    use super::*;

    pub fn initialize(
        ctx: Context<Initialize>,
    ) -> Result<()> {
        let staking_pool = &mut ctx.accounts.staking_pool;
        staking_pool.staking_mint = ctx.accounts.staking_mint.key();
        staking_pool.reward_mint = ctx.accounts.reward_mint.key();
        staking_pool.staking_vault = ctx.accounts.staking_vault.key();
        staking_pool.reward_vault = ctx.accounts.reward_vault.key();
        staking_pool.authority = ctx.accounts.authority.key();
        staking_pool.total_supply = 0;
        staking_pool.reward_index = 0;
        staking_pool.bump = ctx.bumps.staking_pool;
        
        Ok(())
    }

    pub fn stake(
        ctx: Context<Stake>,
        amount: u64,
    ) -> Result<()> {
        require!(amount > 0, StakingError::CannotStakeZero);
        
        // Limit the maximum number of active stakes to prevent gas issues
        const MAX_ACTIVE_STAKES: usize = 50; // Set a reasonable limit
        let active_stakes = ctx.accounts.user_state.stakes
            .iter()
            .filter(|stake| stake.amount > 0)
            .count();
            
        // Only enforce if there are already many stakes
        if active_stakes >= MAX_ACTIVE_STAKES {
            // Try to automatically consolidate similar stakes
            let similar_stake_index = find_similar_stake(&ctx.accounts.user_state.stakes, 24 * 60 * 60); // 1 day window
            
            if let Some(index) = similar_stake_index {
                // Add to an existing stake with similar timestamp
                ctx.accounts.user_state.stakes[index].amount += amount;
                
                // Update the user's total balance
                ctx.accounts.user_state.balance += amount;
                ctx.accounts.staking_pool.total_supply += amount;
                
                // Transfer the tokens to the vault
                let cpi_accounts = Transfer {
                    from: ctx.accounts.user_token_account.to_account_info(),
                    to: ctx.accounts.staking_vault.to_account_info(),
                    authority: ctx.accounts.user.to_account_info(),
                };
                let cpi_program = ctx.accounts.token_program.to_account_info();
                let cpi_ctx = CpiContext::new(cpi_program, cpi_accounts);
                token::transfer(cpi_ctx, amount)?;
                
                // Emit consolidated stake event
                emit!(StakeConsolidatedEvent {
                    user: ctx.accounts.user.key(),
                    amount,
                    existing_stake_index: index as u64,
                    total_stakes: active_stakes as u64,
                });
                
                return Ok(());
            } else {
                // Too many stakes and no similar one found
                emit!(StakeLimitReachedEvent {
                    user: ctx.accounts.user.key(),
                    current_stakes: active_stakes as u64,
                    max_stakes: MAX_ACTIVE_STAKES as u64,
                });
                
                return Err(StakingError::TooManyActiveStakes.into());
            }
        }

        // Update rewards for the user
        let user_state = &mut ctx.accounts.user_state;
        update_rewards(user_state, ctx.accounts.staking_pool.reward_index);

        // Get the current timestamp
        let clock = Clock::get()?;
        let current_time = clock.unix_timestamp;

        // Create a new stake
        let new_stake = StakeInfo {
            amount,
            timestamp: current_time,
        };

        // Add the stake to the user's stakes
        user_state.stakes.push(new_stake);
        user_state.balance += amount;
        ctx.accounts.staking_pool.total_supply += amount;

        // Transfer the tokens to the vault
        let cpi_accounts = Transfer {
            from: ctx.accounts.user_token_account.to_account_info(),
            to: ctx.accounts.staking_vault.to_account_info(),
            authority: ctx.accounts.user.to_account_info(),
        };
        let cpi_program = ctx.accounts.token_program.to_account_info();
        let cpi_ctx = CpiContext::new(cpi_program, cpi_accounts);
        token::transfer(cpi_ctx, amount)?;

        // Emit event
        emit!(StakedEvent {
            user: ctx.accounts.user.key(),
            amount,
            timestamp: current_time,
        });
        
        // If approaching the limit, emit a warning
        if active_stakes >= MAX_ACTIVE_STAKES * 3 / 4 {
            emit!(ApproachingStakeLimitEvent {
                user: ctx.accounts.user.key(),
                current_stakes: active_stakes as u64 + 1, // +1 for the one we just added
                max_stakes: MAX_ACTIVE_STAKES as u64,
            });
        }

        Ok(())
    }

    pub fn unstake(
        ctx: Context<Unstake>,
        amount: u64,
        start_index: Option<u64>,
        max_iterations: Option<u64>,
    ) -> Result<()> {
        require!(amount > 0, StakingError::CannotUnstakeZero);
        require!(ctx.accounts.user_state.balance >= amount, StakingError::InsufficientBalance);

        // Claim rewards first
        let user_state = &mut ctx.accounts.user_state;
        let staking_pool = &mut ctx.accounts.staking_pool;
        let reward_vault = &ctx.accounts.reward_vault;
        let user_reward_account = &ctx.accounts.user_reward_account;
        let token_program = &ctx.accounts.token_program;
        
        let seeds = &[
            b"staking_pool".as_ref(),
            &[staking_pool.bump],
        ];
        let signer = &[&seeds[..]];
        
        claim_internal(
            user_state, 
            staking_pool,
            reward_vault,
            user_reward_account,
            token_program,
            signer
        )?;

        // Get the current timestamp
        let clock = Clock::get()?;
        let current_time = clock.unix_timestamp;

        // Set default values or use provided values
        let start = start_index.unwrap_or(0) as usize;
        let max_iter = max_iterations.unwrap_or(25) as usize; // Default to processing 25 stakes at a time
        
        // Ensure start index is valid
        require!(start < user_state.stakes.len(), StakingError::InvalidStartIndex);
        
        let mut remaining_amount = amount;
        let mut unlocked_amount = 0;
        let mut processed_amount = 0;
        let end = std::cmp::min(start + max_iter, user_state.stakes.len());

        // Process only a subset of stakes
        for i in start..end {
            let stake = &mut user_state.stakes[i];
            
            if stake.amount == 0 || remaining_amount == 0 {
                continue;
            }

            if current_time >= stake.timestamp + LOCK_PERIOD {
                let unstake_amount = std::cmp::min(remaining_amount, stake.amount);
                stake.amount -= unstake_amount;
                remaining_amount -= unstake_amount;
                processed_amount += unstake_amount;
                unlocked_amount += unstake_amount;

                // Create an unbonding request
                let unbonding_end_time = current_time + UNBONDING_PERIOD;
                user_state.unbonding_requests.push(UnbondingRequest {
                    amount: unstake_amount,
                    unbonding_end_time,
                    withdrawn: false,
                });

                // Emit event
                emit!(UnbondingStartedEvent {
                    user: ctx.accounts.user.key(),
                    amount: unstake_amount,
                    unbonding_end_time,
                });
            }
        }

        // Check if we processed any stakes
        require!(processed_amount > 0, StakingError::NoUnlockedStakesInBatch);
        
        // Update balances based on what was successfully processed
        user_state.balance -= processed_amount;
        staking_pool.total_supply -= processed_amount;

        // Update reward index based on what was processed
        update_reward_index(staking_pool, processed_amount)?;
        
        // If we couldn't process the full amount, this will be reflected in the return value
        // The client would need to call unstake again with a higher start_index to continue
        
        // Emit an event indicating partial processing if needed
        if remaining_amount > 0 && end < user_state.stakes.len() {
            emit!(PartialUnstakeEvent {
                user: ctx.accounts.user.key(),
                requested_amount: amount,
                processed_amount,
                remaining_amount,
                next_index: end as u64,
            });
        }

        Ok(())
    }

    pub fn withdraw(
        ctx: Context<Withdraw>,
        max_requests: Option<u64>,
    ) -> Result<()> {
        // Get the current timestamp
        let clock = Clock::get()?;
        let current_time = clock.unix_timestamp;

        let mut withdrawable_amount = 0;
        let max = max_requests.unwrap_or(25) as usize; // Process up to 25 requests by default
        let mut processed_count = 0;

        // Process only a limited number of unbonding requests
        for request in &mut ctx.accounts.user_state.unbonding_requests {
            if processed_count >= max {
                break;
            }
            
            if !request.withdrawn && current_time >= request.unbonding_end_time {
                withdrawable_amount += request.amount;
                request.withdrawn = true;
                processed_count += 1;
            }
        }

        require!(withdrawable_amount > 0, StakingError::NoWithdrawableAmount);

        // Transfer tokens from the vault to the user
        let seeds = &[
            b"staking_pool".as_ref(),
            &[ctx.accounts.staking_pool.bump],
        ];
        let signer = &[&seeds[..]];

        let cpi_accounts = Transfer {
            from: ctx.accounts.staking_vault.to_account_info(),
            to: ctx.accounts.user_token_account.to_account_info(),
            authority: ctx.accounts.staking_pool.to_account_info(),
        };
        let cpi_program = ctx.accounts.token_program.to_account_info();
        let cpi_ctx = CpiContext::new_with_signer(cpi_program, cpi_accounts, signer);
        token::transfer(cpi_ctx, withdrawable_amount)?;

        // If staking token is the same as reward token, combine withdrawal with rewards
        if ctx.accounts.staking_pool.staking_mint == ctx.accounts.staking_pool.reward_mint {
            let reward = ctx.accounts.user_state.earned;
            
            if reward > 0 {
                ctx.accounts.user_state.earned = 0;
                
                // Transfer rewards
                let cpi_accounts = Transfer {
                    from: ctx.accounts.reward_vault.to_account_info(),
                    to: ctx.accounts.user_token_account.to_account_info(),
                    authority: ctx.accounts.staking_pool.to_account_info(),
                };
                let cpi_program = ctx.accounts.token_program.to_account_info();
                let cpi_ctx = CpiContext::new_with_signer(cpi_program, cpi_accounts, signer);
                token::transfer(cpi_ctx, reward)?;
                
                emit!(RewardsClaimedEvent {
                    user: ctx.accounts.user.key(),
                    amount: reward,
                });
            }
        }

        // Cleanup: Remove processed requests that have been withdrawn
        // But only do this if we've processed fewer than max to avoid excessive computation
        if processed_count < max {
            ctx.accounts.user_state.unbonding_requests.retain(|r| !r.withdrawn);
        }

        // Emit event including how many requests were processed
        emit!(WithdrawnEvent {
            user: ctx.accounts.user.key(),
            amount: withdrawable_amount,
        });
        
        // Emit event if there are more pending withdrawals
        let remaining_withdrawals = ctx.accounts.user_state.unbonding_requests
            .iter()
            .filter(|r| !r.withdrawn && current_time >= r.unbonding_end_time)
            .count();
            
        if remaining_withdrawals > 0 {
            emit!(PendingWithdrawalsEvent {
                user: ctx.accounts.user.key(),
                remaining_count: remaining_withdrawals as u64,
            });
        }

        Ok(())
    }

    pub fn claim(
        ctx: Context<Claim>,
    ) -> Result<()> {
        // Update rewards for the user
        let user_state = &mut ctx.accounts.user_state;
        update_rewards(user_state, ctx.accounts.staking_pool.reward_index);
        
        let reward = user_state.earned;
        
        if reward > 0 {
            user_state.earned = 0;
            
            // Transfer rewards
            let seeds = &[
                b"staking_pool".as_ref(),
                &[ctx.accounts.staking_pool.bump],
            ];
            let signer = &[&seeds[..]];
            
            let cpi_accounts = Transfer {
                from: ctx.accounts.reward_vault.to_account_info(),
                to: ctx.accounts.user_reward_account.to_account_info(),
                authority: ctx.accounts.staking_pool.to_account_info(),
            };
            let cpi_program = ctx.accounts.token_program.to_account_info();
            let cpi_ctx = CpiContext::new_with_signer(cpi_program, cpi_accounts, signer);
            token::transfer(cpi_ctx, reward)?;
            
            emit!(RewardsClaimedEvent {
                user: ctx.accounts.user.key(),
                amount: reward,
            });
        }
        
        Ok(())
    }

    // Admin function to add rewards to the pool
    pub fn add_rewards(
        ctx: Context<AddRewards>, 
        amount: u64
    ) -> Result<()> {
        // Transfer reward tokens to the vault
        let cpi_accounts = Transfer {
            from: ctx.accounts.from_account.to_account_info(),
            to: ctx.accounts.reward_vault.to_account_info(),
            authority: ctx.accounts.authority.to_account_info(),
        };
        let cpi_program = ctx.accounts.token_program.to_account_info();
        let cpi_ctx = CpiContext::new(cpi_program, cpi_accounts);
        token::transfer(cpi_ctx, amount)?;
        
        // Update the reward index
        update_reward_index(&mut ctx.accounts.staking_pool, amount)?;
        
        emit!(RewardsAddedEvent {
            amount,
        });
        
        Ok(())
    }
    
    // Added consolidate_stakes function
    pub fn consolidate_stakes(
        ctx: Context<ConsolidateStakes>,
        time_window: i64,
    ) -> Result<()> {
        require!(time_window > 0, StakingError::InvalidTimeWindow);
        
        // Update rewards before modifying stakes
        let user_state = &mut ctx.accounts.user_state;
        update_rewards(user_state, ctx.accounts.staking_pool.reward_index);
        
        // Get array of stakes
        let stakes = &mut user_state.stakes;
        let mut i = 0;
        
        while i < stakes.len() {
            if stakes[i].amount == 0 {
                // Remove zero amount stakes
                stakes.remove(i);
                continue;
            }
            
            let mut j = i + 1;
            while j < stakes.len() {
                // If timestamps are within the window, consolidate
                if (stakes[i].timestamp - stakes[j].timestamp).abs() <= time_window {
                    // Add the amount to the first stake
                    stakes[i].amount += stakes[j].amount;
                    // Use the earlier timestamp for the consolidated stake
                    stakes[i].timestamp = std::cmp::min(stakes[i].timestamp, stakes[j].timestamp);
                    // Zero out the consolidated stake
                    stakes[j].amount = 0;
                }
                j += 1;
            }
            i += 1;
        }
        
        // Final pass to remove all zero-amount stakes
        stakes.retain(|stake| stake.amount > 0);
        
        emit!(StakesConsolidatedEvent {
            user: ctx.accounts.user.key(),
            resulting_stake_count: stakes.len() as u64,
        });
        
        Ok(())
    }
}

// Helper functions
fn update_rewards(
    user_state: &mut Account<'_, UserState>,
    current_reward_index: u128,
) {
    let rewards = calculate_rewards(user_state.balance, current_reward_index, user_state.reward_index_of);
    user_state.earned += rewards;
    user_state.reward_index_of = current_reward_index;
}

fn calculate_rewards(
    shares: u64,
    current_reward_index: u128,
    user_reward_index: u128,
) -> u64 {
    if current_reward_index <= user_reward_index {
        return 0;
    }
    
    let shares_u128 = u128::from(shares);
    let index_delta = current_reward_index - user_reward_index;
    let reward_u128 = shares_u128 * index_delta / u128::from(MULTIPLIER);
    
    u64::try_from(reward_u128).unwrap_or(u64::MAX)
}

fn update_reward_index(
    staking_pool: &mut Account<'_, StakingPool>,
    reward: u64,
) -> Result<()> {
    if staking_pool.total_supply == 0 {
        // If no stakes, just return without updating the index
        return Ok(());
    }
    
    let reward_u128 = u128::from(reward);
    let supply_u128 = u128::from(staking_pool.total_supply);
    let multiplier_u128 = u128::from(MULTIPLIER);
    
    let index_delta = reward_u128 * multiplier_u128 / supply_u128;
    staking_pool.reward_index = staking_pool.reward_index.checked_add(index_delta)
        .ok_or(StakingError::ArithmeticOverflow)?;
    
    Ok(())
}

// Find a stake with a similar timestamp
fn find_similar_stake(stakes: &[StakeInfo], time_window: i64) -> Option<usize> {
    let clock = Clock::get().ok()?;
    let current_time = clock.unix_timestamp;
    
    // First try to find an active stake with a similar timestamp
    for (i, stake) in stakes.iter().enumerate() {
        if stake.amount > 0 && (current_time - stake.timestamp).abs() <= time_window {
            return Some(i);
        }
    }
    
    // If no similar stake found, return the most recent active stake
    stakes.iter()
        .enumerate()
        .filter(|(_, stake)| stake.amount > 0)
        .max_by_key(|(_, stake)| stake.timestamp)
        .map(|(i, _)| i)
}

fn claim_internal<'info>(
    user_state: &mut Account<'_, UserState>,
    staking_pool: &mut Account<'_, StakingPool>,
    reward_vault: &Account<'info, TokenAccount>,
    user_reward_account: &Account<'info, TokenAccount>,
    token_program: &Program<'info, Token>,
    signer: &[&[&[u8]]],
) -> Result<()> {
    update_rewards(user_state, staking_pool.reward_index);
    
    let reward = user_state.earned;
    
    if reward > 0 {
        user_state.earned = 0;
        
        // Transfer rewards
        let cpi_accounts = Transfer {
            from: reward_vault.to_account_info(),
            to: user_reward_account.to_account_info(),
            authority: staking_pool.to_account_info(),
        };
        let cpi_program = token_program.to_account_info();
        let cpi_ctx = CpiContext::new_with_signer(cpi_program, cpi_accounts, signer);
        token::transfer(cpi_ctx, reward)?;
        
        emit!(RewardsClaimedEvent {
            user: user_state.owner,
            amount: reward,
        });
    }
    
    Ok(())
}

// Account Structures
#[account]
#[derive(Default)]
pub struct StakingPool {
    pub staking_mint: Pubkey,
    pub reward_mint: Pubkey,
    pub staking_vault: Pubkey,
    pub reward_vault: Pubkey,
    pub authority: Pubkey,
    pub total_supply: u64,
    pub reward_index: u128,
    pub bump: u8,
}

#[account]
#[derive(Default)]
pub struct UserState {
    pub owner: Pubkey,
    pub balance: u64,
    pub reward_index_of: u128,
    pub earned: u64,
    pub stakes: Vec<StakeInfo>,
    pub unbonding_requests: Vec<UnbondingRequest>,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Default, Debug)]
pub struct StakeInfo {
    pub amount: u64,
    pub timestamp: i64,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Default, Debug)]
pub struct UnbondingRequest {
    pub amount: u64,
    pub unbonding_end_time: i64,
    pub withdrawn: bool,
}

// Events
#[event]
pub struct StakedEvent {
    pub user: Pubkey,
    pub amount: u64,
    pub timestamp: i64,
}

#[event]
pub struct UnbondingStartedEvent {
    pub user: Pubkey,
    pub amount: u64,
    pub unbonding_end_time: i64,
}

#[event]
pub struct WithdrawnEvent {
    pub user: Pubkey,
    pub amount: u64,
}

#[event]
pub struct RewardsClaimedEvent {
    pub user: Pubkey,
    pub amount: u64,
}

#[event]
pub struct RewardsAddedEvent {
    pub amount: u64,
}

#[event]
pub struct StakesConsolidatedEvent {
    pub user: Pubkey,
    pub resulting_stake_count: u64,
}

#[event]
pub struct PartialUnstakeEvent {
    pub user: Pubkey,
    pub requested_amount: u64,
    pub processed_amount: u64,
    pub remaining_amount: u64,
    pub next_index: u64,
}

#[event]
pub struct PendingWithdrawalsEvent {
    pub user: Pubkey,
    pub remaining_count: u64,
}

#[event]
pub struct StakeConsolidatedEvent {
    pub user: Pubkey,
    pub amount: u64,
    pub existing_stake_index: u64,
    pub total_stakes: u64,
}

#[event]
pub struct StakeLimitReachedEvent {
    pub user: Pubkey,
    pub current_stakes: u64,
    pub max_stakes: u64,
}

#[event]
pub struct ApproachingStakeLimitEvent {
    pub user: Pubkey,
    pub current_stakes: u64,
    pub max_stakes: u64,
}

// Error Codes
#[error_code]
pub enum StakingError {
    #[msg("Cannot stake zero amount")]
    CannotStakeZero,
    
    #[msg("Cannot unstake zero amount")]
    CannotUnstakeZero,
    
    #[msg("Insufficient balance")]
    InsufficientBalance,
    
    #[msg("Insufficient unlocked stakes")]
    InsufficientUnlockedStakes,
    
    #[msg("No withdrawable amount")]
    NoWithdrawableAmount,
    
    #[msg("Arithmetic overflow")]
    ArithmeticOverflow,
    
    #[msg("Too many active stakes, please consolidate first")]
    TooManyActiveStakes,
    
    #[msg("Invalid start index")]
    InvalidStartIndex,
    
    #[msg("No unlocked stakes in this batch")]
    NoUnlockedStakesInBatch,
    
    #[msg("Invalid time window")]
    InvalidTimeWindow,
}

// Context Structures
#[derive(Accounts)]
pub struct Initialize<'info> {
    #[account(
        init,
        payer = authority,
        space = 8 + std::mem::size_of::<StakingPool>(),
        seeds = [b"staking_pool"],
        bump
    )]
    pub staking_pool: Account<'info, StakingPool>,
    
    pub staking_mint: Account<'info, token::Mint>,
    pub reward_mint: Account<'info, token::Mint>,
    
    #[account(
        init,
        payer = authority,
        token::mint = staking_mint,
        token::authority = staking_pool,
        seeds = [b"staking_vault", staking_pool.key().as_ref()],
        bump
    )]
    pub staking_vault: Account<'info, TokenAccount>,
    
    #[account(
        init,
        payer = authority,
        token::mint = reward_mint,
        token::authority = staking_pool,
        seeds = [b"reward_vault", staking_pool.key().as_ref()],
        bump
    )]
    pub reward_vault: Account<'info, TokenAccount>,
    
    #[account(mut)]
    pub authority: Signer<'info>,
    
    pub system_program: Program<'info, System>,
    pub token_program: Program<'info, Token>,
    pub rent: Sysvar<'info, Rent>,
}

#[derive(Accounts)]
pub struct Stake<'info> {
    #[account(mut)]
    pub staking_pool: Account<'info, StakingPool>,
    
    #[account(
        mut,
        seeds = [b"staking_vault", staking_pool.key().as_ref()],
        bump
    )]
    pub staking_vault: Account<'info, TokenAccount>,
    
    #[account(
        init_if_needed,
        payer = user,
        space = 8 + std::mem::size_of::<UserState>() + 100 * (std::mem::size_of::<StakeInfo>() + std::mem::size_of::<UnbondingRequest>()),
        seeds = [b"user_state", staking_pool.key().as_ref(), user.key().as_ref()],
        bump
    )]
    pub user_state: Account<'info, UserState>,
    
    #[account(
        mut,
        constraint = user_token_account.owner == user.key(),
        constraint = user_token_account.mint == staking_pool.staking_mint
    )]
    pub user_token_account: Account<'info, TokenAccount>,
    
    #[account(mut)]
    pub user: Signer<'info>,
    
    pub system_program: Program<'info, System>,
    pub token_program: Program<'info, Token>,
    pub rent: Sysvar<'info, Rent>,
}

#[derive(Accounts)]
pub struct Unstake<'info> {
    #[account(mut)]
    pub staking_pool: Account<'info, StakingPool>,
    
    #[account(
        mut,
        seeds = [b"staking_vault", staking_pool.key().as_ref()],
        bump
    )]
    pub staking_vault: Account<'info, TokenAccount>,
    
    #[account(
        mut,
        seeds = [b"reward_vault", staking_pool.key().as_ref()],
        bump
    )]
    pub reward_vault: Account<'info, TokenAccount>,
    
    #[account(
        mut,
        seeds = [b"user_state", staking_pool.key().as_ref(), user.key().as_ref()],
        bump,
        constraint = user_state.owner == user.key()
    )]
    pub user_state: Account<'info, UserState>,
    
    #[account(
        mut,
        constraint = user_token_account.owner == user.key(),
        constraint = user_token_account.mint == staking_pool.staking_mint
    )]
    pub user_token_account: Account<'info, TokenAccount>,
    
    #[account(
        mut,
        constraint = user_reward_account.owner == user.key(),
        constraint = user_reward_account.mint == staking_pool.reward_mint
    )]
    pub user_reward_account: Account<'info, TokenAccount>,
    
    #[account(mut)]
    pub user: Signer<'info>,
    
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct Withdraw<'info> {
    #[account(mut)]
    pub staking_pool: Account<'info, StakingPool>,
    
    #[account(
        mut,
        seeds = [b"staking_vault", staking_pool.key().as_ref()],
        bump
    )]
    pub staking_vault: Account<'info, TokenAccount>,
    
    #[account(
        mut,
        seeds = [b"reward_vault", staking_pool.key().as_ref()],
        bump
    )]
    pub reward_vault: Account<'info, TokenAccount>,
    
    #[account(
        mut,
        seeds = [b"user_state", staking_pool.key().as_ref(), user.key().as_ref()],
        bump,
        constraint = user_state.owner == user.key()
    )]
    pub user_state: Account<'info, UserState>,
    
    #[account(
        mut,
        constraint = user_token_account.owner == user.key(),
        constraint = user_token_account.mint == staking_pool.staking_mint
    )]
    pub user_token_account: Account<'info, TokenAccount>,
    
    #[account(mut)]
    pub user: Signer<'info>,
    
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct Claim<'info> {
    #[account(mut)]
    pub staking_pool: Account<'info, StakingPool>,
    
    #[account(
        mut,
        seeds = [b"reward_vault", staking_pool.key().as_ref()],
        bump
    )]
    pub reward_vault: Account<'info, TokenAccount>,
    
    #[account(
        mut,
        seeds = [b"user_state", staking_pool.key().as_ref(), user.key().as_ref()],
        bump,
        constraint = user_state.owner == user.key()
    )]
    pub user_state: Account<'info, UserState>,
    
    #[account(
        mut,
        constraint = user_reward_account.owner == user.key(),
        constraint = user_reward_account.mint == staking_pool.reward_mint
    )]
    pub user_reward_account: Account<'info, TokenAccount>,
    
    #[account(mut)]
    pub user: Signer<'info>,
    
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct AddRewards<'info> {
    #[account(
        mut,
        constraint = staking_pool.authority == authority.key()
    )]
    pub staking_pool: Account<'info, StakingPool>,
    
    #[account(
        mut,
        seeds = [b"reward_vault", staking_pool.key().as_ref()],
        bump
    )]
    pub reward_vault: Account<'info, TokenAccount>,
    
    #[account(
        mut,
        constraint = from_account.owner == authority.key(),
        constraint = from_account.mint == staking_pool.reward_mint
    )]
    pub from_account: Account<'info, TokenAccount>,
    
    #[account(mut)]
    pub authority: Signer<'info>,
    
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct ConsolidateStakes<'info> {
    #[account(mut)]
    pub staking_pool: Account<'info, StakingPool>,
    
    #[account(
        mut,
        seeds = [b"user_state", staking_pool.key().as_ref(), user.key().as_ref()],
        bump,
        constraint = user_state.owner == user.key()
    )]
    pub user_state: Account<'info, UserState>,
    
    #[account(mut)]
    pub user: Signer<'info>,
}
