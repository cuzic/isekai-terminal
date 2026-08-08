"""Closed-form derivation of the cost_a threshold above which the pi_b>1/2 P(GG|return) bump
produces a genuine interior n_H* that beats the "park forever" plateau, in the single-entry toy
model g(n)=(A+B*n)/(n+C*P_GG(n)) (see WARM_COLD_PURE_GILBERT_NOTES.md, "CLOSED-FORM n_H*
BUMP-TRANSITION THRESHOLD" section, 2026-07-19).

Key trick: since B=cost_a exactly equals the plateau value g(n->infinity), the n-linear terms in
g(n)-cost_a cancel exactly, reducing "does any finite n beat the plateau" to a single inequality
P_GG(n) > A/(cost_a*C) with no n-dependence in the sign. Substituting the already-known P_GG
vertex (x*=(2*pi_b-1)/(2*pi_b), valid for pi_b>1/2) gives P_GG_max=(1-pi_b)/(4*pi_b) in closed
form, and solving the equality for cost_a gives the threshold
  cost_a* = (1+2*c_switch_cold) / (1 + N*(1-pi_b)/(4*pi_b))
which this script derives symbolically and evaluates numerically.

Run with: uv run --with sympy python pure_gilbert_nh_threshold_derivation_demo.py
"""

import sympy as sp

pi_b, lam, cost_a, c_switch_cold, x = sp.symbols("pi_b lambda cost_a c_switch_cold x", positive=True)

p_gb = pi_b*(1-lam)
q_G = 1 - p_gb
N = 1/(1-q_G**2)

# single-entry toy: A = 1+2*c_switch_cold-cost_a, B=cost_a, C=N
A = 1 + 2*c_switch_cold - cost_a
B = cost_a
C = N

P_GG = (1-pi_b)*(1-x)*(1-pi_b+pi_b*x)

# g(n) - cost_a numerator (after B=cost_a cancellation): A - cost_a*C*P_GG
expr = A - cost_a*C*P_GG
print("numerator of g(n)-cost_a (should just be A - cost_a*C*P_GG(x)):")
sp.pprint(sp.simplify(expr))

# vertex of P_GG in x
dPGG = sp.diff(P_GG, x)
xstar = sp.solve(sp.Eq(dPGG,0), x)
print("\nvertex x* =", xstar)

xstar_formula = (2*pi_b-1)/(2*pi_b)
print("check vertex formula (2pi_b-1)/(2pi_b) matches:", sp.simplify(xstar[0]-xstar_formula) if xstar else None)

P_GG_max = sp.simplify(P_GG.subs(x, xstar_formula))
print("\nP_GG_max =", P_GG_max, "  simplified:", sp.simplify(P_GG_max))

# threshold equation: P_GG_max = A/(cost_a*C)  =>  solve for cost_a
threshold_eq = sp.Eq(P_GG_max, A/(cost_a*C))
cost_a_thresh = sp.solve(threshold_eq, cost_a)
print("\ncost_a threshold solutions:", cost_a_thresh)

# numeric check at pi_b=0.8, lambda=0.85, c_switch_cold=0.05
for sol in cost_a_thresh:
    val = sol.subs({pi_b: sp.Rational(4,5), lam: sp.Rational(17,20), c_switch_cold: sp.Rational(1,20)})
    print("numeric cost_a* =", sp.N(val, 10))
