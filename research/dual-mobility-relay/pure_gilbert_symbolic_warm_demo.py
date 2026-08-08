"""Symbolic (sympy) derivation of g_warm for the SYMMETRIC pure-Gilbert hop pair, per the
Opus-advisor consultation (2026-07-19): builds the general machinery (channel stationary
distribution + one-step transition kernel, both policy-independent since the channel is
exogenous) rather than hardcoding the target closed forms, so this script is a genuine
independent check of the advisor's hand-derived formulas, not a transcription of them.

Reparametrization (advisor's recommendation): pi_b = P(Bad) = p_gb/(p_gb+p_bg), lambda = the
persistence eigenvalue = 1-p_gb-p_bg. p_gb = pi_b*(1-lambda), p_bg = (1-pi_b)*(1-lambda).

Pitfall checklist applied (per advisor):
  1. One-step-ahead: cost of routing B given last-observed c uses (T @ path_b_loss)[c], not
     path_b_loss[c] directly (the timing bug found earlier this session).
  2. Exogenous channel: stationary distribution pi_joint is policy-INDEPENDENT (product of each
     hop's own marginal stationary [1-pi_b, pi_b]) -- never solved as a policy-dependent system.
  3. Everything kept as exact Rational/symbolic, no floats, until final numeric substitution.

Run with: uv run --with sympy python pure_gilbert_symbolic_warm_demo.py
"""

from __future__ import annotations

import sympy as sp

pi_b, lam, cost_a, c_warm, c_switch_warm = sp.symbols("pi_b lambda cost_a c_warm c_switch_warm",
                                                        positive=True)

p_gb = pi_b * (1 - lam)
p_bg = (1 - pi_b) * (1 - lam)

# Single-hop transition matrix, rows/cols = [Good, Bad].
T1 = sp.Matrix([[1 - p_gb, p_gb], [p_bg, 1 - p_bg]])

# Joint (symmetric hop1=hop2) transition matrix over states [GG, GB, BG, BB] (index = s1*2+s2),
# via Kronecker product -- this IS the joint_transition_matrix(hop1, hop2, rho=0) construction.
T = sp.Matrix(sp.kronecker_product(T1, T1))
T = sp.simplify(T)

# Joint stationary distribution: product of each hop's own marginal stationary [1-pi_b, pi_b].
pi1 = sp.Matrix([[1 - pi_b, pi_b]])
pi_joint = sp.Matrix(sp.kronecker_product(pi1, pi1))  # row vector, order [GG,GB,BG,BB]
pi_joint = sp.simplify(pi_joint)

# Sanity check (symbolic): pi_joint is a LEFT eigenvector of T at eigenvalue 1, i.e. pi_joint @ T == pi_joint.
check = sp.simplify(pi_joint * T - pi_joint)
print("Stationarity check (should be zero matrix):", check)

# Pure-Gilbert path-B loss: 0 only at GG, 1 otherwise (states ordered GG,GB,BG,BB).
path_b_loss = sp.Matrix([0, 1, 1, 1])

# One-step-ahead predictive loss: E[path_b_loss(c') | last-observed c] = (T @ path_b_loss)[c].
predictive_loss = sp.simplify(T * path_b_loss)
print("\nPredictive path_b_loss per last-observed state [GG,GB,BG,BB]:")
sp.pprint(predictive_loss)


def g_always_A():
    return cost_a + c_warm


def g_always_B():
    """No switching ever occurs (context permanently B). g = c_warm + E_pi[predictive_loss]."""
    stationary_avg = sp.simplify((pi_joint * predictive_loss)[0, 0])
    return sp.simplify(c_warm + stationary_avg)


def g_route_b_iff_gg():
    """Policy: route to B next iff last-observed state == GG (state index 0), else route to A.
    Context is then a deterministic function of channel history (context_t = 1{c_{t-1}=GG}), so
    a switch occurs in a step iff exactly one of (c_{t-1}==GG), (c_t==GG) holds -- i.e. iff the
    "is GG" indicator changes between consecutive last-observed states.
    """
    GG = 0
    a = 1 - pi_b  # P(hop Good) marginal; pi_joint[GG] = a**2
    p_GG = pi_joint[0, GG]
    route_loss_expectation = sp.simplify(p_GG * predictive_loss[GG, 0] + (1 - p_GG) * cost_a)

    # P(switch) = P(c_{t-1}==GG, c_t!=GG) + P(c_{t-1}!=GG, c_t==GG). By stationarity these two
    # transition probabilities are equal (both reduce to pi_joint[GG] - pi_joint[GG,GG] under the
    # stationary distribution's own flow-conservation identity) -- verify this symbolically too,
    # not just trust the advisor's claim.
    p_GG_to_not_GG = pi_joint[0, GG] * (1 - T[GG, GG])
    p_not_GG_to_GG = 0
    for c in range(1, 4):
        p_not_GG_to_GG += pi_joint[0, c] * T[c, GG]
    p_not_GG_to_GG = sp.simplify(p_not_GG_to_GG)
    p_GG_to_not_GG = sp.simplify(p_GG_to_not_GG)
    print(f"\n  [route_b_iff_gg] P(GG->not GG) flow = {p_GG_to_not_GG}")
    print(f"  [route_b_iff_gg] P(not GG->GG) flow = {p_not_GG_to_GG}")
    flow_balance_check = sp.simplify(p_GG_to_not_GG - p_not_GG_to_GG)
    print(f"  [route_b_iff_gg] flow balance check (should be 0): {flow_balance_check}")

    p_switch = sp.simplify(p_GG_to_not_GG + p_not_GG_to_GG)
    g = c_warm + route_loss_expectation + c_switch_warm * p_switch
    return sp.simplify(g)


g_a = g_always_A()
g_b = g_always_B()
g_gg = g_route_b_iff_gg()

print("\n=== Derived closed forms (symmetric pure-Gilbert) ===")
print("g(always-A)      =", g_a)
print("g(always-B)      =", sp.factor(g_b))
print("g(route B iff GG)=", sp.simplify(g_gg))

# Advisor's claimed closed forms, for direct comparison.
a_sym = 1 - pi_b
q_G = 1 - p_gb
advisor_g_b = c_warm + pi_b * (2 - pi_b)
advisor_g_gg = c_warm + a_sym**2 * (1 - q_G**2) * (1 + 2 * c_switch_warm) + (1 - a_sym**2) * cost_a

print("\n=== Comparison against advisor's hand-derived formulas ===")
diff_b = sp.simplify(g_b - advisor_g_b)
diff_gg = sp.simplify(sp.expand(g_gg) - sp.expand(advisor_g_gg))
print("g(always-B) - advisor's formula      =", diff_b, "  (should be 0)")
print("g(route B iff GG) - advisor's formula =", diff_gg, "  (should be 0)")
