"""Generate every chart and table in docs/benchmarks.md from bench/results/.

    python bench/charts.py

Only committed raw data is used: no number reaches the docs that
bench/results/ cannot reproduce. The newest file per experiment wins.
"""

import glob
import json
import os

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt  # noqa: E402

HERE = os.path.dirname(os.path.abspath(__file__))
OUT = os.path.join(os.path.dirname(HERE), "docs", "img")
COLORS = {"f1": "#9a6b4b", "f2": "#3b6ea8", "f3": "#4a9a5b", "band": "#3b6ea8", "uniform": "#9a6b4b", "flat": "#9a6b4b", "tree-2x4": "#3b6ea8", "tree-4x2": "#4a9a5b"}


def latest(name):
    files = sorted(glob.glob(os.path.join(HERE, "results", f"{name}-*.json")))
    return json.load(open(files[-1])) if files else None


def caption(d, extra=""):
    fp = d["fingerprint"]
    return f"{fp['virtualization']} {fp['kernel']}, {fp['cores']} cores, {fp['memory_mb']} MB; commit {fp['commit']}{extra}"


def style(ax, title, xlabel, ylabel):
    ax.set_title(title, fontsize=11, loc="left")
    ax.set_xlabel(xlabel)
    ax.set_ylabel(ylabel)
    ax.grid(alpha=0.25)
    for s in ("top", "right"):
        ax.spines[s].set_visible(False)


def chart_e1(d, md):
    fams = sorted({r["family"] for r in d["rows"]})
    fig, axes = plt.subplots(1, len(fams), figsize=(4.2 * len(fams), 3.4), sharey=False)
    md.append("| Family | Mode | G | p50 ms | p99 ms |\n|---|---|---|---|---|")
    for ax, fam in zip(axes, fams):
        for mode in ("f1", "f2", "f3"):
            rs = [r for r in d["rows"] if r["family"] == fam and r["mode"] == mode and "group_ms" in r]
            if not rs:
                continue
            ax.plot([r["g"] for r in rs], [r["group_ms"]["p50"] for r in rs], "o-", color=COLORS[mode], label=mode.upper())
            ax.fill_between([r["g"] for r in rs], [r["group_ms"]["min"] for r in rs], [r["group_ms"]["max"] for r in rs], color=COLORS[mode], alpha=0.12)
            for r in rs:
                md.append(f"| {fam} | {mode.upper()} | {r['g']} | {r['group_ms']['p50']:.0f} | {r['group_ms']['p99']:.0f} |")
        ax.set_yscale("log")
        style(ax, fam, "group size G", "ms to a usable group (p50)")
        ax.legend(frameon=False)
    fig.text(0.01, 0.005, caption(d, f"; {d['reps']} reps per point, band = min to max"), fontsize=7, color="#555")
    fig.tight_layout(rect=(0, 0.04, 1, 1))
    fig.savefig(os.path.join(OUT, "e1_fork_modes.svg"))


def chart_e2(d, md):
    fig, ax = plt.subplots(figsize=(5.5, 3.4))
    md.append("| Mode | G | PSS after fork (MB) | after query | after copying the data |\n|---|---|---|---|---|")
    for mode in ("f2", "f3"):
        rs = [r for r in d["rows"] if r["mode"] == mode]
        ax.plot([r["g"] for r in rs], [r["pss_mb_after_query"] for r in rs], "o-", color=COLORS[mode], label=f"{mode.upper()} after a query")
        ax.plot([r["g"] for r in rs], [r["pss_mb_after_copy"] for r in rs], "o--", color=COLORS[mode], alpha=0.6, label=f"{mode.upper()} after df.copy()")
        for r in rs:
            md.append(f"| {mode.upper()} | {r['g']} | {r['pss_mb_after_fork']:.0f} | {r['pss_mb_after_query']:.0f} | {r['pss_mb_after_copy']:.0f} |")
    style(ax, "data-analysis: total memory of G sandboxes", "G", "total PSS (MB)")
    ax.legend(frameon=False, fontsize=8)
    fig.text(0.01, 0.005, caption(d), fontsize=7, color="#555")
    fig.tight_layout(rect=(0, 0.04, 1, 1))
    fig.savefig(os.path.join(OUT, "e2_memory.svg"))


def chart_e3(d, md):
    fig, ax = plt.subplots(figsize=(5.5, 3.4))
    rs = d["rows"]
    ax.plot([r["pool"] for r in rs], [r["rollouts_per_s"] for r in rs], "o-", color=COLORS["f2"], label="rollouts/s")
    ax2 = ax.twinx()
    ax2.plot([r["pool"] for r in rs], [r["latency_s"]["p50"] for r in rs], "s--", color="#999", label="rollout latency p50 (s)")
    ax2.set_ylabel("latency p50 (s)", color="#777")
    style(ax, "py-bugfix throughput, open-loop, scripted agent", "pool size", "rollouts per second")
    md.append("| Pool | Rollouts/s | Latency p50 s | p99 s | Errors |\n|---|---|---|---|---|")
    for r in rs:
        md.append(f"| {r['pool']} | {r['rollouts_per_s']:.2f} | {r['latency_s']['p50']:.2f} | {r['latency_s']['p99']:.2f} | {r['errors']} |")
    fig.text(0.01, 0.005, caption(d, f"; {d['duration_s']:.0f} s per point"), fontsize=7, color="#555")
    fig.tight_layout(rect=(0, 0.04, 1, 1))
    fig.savefig(os.path.join(OUT, "e3_throughput.svg"))


def chart_e5(d, md):
    fig, ax = plt.subplots(figsize=(5.5, 3.4))
    md.append("| Strategy | Seed | Useful-group fraction | Sandbox-seconds per useful group |\n|---|---|---|---|")
    for strat in ("band", "uniform"):
        for i, r in enumerate(x for x in d["rows"] if x["strategy"] == strat):
            c = r["useful_curve"]
            smooth = [sum(c[max(0, j - 9) : j + 1]) / len(c[max(0, j - 9) : j + 1]) for j in range(len(c))]
            ax.plot(smooth, color=COLORS[strat], alpha=0.8, label=strat if i == 0 else None)
            md.append(f"| {strat} | {r['seed']} | {r['useful_fraction']:.2f} | {r['sandbox_seconds_per_useful_group']:.0f} |")
    style(ax, "useful groups per step (simulated, 10-step mean)", "step", "fraction of groups with reward variance")
    ax.legend(frameon=False)
    fig.text(0.01, 0.005, caption(d, "; simulation of scheduler.rs"), fontsize=7, color="#555")
    fig.tight_layout(rect=(0, 0.04, 1, 1))
    fig.savefig(os.path.join(OUT, "e5_scheduling.svg"))


def chart_e6(d, md):
    shapes = ["flat", "tree-2x4", "tree-4x2"]
    agg = {s: [r for r in d["rows"] if r["shape"] == s] for s in shapes}
    fig, axes = plt.subplots(1, 2, figsize=(8, 3.2))
    vals = [sum(r["command_s_per_leaf"] for r in agg[s]) / len(agg[s]) for s in shapes]
    axes[0].bar(shapes, vals, color=[COLORS[s] for s in shapes])
    style(axes[0], "environment: command seconds per leaf", "", "s")
    vals2 = [sum(r["prefill_tokens_per_leaf"] for r in agg[s]) / len(agg[s]) for s in shapes]
    axes[1].bar(shapes, vals2, color=[COLORS[s] for s in shapes])
    style(axes[1], "model: prefill tokens computed per leaf", "", "tokens")
    md.append("| Shape | Command s/leaf | Env s/leaf | Prefill tokens/leaf | Prefix hit rate | Wall s |\n|---|---|---|---|---|---|")
    for s in shapes:
        rs = agg[s]
        m = lambda k: sum(r[k] for r in rs) / len(rs)  # noqa: E731
        md.append(f"| {s} | {m('command_s_per_leaf'):.2f} | {m('env_s_per_leaf'):.2f} | {m('prefill_tokens_per_leaf'):.0f} | {m('prefix_hit_rate'):.2f} | {m('wall_s'):.1f} |")
    fig.text(0.01, 0.005, caption(d, f"; {len(agg['flat'])} reps"), fontsize=7, color="#555")
    fig.tight_layout(rect=(0, 0.05, 1, 1))
    fig.savefig(os.path.join(OUT, "e6_branching.svg"))


def main():
    os.makedirs(OUT, exist_ok=True)
    tables = {}
    for name, fn in (("e1", chart_e1), ("e2", chart_e2), ("e3", chart_e3), ("e5", chart_e5), ("e6", chart_e6)):
        d = latest(name)
        if d is None:
            continue
        md = []
        fn(d, md)
        tables[name] = "\n".join(md)
        print(f"{name}: chart and table from {d['fingerprint']['commit']}")
    with open(os.path.join(HERE, "results", "tables.md"), "w") as f:
        for k, v in tables.items():
            f.write(f"## {k}\n\n{v}\n\n")


if __name__ == "__main__":
    main()
