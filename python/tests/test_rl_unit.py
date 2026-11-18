"""Unit tests for the RL side: tool protocol, advantage math on fixed vectors,
trajectory bookkeeping, the loss mask, and token-exactness."""

import math

import pytest

from sandfork_rl import grpo, tools
from sandfork_rl.generate import ByteTokenizer, ScriptedGenerator
from sandfork_rl.trajectory import Trajectory, assert_token_exact

# ---------------------------------------------------------------- tool protocol


def test_parse_valid_call():
    c = tools.parse_tool_call('I will look.<tool>{"name": "bash", "args": {"cmd": "ls -la"}}</tool>')
    assert c.name == "bash" and c.args == {"cmd": "ls -la"} and c.error is None


def test_parse_takes_the_first_call():
    c = tools.parse_tool_call(tools.format_call("read_file", path="a") + tools.format_call("bash", cmd="x"))
    assert c.name == "read_file"


@pytest.mark.parametrize(
    "text,fragment",
    [
        ("<tool>not json</tool>", "not valid JSON"),
        ('<tool>["bash"]</tool>', '"name"'),
        ('<tool>{"name": "bash", "args": [1]}</tool>', '"args"'),
        ('<tool>{"name": "bash"', "closing"),
    ],
)
def test_malformed_calls_explain_themselves(text, fragment):
    c = tools.parse_tool_call(text)
    assert c.name == "invalid" and fragment in c.error


def test_no_call_is_none():
    assert tools.parse_tool_call("I am done thinking.") is None


def test_format_roundtrip():
    text = tools.format_call("write_file", path="src/a.py", content='print("<tool>")\n')
    c = tools.parse_tool_call(text)
    assert c.name == "write_file" and c.args["content"] == 'print("<tool>")\n'


def test_long_output_keeps_head_and_tail():
    text = "A" * 5000 + "MIDDLE" + "Z" * 5000
    out = tools.truncate_middle(text, keep=100)
    assert out.startswith("A" * 100) and out.endswith("Z" * 100)
    assert "characters omitted" in out and "MIDDLE" not in out
    assert tools.truncate_middle("short") == "short"


def test_observation_is_wrapped():
    assert tools.format_observation("ok") == "<obs>ok</obs>"


# ---------------------------------------------------------------- advantages


def test_dr_grpo_subtracts_the_mean_only():
    assert grpo.advantages([1.0, 0.0, 0.0, 1.0]) == [0.5, -0.5, -0.5, 0.5]
    assert grpo.advantages([0.875, 0.5]) == [0.1875, -0.1875]


def test_grpo_divides_by_the_sample_std():
    a = grpo.advantages([1.0, 0.0, 0.0, 1.0], "grpo")
    std = math.sqrt(1 / 3)
    assert a == pytest.approx([0.5 / std, -0.5 / std, -0.5 / std, 0.5 / std], rel=1e-5)


def test_rloo_uses_the_other_members():
    assert grpo.advantages([1.0, 0.0, 0.5], "rloo") == pytest.approx([0.75, -0.75, 0.0])


def test_infra_errors_are_dropped_not_zeroed():
    a = grpo.advantages([1.0, None, 0.0])
    assert a == [0.5, None, -0.5], "the mean is over valid rollouts only"


def test_a_group_with_fewer_than_two_valid_rollouts_is_dropped():
    assert grpo.advantages([None, 1.0, None]) is None
    assert grpo.advantages([]) is None


def test_identical_rewards_give_zero_advantage():
    assert grpo.advantages([1.0] * 8) == [0.0] * 8


def test_wasted_probability_table():
    for p, want in [(0.05, 0.66), (0.1, 0.43), (0.2, 0.17), (0.5, 0.008), (0.8, 0.17), (0.95, 0.66)]:
        assert abs(grpo.wasted_probability(p, 8) - want) < 0.01


def test_group_summary():
    s = grpo.summarize_groups([[1.0, 0.0], [1.0, 1.0], [None, 1.0]])
    assert s["useful_groups"] == 1 and s["dropped_groups"] == 1


# ---------------------------------------------------------------- trajectories


def make_traj():
    t = Trajectory()
    t.add_prompt([1, 2, 3])
    t.add_generated([4, 5], [-0.1, -0.2], 0, 0)
    t.add_observation([6, 7, 8], 0)
    t.add_generated([9], [-0.3], 1, 1)
    return t


def test_mask_and_logprobs_line_up():
    t = make_traj()
    t.check()
    assert t.mask == [0, 0, 0, 1, 1, 0, 0, 0, 1]
    assert t.logp == [0, 0, 0, -0.1, -0.2, 0, 0, 0, -0.3]
    assert t.model_tokens == 3 and t.oldest_policy == 0


def test_truncate_to_a_checkpoint_turn():
    t = make_traj()
    t.truncate_to_turn(0)
    t.check()
    assert t.ids == [1, 2, 3, 4, 5, 6, 7, 8] and t.resumed and t.turns == 1


def test_mismatched_logprobs_are_rejected():
    with pytest.raises(ValueError):
        Trajectory().add_generated([1, 2], [-0.1], 0, 0)


def test_token_exactness_catches_retokenization_drift():
    t = make_traj()
    assert_token_exact(t, [[4, 5], [9]])
    with pytest.raises(AssertionError, match="span 1"):
        assert_token_exact(t, [[4, 5], [10]])


def test_branch_shares_the_prefix():
    t = make_traj()
    b = t.branch()
    b.add_observation([10], 1)
    assert b.ids[: len(t.ids)] == t.ids and len(t.ids) == 9


# ---------------------------------------------------------------- generators


@pytest.mark.asyncio
async def test_scripted_generator_and_prefix_cache():
    tok = ByteTokenizer()
    gen = ScriptedGenerator(tok, [tools.format_call("bash", cmd="ls"), tools.format_call("submit")])
    prompt = tok.chat_prompt("sys", "user" * 20)
    a = await gen.generate(prompt, max_new_tokens=512, stop=[tools.TOOL_CLOSE], turn=0)
    assert tok.decode(a.token_ids) == tools.format_call("bash", cmd="ls")
    assert len(a.logprobs) == len(a.token_ids)
    b = await gen.generate(prompt, max_new_tokens=512, stop=[tools.TOOL_CLOSE], turn=1)
    assert b.cached_prompt_tokens >= len(prompt) - gen.block, "a repeated prefix is served from the cache"


def test_byte_tokenizer_roundtrips():
    tok = ByteTokenizer()
    s = 'héllo <tool>{"a":1}</tool>'
    assert tok.decode(tok.encode(s)) == s
