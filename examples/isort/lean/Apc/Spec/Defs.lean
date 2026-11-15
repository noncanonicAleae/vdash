import Apc.Spec.Impl

namespace Apc.Spec

/-- A list of naturals is sorted in non-decreasing order. -/
def Sorted : List Nat → Prop
  | [] => True
  | [_] => True
  | x :: y :: r => x ≤ y ∧ Sorted (y :: r)

/-- A sorting function is correct if its output is sorted and a permutation of its input. -/
def Correct (f : List Nat → List Nat) : Prop :=
  ∀ l, Sorted (f l) ∧ (f l).Perm l

end Apc.Spec
