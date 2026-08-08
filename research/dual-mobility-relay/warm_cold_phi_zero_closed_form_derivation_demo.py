"""Closed-form Phi=0 (g_warm*=g_cold*) boundary for the primary/default active cell identified by
`warm_cold_phi_zero_active_cell_demo.py`'s numeric sweep: warm-optimal policy = route-B-iff-GG,
cold-optimal policy = plateau (always-A-cold, g_cold*=cost_a). This is the active cell for
pi_b<=1/2 (real-data-relevant) parameter points at moderate cost_a -- see
WARM_COLD_PURE_GILBERT_NOTES.md, "PRIORITY (3): Phi=0 BOUNDARY -- FIRST CLOSED FORM FOUND".

Substitutes the two already-validated closed forms (g_warm route-B-iff-GG from
pure_gilbert_symbolic_warm_demo.py; g_cold plateau = cost_a trivially) and solves Phi=0 for
cost_a, giving
  cost_a* = c_warm/(1-pi_b)^2 + (1-q_G^2)*(1+2*c_switch_warm),  q_G = 1-pi_b*(1-lambda)
Verified to 5 decimal places against the exact 8-state warm policy-iteration solver (see
warm_cold_phi_zero_active_cell_demo.py's sweep and the bisection check referenced in the notes).

Run with: uv run --with sympy python warm_cold_phi_zero_closed_form_derivation_demo.py
"""

import sympy as sp

pi_b, lam, cost_a, c_warm, c_switch_warm = sp.symbols("pi_b lambda cost_a c_warm c_switch_warm", positive=True)

p_gb = pi_b*(1-lam)
a = 1-pi_b
q_G = 1-p_gb

# g_warm in route-B-iff-GG regime (validated closed form from pure_gilbert_symbolic_warm_demo.py)
g_warm_routeGG = c_warm + a**2*(1-q_G**2)*(1+2*c_switch_warm) + (1-a**2)*cost_a

# g_cold in plateau/always-A-cold regime
g_cold_plateau = cost_a

Phi = sp.simplify(g_warm_routeGG - g_cold_plateau)
print("Phi (route-B-iff-GG warm, plateau cold) =")
sp.pprint(Phi)

# Solve Phi=0 for cost_a
cost_a_star = sp.solve(sp.Eq(Phi, 0), cost_a)
print("\ncost_a* solutions:", cost_a_star)

for sol in cost_a_star:
    simplified = sp.simplify(sol)
    print("cost_a* =", simplified)
    numeric = simplified.subs({pi_b: sp.Rational(3,10), lam: sp.Rational(2,5),
                                c_warm: sp.Rational(2,100), c_switch_warm: sp.Rational(1,100)})
    print("numeric (pi_b=0.3, lambda=0.4, c_warm=0.02, c_switch_warm=0.01):", sp.N(numeric, 10))
