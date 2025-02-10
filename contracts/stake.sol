// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

interface IERC20 {
    function totalSupply() external view returns (uint256);
    function balanceOf(address account) external view returns (uint256);
    function transfer(address recipient, uint256 amount) external returns (bool);
    function allowance(address owner, address spender) external view returns (uint256);
    function approve(address spender, uint256 amount) external returns (bool);
    function transferFrom(address sender, address recipient, uint256 amount) external returns (bool);
}

contract DiscreteStakingRewards {
    IERC20 public immutable stakingToken;
    IERC20 public immutable rewardToken;
    
    uint256 public totalSupply;
    uint256 private constant MULTIPLIER = 1e18;
    uint256 private constant LOCK_PERIOD = 180 days;
    uint256 private constant UNBONDING_PERIOD = 15 days;
    uint256 private rewardIndex;
    
    struct StakeInfo {
        uint256 amount;
        uint256 timestamp;
    }

    struct UnbondingRequest {
        uint256 amount;
        uint256 unbondingEndTime;
        bool withdrawn;
    }
    
    /**
     * @dev This struct tracks an ongoing batch unstake request.
     *      - totalAmount: the total amount the user wants to unstake
     *      - processedAmount: how much has already been assigned to unbonding
     *      - currentIndex: the current index in userStakes array
     *      - active: whether this batch request is still ongoing
     */
    struct BatchUnstakeRequest {
        uint256 totalAmount;
        uint256 processedAmount;
        uint256 currentIndex;
        bool active;
    }

    // Mapping of user => array of stakes
    mapping(address => StakeInfo[]) public userStakes;
    
    // Mapping of user => staked balance (sum of stakeInfo amounts)
    mapping(address => uint256) public balanceOf;
    
    // Each user can have multiple unbonding requests
    mapping(address => UnbondingRequest[]) public unbondingRequests;
    
    // Tracks per-user reward index and earned
    mapping(address => uint256) private rewardIndexOf;
    mapping(address => uint256) private earned;

    // Mapping to hold ongoing batch-unstake requests for each user
    mapping(address => BatchUnstakeRequest) public batchUnstakeRequests;
    
    event Staked(address indexed user, uint256 amount, uint256 timestamp);
    event UnbondingStarted(address indexed user, uint256 amount, uint256 unbondingEndTime);
    event Withdrawn(address indexed user, uint256 amount);
    event RewardsClaimed(address indexed user, uint256 amount);
    
    constructor(address _stakingToken, address _rewardToken) {
        stakingToken = IERC20(_stakingToken);
        rewardToken = IERC20(_rewardToken);
    }
    
    /**
     * @dev Stake tokens. Creates a new stake entry (amount/timestamp).
     */
    function stake(uint256 amount) external {
        require(amount > 0, "Cannot stake 0");
        _updateRewards(msg.sender);
        
        userStakes[msg.sender].push(StakeInfo({
            amount: amount,
            timestamp: block.timestamp
        }));
        
        balanceOf[msg.sender] += amount;
        totalSupply += amount;
        
        // Transfer the staking tokens from the user to this contract
        stakingToken.transferFrom(msg.sender, address(this), amount);
        emit Staked(msg.sender, amount, block.timestamp);
    }

    /**
     * @notice This replaces the old single-transaction `unstake` loop.
     *         The user calls `unstakeInBatches(amount, maxIterations)` repeatedly
     *         until the `processedAmount` reaches `totalAmount`.
     *
     * @param amount        The total amount to unstake (only used if no active request).
     * @param maxIterations The maximum stake entries to process in this transaction.
     */
    function unstakeInBatches(uint256 amount, uint256 maxIterations) external {
        // Ensure user is updating their rewards
        _claim();

        // Reference the user's current batch request
        BatchUnstakeRequest storage request = batchUnstakeRequests[msg.sender];

        // If there's no active request, we're creating a new one
        if (!request.active) {
            require(amount > 0, "Cannot unstake 0");
            require(balanceOf[msg.sender] >= amount, "Insufficient balance");

            request.totalAmount = amount;
            request.processedAmount = 0;
            request.currentIndex = 0;
            request.active = true;
        } else {
            // User already has an ongoing request
            // If they pass a new 'amount' different from the existing one, revert
            // (Alternatively, you could allow it and override, but that's design-specific)
            require(amount == request.totalAmount, "Ongoing request mismatch");
        }

        // Short circuit if no iterations or request is already fully processed
        if (maxIterations == 0 || request.processedAmount == request.totalAmount) {
            return;
        }
        
        uint256 remaining = request.totalAmount - request.processedAmount;
        uint256 currentTime = block.timestamp;
        
        // Process up to maxIterations stake entries
        StakeInfo[] storage stakes = userStakes[msg.sender];
        uint256 i = request.currentIndex;
        uint256 length = stakes.length;

        for (uint256 iterCount = 0; iterCount < maxIterations && i < length && remaining > 0; i++) {
            StakeInfo storage stakeInfo = stakes[i];
            
            // Skip already depleted stakes
            if (stakeInfo.amount == 0) {
                // No changes, just move on
                continue;
            }

            // Check if stake is unlocked
            if (currentTime >= stakeInfo.timestamp + LOCK_PERIOD) {
                // Determine how much we can unstake from this stake record
                uint256 unstakeAmount = (remaining > stakeInfo.amount)
                    ? stakeInfo.amount
                    : remaining;

                // Decrease from the stake
                stakeInfo.amount -= unstakeAmount;
                // Decrease from the user's remaining request
                remaining -= unstakeAmount;
                // Increase processed
                request.processedAmount += unstakeAmount;

                // Create a new unbonding request
                unbondingRequests[msg.sender].push(UnbondingRequest({
                    amount: unstakeAmount,
                    unbondingEndTime: currentTime + UNBONDING_PERIOD,
                    withdrawn: false
                }));
                emit UnbondingStarted(msg.sender, unstakeAmount, currentTime + UNBONDING_PERIOD);
            }
            
            // We processed 1 stake entry in this iteration
            iterCount++;
        }

        // Update the request's state (where we left off in the array)
        request.currentIndex = i;

        // If we've processed the entire amount, finalize the unstake
        if (request.processedAmount == request.totalAmount) {
            // Decrement the user's staked balance and total supply
            balanceOf[msg.sender] -= request.totalAmount;
            totalSupply -= request.totalAmount;

            // Update the global reward index based on the total amount unstaked
            _updateRewardIndex(request.totalAmount);

            // Mark the request as inactive
            request.active = false;
        }
    }

    /**
     * @dev Withdraw any completed unbonding requests (and optionally rewards
     *      if stakingToken == rewardToken).
     */
    function withdraw() external {
        uint256 withdrawableAmount = 0;
        uint256 currentTime = block.timestamp;
        UnbondingRequest[] storage requests = unbondingRequests[msg.sender];
        
        for (uint256 i = 0; i < requests.length; i++) {
            if (!requests[i].withdrawn && currentTime >= requests[i].unbondingEndTime) {
                withdrawableAmount += requests[i].amount;
                requests[i].withdrawn = true;
            }
        }
        require(withdrawableAmount > 0, "No withdrawable amount");
        
        // If the staking token and the reward token are the same,
        // you can withdraw both principal + rewards in one go.
        if (address(stakingToken) == address(rewardToken)) {
            // Also withdraw any pending earned rewards
            uint256 totalAmount = withdrawableAmount + earned[msg.sender];
            // Reset earned
            earned[msg.sender] = 0;
            // Transfer principal + rewards
            stakingToken.transfer(msg.sender, totalAmount);
            // In this scenario, we've effectively claimed the rewards
            emit RewardsClaimed(msg.sender, totalAmount - withdrawableAmount);
        } else {
            // Otherwise, just withdraw the principal
            stakingToken.transfer(msg.sender, withdrawableAmount);
        }
        emit Withdrawn(msg.sender, withdrawableAmount);
    }

    /**
     * @dev Claim only the rewards (does not affect staked balance).
     */
    function claim() external returns (uint256) {
        return _claim();
    }

    /**
     * @dev Internal function to claim the rewards for `account`.
     */
    function _claim() private returns (uint256) {
        _updateRewards(msg.sender);
        uint256 reward = earned[msg.sender];
        if (reward > 0) {
            earned[msg.sender] = 0;
            rewardToken.transfer(msg.sender, reward);
            emit RewardsClaimed(msg.sender, reward);
        }
        return reward;
    }

    /**
     * @dev Increases the global reward index based on `reward` distribution.
     */
    function _updateRewardIndex(uint256 reward) private {
        // Avoid division by zero
        if (totalSupply == 0) {
            return;
        }
        rewardIndex += (reward * MULTIPLIER) / totalSupply;
    }

    /**
     * @dev Calculates pending rewards for a user (based on the difference
     *      in their personal rewardIndex).
     */
    function _calculateRewards(address account) private view returns (uint256) {
        uint256 shares = balanceOf[account];
        if (shares == 0) {
            return 0;
        }
        return (shares * (rewardIndex - rewardIndexOf[account])) / MULTIPLIER;
    }

    /**
     * @dev Public view function to check how many total rewards a user
     *      has accumulated (both stored + pending).
     */
    function calculateRewardsEarned(address account) external view returns (uint256) {
        return earned[account] + _calculateRewards(account);
    }

    /**
     * @dev Updates the user's `earned` rewards by first calculating them
     *      (based on the difference in global rewardIndex) and then
     *      synchronizing the user's personal rewardIndex.
     */
    function _updateRewards(address account) private {
        uint256 newRewards = _calculateRewards(account);
        if (newRewards > 0) {
            earned[account] += newRewards;
        }
        rewardIndexOf[account] = rewardIndex;
    }
    
    /**
     * @dev Helper to get the stake array for UI/analytics.
     */
    function getUserStakes(address user) external view returns (
        uint256[] memory amounts,
        uint256[] memory timestamps
    ) {
        StakeInfo[] storage stakes = userStakes[user];
        amounts = new uint256[](stakes.length);
        timestamps = new uint256[](stakes.length);
        
        for(uint256 i = 0; i < stakes.length; i++) {
            amounts[i] = stakes[i].amount;
            timestamps[i] = stakes[i].timestamp;
        }
        
        return (amounts, timestamps);
    }
}
