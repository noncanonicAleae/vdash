import Apc.Spec.Impl
import Apc.Spec.Mutants

namespace Apc.Spec

example : isort [3, 1, 2] = [1, 2, 3] := by decide

example : isortBad [1, 1] = [1] := by decide

end Apc.Spec
