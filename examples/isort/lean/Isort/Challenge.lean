import Isort.Spec.Impl
import Isort.Spec.Defs
import Isort.Spec.Mutants

namespace Isort

theorem isort_correct : Spec.Correct Spec.isort := by
  sorry

theorem isort_fixes_sorted (l : List Nat) (h : Spec.Sorted l) : Spec.isort l = l := by
  sorry

theorem sorted_nonvacuous : ∃ l : List Nat, 2 ≤ l.length ∧ Spec.Sorted l := by
  sorry

theorem isortBad_incorrect : ¬ Spec.Correct Spec.isortBad := by
  sorry

end Isort
