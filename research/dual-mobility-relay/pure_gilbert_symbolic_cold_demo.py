"""Symbolic (sympy) derivation of g_cold for the SYMMETRIC pure-Gilbert hop pair, with park
durations n_H, n_BB kept as FREE symbols (not committed to specific integers), per user request
and Opus-advisor consultation (2026-07-19).

Convention (the advisor's "clean restatement", arrived at after debugging the numeric version in
`pure_gilbert_finite_mdp_demo.py`'s cold-side companion this session): n_H, n_BB represent the
DECAY EXPONENT directly (matching what's observed by tracing the continuous solver's policy grid
-- "how many consecutive 'stay' decisions occur before the first 'switch'"), NOT the number of
park steps. The number of cost_a-charging park steps is (n_H - 1) / (n_BB - 1).

Uses the CLOSED FORM for a single hop's n-step transition (no matrix powers needed):
  P(Bad at exponent n | started Good) = pi_b * (1 - x)
  P(Bad at exponent n | started Bad)  = pi_b + x * (1 - pi_b)
where x = lambda^n. Joint (two independent hops, rho=0) factors as a product.

Structure: Markov-renewal-reward over the embedded chain {H, BB} (H = one hop bad, merged
GB=BG under the hop1=hop2 symmetry). Each "cycle" = [(n_entry-1) park steps] + [1 return step,
using the n_entry-step-decayed belief] + [conditional B-ride while GG persists, via the absorbing
fundamental matrix with GG as the sole transient state].

Run with: uv run --with sympy python pure_gilbert_symbolic_cold_demo.py
"""

from __future__ import annotations

import sympy as sp

pi_b, lam, cost_a, c_switch_cold = sp.symbols("pi_b lambda cost_a c_switch_cold", positive=True)
n_H, n_BB = sp.symbols("n_H n_BB", positive=True, integer=True)
x_H, x_BB = sp.symbols("x_H x_BB", positive=True)  # x_i := lambda**n_i, kept independent until substitution

p_gb = pi_b * (1 - lam)
q_G = 1 - p_gb  # P(Good next | Good), single step -- already validated on the warm side

N = 1 / (1 - q_G**2)  # expected GG-ride length once entered (absorbing fundamental matrix, scalar)

# GG's single-step exit distribution (already validated identity, matches the warm-side f_GG spot
# check the advisor gave: f_GG[H] = 2*(1-p_gb)/(2-p_gb), f_GG[BB] = p_gb/(2-p_gb)).
f_GG_H = sp.simplify(2 * (1 - p_gb) / (2 - p_gb))
f_GG_BB = sp.simplify(p_gb / (2 - p_gb))
print("GG exit distribution check: f_GG_H + f_GG_BB =", sp.simplify(f_GG_H + f_GG_BB), "(should be 1)")


def return_step_distribution(x, entry: str):
    """Joint distribution [P(GG), P(H), P(BB)] at the return step, entry-exponent x=lambda**n_entry."""
    if entry == "H":
        a1 = pi_b + x * (1 - pi_b)  # hop1 started Bad
        a2 = pi_b * (1 - x)         # hop2 started Good
    else:  # BB
        a1 = a2 = pi_b + x * (1 - pi_b)
    P_GG = (1 - a1) * (1 - a2)
    P_H = a1 * (1 - a2) + (1 - a1) * a2
    P_BB = a1 * a2
    return sp.simplify(P_GG), sp.simplify(P_H), sp.simplify(P_BB)


def cycle_tau_R_P(n_entry, x_entry, entry: str):
    P_GG, P_H, P_BB = return_step_distribution(x_entry, entry)
    return_cost = sp.simplify(1 - P_GG)  # E[path_b_loss] at the return step = 1 - P(GG)

    tau = sp.simplify((n_entry - 1) + 1 + P_GG * N)  # = n_entry + P_GG*N
    R = sp.simplify((n_entry - 1) * cost_a + return_cost + P_GG * 1 + 2 * c_switch_cold)

    # embedded transition: direct landing (P_H, P_BB) + GG-landing redistributed via GG's own exit
    P_to_H = sp.simplify(P_H + P_GG * f_GG_H)
    P_to_BB = sp.simplify(P_BB + P_GG * f_GG_BB)
    check = sp.simplify(P_to_H + P_to_BB - 1)
    return tau, R, P_to_H, P_to_BB, check


tau_H, R_H, PHH, PHBB, checkH = cycle_tau_R_P(n_H, x_H, "H")
tau_BB, R_BB, PBBH, PBBBB, checkBB = cycle_tau_R_P(n_BB, x_BB, "BB")

print("\nembedded transition row sums (should both be 0):", checkH, checkBB)
print("\ntau(H)  =", tau_H)
print("R(H)    =", R_H)
print("P(H->H) =", PHH, " P(H->BB) =", PHBB)
print("\ntau(BB) =", tau_BB)
print("R(BB)   =", R_BB)
print("P(BB->H)=", PBBH, " P(BB->BB)=", PBBBB)

# Embedded chain stationary distribution (left eigenvector nu P = nu, normalized).
Pmat = sp.Matrix([[PHH, PHBB], [PBBH, PBBBB]])
nu_H_sym, nu_BB_sym = sp.symbols("nu_H nu_BB", positive=True)
nu_vec = sp.Matrix([[nu_H_sym, nu_BB_sym]])
eqs = list((nu_vec * Pmat - nu_vec)) + [nu_H_sym + nu_BB_sym - 1]
sol = sp.solve(eqs[:2] + [eqs[-1]], [nu_H_sym, nu_BB_sym], dict=True)
print("\nembedded stationary distribution nu:", sol)

if sol:
    nu_H_val, nu_BB_val = sol[0][nu_H_sym], sol[0][nu_BB_sym]
    g_cold_expr = sp.simplify((nu_H_val * R_H + nu_BB_val * R_BB) / (nu_H_val * tau_H + nu_BB_val * tau_BB))
    print("\n=== g_cold(n_H, n_BB) symbolic (via nu solved exactly) ===")
    sp.pprint(g_cold_expr)

    # Numeric cross-check against the continuous solver's exact value: pi_b=0.1, lambda=0.5,
    # cost_a=0.30, c_switch_cold=0.10 -> true g=0.174069319431571 (always_cold_value_iteration,
    # stable across resolution 80-500, see WARM_COLD_PURE_GILBERT_NOTES.md).
    #
    # IMPORTANT: n_H=3, n_BB=4 here are the ORIGINAL TRACED VALUES (how many consecutive "stay"
    # decisions the continuous solver's policy grid shows before the first "switch", for this
    # parameter point) -- per the advisor's "clean restatement" convention this module uses
    # throughout (n_i = decay exponent directly, park-step count = n_i - 1). An earlier debugging
    # session mistakenly substituted n_H=2, n_BB=3 here (values from a now-abandoned "T^{n_e+1}"
    # convention where n_e meant something else) and got diff=0.005 -- NOT a formula bug, just a
    # substitution error. Always use the ORIGINAL traced values with this module's formulas.
    subs = {pi_b: sp.Rational(1, 10), lam: sp.Rational(1, 2), cost_a: sp.Rational(3, 10),
            c_switch_cold: sp.Rational(1, 10), n_H: 3, n_BB: 4,
            x_H: sp.Rational(1, 2) ** 3, x_BB: sp.Rational(1, 2) ** 4}
    g_numeric = g_cold_expr.subs(subs)
    print("\nNumeric check (pi_b=0.1, lambda=0.5, cost_a=0.30, c_switch_cold=0.10, n_H=3, n_BB=4):")
    print("g_cold (symbolic, evaluated) =", sp.N(g_numeric, 15))
    print("g_cold (continuous solver, true value)  = 0.174069319431571")
    print("diff =", sp.N(g_numeric, 15) - sp.Float("0.174069319431571"))
