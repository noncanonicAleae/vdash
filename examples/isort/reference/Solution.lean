import Isort.Spec.Impl
import Isort.Spec.Defs
import Isort.Spec.Mutants

namespace Isort

private theorem insert_perm (x : Nat) : ∀ l : List Nat, (Spec.insert x l).Perm (x :: l)
  | [] => List.Perm.refl [x]
  | y :: ys => by
    unfold Spec.insert
    split
    · exact List.Perm.refl (x :: y :: ys)
    · exact List.Perm.trans (List.Perm.cons y (insert_perm x ys)) (List.Perm.swap x y ys)

private theorem isort_perm : ∀ l : List Nat, (Spec.isort l).Perm l
  | [] => List.Perm.refl []
  | x :: xs => by
    unfold Spec.isort
    exact List.Perm.trans (insert_perm x (Spec.isort xs)) (List.Perm.cons x (isort_perm xs))

private theorem insert_head_cases (x a : Nat) :
    ∀ ys : List Nat,
      (∃ r, Spec.insert x (a :: ys) = x :: r) ∨
      (∃ r, Spec.insert x (a :: ys) = a :: r) := by
  intro ys
  unfold Spec.insert
  split
  · exact Or.inl ⟨a :: ys, rfl⟩
  · exact Or.inr ⟨Spec.insert x ys, rfl⟩

private theorem insert_sorted (x : Nat) : ∀ l : List Nat, Spec.Sorted l → Spec.Sorted (Spec.insert x l)
  | [], _ => trivial
  | [y], _ => by
    unfold Spec.insert
    split
    · next hxy => exact ⟨hxy, trivial⟩
    · next hxy =>
      have hyx : y ≤ x := Nat.le_of_lt (Nat.gt_of_not_le hxy)
      exact ⟨hyx, trivial⟩
  | y :: z :: zs, h => by
    obtain ⟨hyz, hzs⟩ := h
    unfold Spec.insert
    split
    · next hxy => exact ⟨hxy, hyz, hzs⟩
    · next hxy =>
      have hyx : y ≤ x := Nat.le_of_lt (Nat.gt_of_not_le hxy)
      have ih : Spec.Sorted (Spec.insert x (z :: zs)) := insert_sorted x (z :: zs) hzs
      rcases insert_head_cases x z zs with ⟨r, hr⟩ | ⟨r, hr⟩
      · rw [hr] at ih ⊢
        exact ⟨hyx, ih⟩
      · rw [hr] at ih ⊢
        exact ⟨hyz, ih⟩

private theorem isort_sorted : ∀ l : List Nat, Spec.Sorted (Spec.isort l)
  | [] => trivial
  | x :: xs => by
    unfold Spec.isort
    exact insert_sorted x (Spec.isort xs) (isort_sorted xs)

theorem isort_correct : Spec.Correct Spec.isort := by
  intro l
  exact ⟨isort_sorted l, isort_perm l⟩

theorem isort_fixes_sorted : ∀ (l : List Nat), Spec.Sorted l → Spec.isort l = l
  | [], _ => rfl
  | [x], _ => rfl
  | x :: y :: r, h => by
    obtain ⟨hxy, hr⟩ := h
    have ih : Spec.isort (y :: r) = y :: r := isort_fixes_sorted (y :: r) hr
    unfold Spec.isort
    rw [ih]
    unfold Spec.insert
    rw [if_pos hxy]

theorem sorted_nonvacuous : ∃ l : List Nat, 2 ≤ l.length ∧ Spec.Sorted l := by
  refine ⟨[1, 2], ?_, ?_⟩
  · decide
  · exact ⟨by decide, trivial⟩

theorem isortBad_incorrect : ¬ Spec.Correct Spec.isortBad := by
  intro h
  have hperm : (Spec.isortBad [1, 1]).Perm [1, 1] := (h [1, 1]).2
  have hlen : (Spec.isortBad [1, 1]).length = ([1, 1] : List Nat).length := List.Perm.length_eq hperm
  revert hlen
  decide

end Isort
