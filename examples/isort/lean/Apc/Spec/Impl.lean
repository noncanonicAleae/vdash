namespace Apc.Spec

/-- Insert `x` into `l` before the first element `y` with `x ≤ y`. -/
def insert (x : Nat) : List Nat → List Nat
  | [] => [x]
  | y :: ys => if x ≤ y then x :: y :: ys else y :: insert x ys

/-- Insertion sort by structural recursion. -/
def isort : List Nat → List Nat
  | [] => []
  | x :: xs => insert x (isort xs)

end Apc.Spec
