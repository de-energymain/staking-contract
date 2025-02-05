// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

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
    
    mapping(address => StakeInfo[]) public userStakes;
    mapping(address => uint256) public balanceOf;
    mapping(address => UnbondingRequest[]) public unbondingRequests;
    mapping(address => uint256) private rewardIndexOf;
    mapping(address => uint256) private earned;
    
    event Staked(address indexed user, uint256 amount, uint256 timestamp);
    event UnbondingStarted(address indexed user, uint256 amount, uint256 unbondingEndTime);
    event Withdrawn(address indexed user, uint256 amount);
    event RewardsClaimed(address indexed user, uint256 amount);
    
    constructor(address _stakingToken, address _rewardToken) {
        stakingToken = IERC20(_stakingToken);
        rewardToken = IERC20(_rewardToken);
    }
    
    function stake(uint256 amount) external {
        require(amount > 0, "Cannot stake 0");
        _updateRewards(msg.sender);
        userStakes[msg.sender].push(StakeInfo(amount, block.timestamp));
        balanceOf[msg.sender] += amount;
        totalSupply += amount;
        stakingToken.transferFrom(msg.sender, address(this), amount);
        emit Staked(msg.sender, amount, block.timestamp);
    }
    
    function unstake(uint256 amount) external {
        require(amount > 0, "Cannot unstake 0");
        require(balanceOf[msg.sender] >= amount, "Insufficient balance");
        _claim();
        uint256 remainingAmount = amount;
        uint256 currentTime = block.timestamp;
        
        for (uint256 i = 0; i < userStakes[msg.sender].length && remainingAmount > 0; i++) {
            StakeInfo storage stakeInfo = userStakes[msg.sender][i];
            if (stakeInfo.amount == 0) continue;
            if (currentTime >= stakeInfo.timestamp + LOCK_PERIOD) {
                uint256 unstakeAmount = remainingAmount > stakeInfo.amount ? stakeInfo.amount : remainingAmount;
                stakeInfo.amount -= unstakeAmount;
                remainingAmount -= unstakeAmount;
                unbondingRequests[msg.sender].push(UnbondingRequest(unstakeAmount, currentTime + UNBONDING_PERIOD, false));
                emit UnbondingStarted(msg.sender, unstakeAmount, currentTime + UNBONDING_PERIOD);
            }
        }
        require(remainingAmount == 0, "Insufficient unlocked stakes");
        balanceOf[msg.sender] -= amount;
        totalSupply -= amount;
        _updateRewardIndex(amount);
    }
    
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
        
        if (address(stakingToken) == address(rewardToken)) {
            uint256 totalAmount = withdrawableAmount + earned[msg.sender];
            earned[msg.sender] = 0;
            stakingToken.transfer(msg.sender, totalAmount);
            emit RewardsClaimed(msg.sender, earned[msg.sender]);
        } else {
            stakingToken.transfer(msg.sender, withdrawableAmount);
        }
        emit Withdrawn(msg.sender, withdrawableAmount);
    }
    
    function claim() external returns (uint256) {
        return _claim();
    }
    
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
    
    function _updateRewardIndex(uint256 reward) private {
        rewardIndex += (reward * MULTIPLIER) / totalSupply;
    }
    
    function _calculateRewards(address account) private view returns (uint256) {
        uint256 shares = balanceOf[account];
        return (shares * (rewardIndex - rewardIndexOf[account])) / MULTIPLIER;
    }
    
    function calculateRewardsEarned(address account) external view returns (uint256) {
        return earned[account] + _calculateRewards(account);
    }
    
    function _updateRewards(address account) private {
        earned[account] += _calculateRewards(account);
        rewardIndexOf[account] = rewardIndex;
    }
    
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

interface IERC20 {
    function totalSupply() external view returns (uint256);
    function balanceOf(address account) external view returns (uint256);
    function transfer(address recipient, uint256 amount) external returns (bool);
    function allowance(address owner, address spender) external view returns (uint256);
    function approve(address spender, uint256 amount) external returns (bool);
    function transferFrom(address sender, address recipient, uint256 amount) external returns (bool);
}