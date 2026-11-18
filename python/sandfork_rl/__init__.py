"""The rollout orchestrator and a minimal GRPO trainer on top of Sandfork.

The RL loop is the test harness, not the product: about a thousand lines that
drive Sandfork exactly the way a real trainer would, with token-exact
trajectories and correct loss masks.
"""
