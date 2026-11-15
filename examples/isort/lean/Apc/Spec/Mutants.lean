namespace Apc.Spec

/-- Buggy insert that drops `x` when it equals an existing element. -/
def insertBad (x : Nat) : List Nat → List Nat
  | [] => [x]
  | y :: ys =>
    if x < y then x :: y :: ys
    else if x == y then y :: ys
    else y :: insertBad x ys

/-- Buggy insertion sort built on `insertBad`. -/
def isortBad : List Nat → List Nat
  | [] => []
  | x :: xs => insertBad x (isortBad xs)

end Apc.Spec
