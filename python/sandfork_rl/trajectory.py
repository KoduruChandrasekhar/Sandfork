"""Token-exact trajectories.

Never decode the conversation to text and tokenize it again, neither to build
the next prompt nor to build training data. Byte-pair encoding of the same
text can produce different token IDs than the model actually generated, most
often at the boundary between generated text and an appended observation.
Training on those IDs is silently off-policy.

So a trajectory is built by appending IDs: the prompt's, then each generated
span exactly as the generator returned it, then each observation's. Every
generated span is recorded separately, so the token-exactness assertion can
compare it, ID for ID, with what the generator returned.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any, Dict, List, Optional


@dataclass
class Span:
    """One contiguous piece of a trajectory."""

    kind: str  # "prompt", "gen" or "obs"
    start: int
    token_ids: List[int]
    #: Behaviour-policy log-probabilities, generated spans only.
    logprobs: List[float] = field(default_factory=list)
    policy_version: int = -1
    turn: int = -1


@dataclass
class Trajectory:
    ids: List[int] = field(default_factory=list)
    #: 1 for tokens the model produced (loss is computed on them), else 0.
    mask: List[int] = field(default_factory=list)
    #: Behaviour log-probability per token; 0.0 where mask is 0.
    logp: List[float] = field(default_factory=list)
    spans: List[Span] = field(default_factory=list)
    task_id: str = ""
    sandbox_id: str = ""
    group_id: str = ""
    reward: Optional[float] = None
    status: str = ""
    turns: int = 0
    resumed: bool = False
    #: Index of the turn this rollout branched from (Part 9), or -1.
    branch_turn: int = -1
    branch_id: int = -1
    meta: Dict[str, Any] = field(default_factory=dict)

    def __len__(self) -> int:
        return len(self.ids)

    def add_prompt(self, ids: List[int]) -> None:
        self.spans.append(Span("prompt", len(self.ids), list(ids)))
        self.ids += ids
        self.mask += [0] * len(ids)
        self.logp += [0.0] * len(ids)

    def add_generated(self, ids: List[int], logprobs: List[float], policy_version: int, turn: int) -> None:
        if len(ids) != len(logprobs):
            raise ValueError(f"{len(ids)} generated tokens but {len(logprobs)} log-probabilities")
        self.spans.append(Span("gen", len(self.ids), list(ids), list(logprobs), policy_version, turn))
        self.ids += ids
        self.mask += [1] * len(ids)
        self.logp += list(logprobs)

    def add_observation(self, ids: List[int], turn: int) -> None:
        self.spans.append(Span("obs", len(self.ids), list(ids), turn=turn))
        self.ids += ids
        self.mask += [0] * len(ids)
        self.logp += [0.0] * len(ids)

    @property
    def model_tokens(self) -> int:
        return sum(self.mask)

    @property
    def oldest_policy(self) -> int:
        versions = [s.policy_version for s in self.spans if s.kind == "gen"]
        return min(versions) if versions else -1

    def truncate_to_turn(self, turn: int) -> None:
        """Drop everything after turn ``turn`` (crash recovery: resume from a
        checkpoint taken after that turn). Kept tokens keep the log-probability
        of the policy that generated them, so the rollout stays a valid sample."""
        keep = [s for s in self.spans if s.kind == "prompt" or s.turn <= turn]
        end = keep[-1].start + len(keep[-1].token_ids) if keep else 0
        self.spans = keep
        self.ids, self.mask, self.logp = self.ids[:end], self.mask[:end], self.logp[:end]
        self.turns = turn + 1
        self.resumed = True

    def branch(self) -> "Trajectory":
        """A copy sharing this trajectory's prefix, for a branched rollout."""
        t = Trajectory(list(self.ids), list(self.mask), list(self.logp), [Span(s.kind, s.start, list(s.token_ids), list(s.logprobs), s.policy_version, s.turn) for s in self.spans])
        t.task_id, t.turns, t.branch_turn = self.task_id, self.turns, self.turns - 1
        return t

    def check(self) -> None:
        """Internal consistency: spans tile the sequence exactly, the mask is 1
        on generated spans only, and the log-probabilities line up."""
        pos = 0
        for s in self.spans:
            assert s.start == pos, f"span at {s.start} expected at {pos}"
            assert self.ids[s.start : s.start + len(s.token_ids)] == s.token_ids
            m = 1 if s.kind == "gen" else 0
            assert all(x == m for x in self.mask[s.start : s.start + len(s.token_ids)]), f"mask wrong on a {s.kind} span"
            if s.kind == "gen":
                assert self.logp[s.start : s.start + len(s.token_ids)] == s.logprobs
            pos += len(s.token_ids)
        assert pos == len(self.ids) == len(self.mask) == len(self.logp)


def assert_token_exact(traj: Trajectory, returned: List[List[int]]) -> None:
    """Assert each generated span equals, ID for ID, what the generator returned."""
    gen = [s.token_ids for s in traj.spans if s.kind == "gen"]
    if len(gen) != len(returned):
        raise AssertionError(f"{len(gen)} generated spans recorded, {len(returned)} returned by the generator")
    for i, (a, b) in enumerate(zip(gen, returned)):
        if a != b:
            first = next((j for j, (x, y) in enumerate(zip(a, b)) if x != y), min(len(a), len(b)))
            raise AssertionError(f"generated span {i} differs from the generator's output at token {first}")
