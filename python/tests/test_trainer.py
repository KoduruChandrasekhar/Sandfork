"""Trainer tests that need PyTorch: the loss mask, clipping, token-level
aggregation, and that a step changes the policy in the advantage's direction."""

import pytest

torch = pytest.importorskip("torch")
pytest.importorskip("transformers")

from sandfork_rl.grpo import LossConfig, policy_loss, token_logprobs  # noqa: E402
from sandfork_rl.train import tiny_model  # noqa: E402
from sandfork_rl.trainer import TrainConfig, Trainer, pack  # noqa: E402
from sandfork_rl.trajectory import Trajectory  # noqa: E402


def traj(prompt, gen, obs, gen2, model):
    t = Trajectory()
    t.add_prompt(prompt)
    ids = torch.tensor([prompt + gen + obs + gen2])
    with torch.no_grad():
        lp = token_logprobs(model, ids)[0].tolist()
    p, g, o = len(prompt), len(gen), len(obs)
    t.add_generated(gen, lp[p : p + g], 0, 0)
    t.add_observation(obs, 0)
    t.add_generated(gen2, lp[p + g + o :], 0, 1)
    return t


def test_prompt_and_observation_tokens_get_exactly_zero_gradient():
    """Gradient hook on the input embeddings: only positions whose *next*
    token is a model token may receive gradient through the loss."""
    model = tiny_model(259)
    t = traj([256, 10, 11, 12], [20, 21], [30, 31, 32], [40], model)
    ids = torch.tensor([t.ids])
    emb = model.get_input_embeddings()(ids).detach().requires_grad_(True)
    out = model(inputs_embeds=emb)
    logp = torch.log_softmax(out.logits[:, :-1], -1).gather(-1, ids[:, 1:].unsqueeze(-1)).squeeze(-1)
    logp = torch.cat([torch.zeros_like(logp[:, :1]), logp], 1)
    mask = torch.tensor([t.mask], dtype=torch.float)
    loss, _ = policy_loss(logp, torch.tensor([t.logp]), torch.tensor([1.0]), mask, LossConfig())
    loss.backward()
    g = emb.grad[0].abs().sum(-1)
    # Causal model: position j's embedding influences predictions at j+1...
    # Positions after the last model token must have zero gradient.
    last_model = max(i for i, m in enumerate(t.mask) if m)
    assert torch.all(g[last_model:] == 0), "no position at or after the last model token can affect the loss"
    # And the loss is a function of model tokens only: changing an
    # observation token's *label* does not change the loss.
    mask2 = mask.clone()
    lp2 = logp.detach().clone()
    obs_pos = t.spans[2].start + 1  # inside the observation span
    assert t.mask[obs_pos] == 0
    lp2[0, obs_pos] += 3.0
    l2, _ = policy_loss(lp2, torch.tensor([t.logp]), torch.tensor([1.0]), mask2, LossConfig())
    l1, _ = policy_loss(logp.detach(), torch.tensor([t.logp]), torch.tensor([1.0]), mask2, LossConfig())
    assert float(l1) == float(l2)


def test_masked_positions_contribute_nothing_to_parameter_gradients():
    model = tiny_model(259)
    t = traj([256, 1, 2], [3, 4], [5, 6], [7], model)
    ids = torch.tensor([t.ids])
    lp = token_logprobs(model, ids)
    mask = torch.tensor([t.mask], dtype=torch.float)
    zero_mask = torch.zeros_like(mask)
    loss, _ = policy_loss(lp, torch.tensor([t.logp]), torch.tensor([1.0]), zero_mask, LossConfig())
    loss.backward()
    assert all(p.grad is None or torch.all(p.grad == 0) for p in model.parameters()), "an all-zero mask must give zero gradient everywhere"


def test_ratio_is_one_and_mismatch_zero_when_recomputed_exactly():
    model = tiny_model(259)
    t = traj([256, 1, 2], [3, 4, 5], [6], [7, 8], model)
    ids = torch.tensor([t.ids])
    with torch.no_grad():
        lp = token_logprobs(model, ids)
    _, m = policy_loss(lp, torch.tensor([t.logp]), torch.tensor([0.5]), torch.tensor([t.mask], dtype=torch.float), LossConfig())
    assert m["mismatch_mean"] < 1e-5 and m["ratio_off_frac"] == 0.0


def test_trainer_disables_dropout():
    from transformers import GPT2Config, GPT2LMHeadModel

    from sandfork_rl.trainer import disable_dropout

    m = GPT2LMHeadModel(GPT2Config(vocab_size=50, n_embd=16, n_layer=1, n_head=1))
    assert disable_dropout(m) > 0
    assert all(d.p == 0 for d in m.modules() if isinstance(d, torch.nn.Dropout))
    m.train()
    ids = torch.tensor([[1, 2, 3, 4]])
    a, b = token_logprobs(m, ids), token_logprobs(m, ids)
    assert torch.equal(a, b), "train-mode forward passes must be deterministic once dropout is off"


def test_asymmetric_clipping():
    cfg = LossConfig(eps_low=0.2, eps_high=0.28)
    lgen = torch.zeros(1, 3)
    mask = torch.tensor([[0.0, 1.0, 1.0]])
    for ratio, adv, want in [(1.5, 1.0, -1.28), (0.5, -1.0, 0.8), (1.1, 1.0, -1.1), (0.5, 1.0, -0.5)]:
        lnew = torch.full((1, 3), float(torch.log(torch.tensor(ratio))))
        loss, _ = policy_loss(lnew, lgen, torch.tensor([adv]), mask, cfg)
        assert float(loss) == pytest.approx(want, abs=1e-5), (ratio, adv)


def test_token_level_aggregation_weights_every_token_equally():
    cfg = LossConfig()
    lgen = torch.zeros(2, 5)
    lnew = torch.zeros(2, 5)
    mask = torch.tensor([[0, 1, 0, 0, 0], [0, 1, 1, 1, 1]], dtype=torch.float)
    loss, m = policy_loss(lnew, lgen, torch.tensor([1.0, -1.0]), mask, cfg)
    # One token at +1, four at -1: token mean is (1 - 4) / 5.
    assert float(loss) == pytest.approx(-(1 - 4) / 5)
    assert m["tokens"] == 5


def test_pack_respects_the_token_budget():
    ts = []
    for n in [3, 50, 7, 49, 4]:
        t = Trajectory()
        t.add_prompt([1] * n)
        ts.append(t)
    batches = pack(ts, max_tokens=100)
    assert sorted(i for b in batches for i in b) == list(range(5))
    for b in batches:
        assert max(len(ts[i]) for i in b) * len(b) <= 100 or len(b) == 1


def test_a_step_moves_the_policy_toward_positive_advantage():
    torch.manual_seed(0)
    model = tiny_model(259, seed=1)
    good = traj([256, 1, 2, 3], [100, 101, 102], [5], [103], model)
    bad = traj([256, 1, 2, 3], [200, 201, 202], [5], [203], model)
    trainer = Trainer(model, TrainConfig(lr=5e-3))

    def seq_logp(t):
        with torch.no_grad():
            lp = token_logprobs(trainer.model, torch.tensor([t.ids]))[0]
        return float((lp * torch.tensor(t.mask, dtype=torch.float)).sum())

    before = seq_logp(good) - seq_logp(bad)
    for _ in range(3):
        m = trainer.step([good, bad], [1.0, -1.0])
    after = seq_logp(good) - seq_logp(bad)
    assert after > before, "the positively-advantaged sequence must become more likely relative to the other"
    assert trainer.version == 3 and m["grad_norm"] > 0 and "mismatch_mean" in m
