import Isort.Spec.Impl
import Isort.Spec.Mutants

namespace Isort.Spec

example : isort [3, 1, 2] = [1, 2, 3] := by decide

example : isortBad [1, 1] = [1] := by decide

end Isort.Spec
