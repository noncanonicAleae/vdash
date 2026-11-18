open Lean Meta

namespace ApcTool

def kindOfConst : ConstantInfo → String
  | .axiomInfo _ => "axiom"
  | .defnInfo _ => "def"
  | .thmInfo _ => "theorem"
  | .opaqueInfo _ => "opaque"
  | .quotInfo _ => "quot"
  | .inductInfo _ => "inductive"
  | .ctorInfo _ => "ctor"
  | .recInfo _ => "rec"

def moduleOfConst (env : Environment) (n : Name) : String :=
  match env.getModuleIdxFor? n with
  | some idx =>
    let mods := env.header.moduleNames
    if h : idx.toNat < mods.size then
      (mods[idx.toNat]'h).toString
    else
      "<local>"
  | none => "<local>"

def serLevel : Level → String
  | .zero => "0"
  | .succ u => s!"S({serLevel u})"
  | .max u v => s!"M({serLevel u},{serLevel v})"
  | .imax u v => s!"I({serLevel u},{serLevel v})"
  | .param n => s!"P({n})"
  | .mvar id => s!"?({id.name})"

def serBinderInfo : BinderInfo → String
  | .default => "d"
  | .implicit => "i"
  | .strictImplicit => "s"
  | .instImplicit => "c"

structure DagState where
  memo : Std.HashMap String Nat := {}
  lines : Array String := #[]

def emitLine (line : String) : StateT DagState (Except String) Nat := do
  let st ← get
  match st.memo[line]? with
  | some idx => return idx
  | none =>
    let idx := st.lines.size
    set ({ memo := st.memo.insert line idx, lines := st.lines.push line } : DagState)
    return idx

partial def serExprNode : Expr → StateT DagState (Except String) Nat
  | .mdata _ e => serExprNode e
  | .bvar i => emitLine s!"b {i}"
  | .sort u => emitLine s!"s {serLevel u}"
  | .const n us =>
    let lvls := String.intercalate "," (us.map serLevel)
    emitLine s!"c {n} [{lvls}]"
  | .app f x => do
    let fi ← serExprNode f
    let xi ← serExprNode x
    emitLine s!"a {fi} {xi}"
  | .lam _ ty body bi => do
    let ti ← serExprNode ty
    let biIdx ← serExprNode body
    emitLine s!"l {serBinderInfo bi} {ti} {biIdx}"
  | .forallE _ ty body bi => do
    let ti ← serExprNode ty
    let biIdx ← serExprNode body
    emitLine s!"p {serBinderInfo bi} {ti} {biIdx}"
  | .letE _ ty val body _ => do
    let ti ← serExprNode ty
    let vi ← serExprNode val
    let biIdx ← serExprNode body
    emitLine s!"t {ti} {vi} {biIdx}"
  | .lit (.natVal n) => emitLine s!"n {n}"
  | .lit (.strVal s) => emitLine s!"x {(Json.str s).compress}"
  | .proj structName idx e => do
    let ei ← serExprNode e
    emitLine s!"j {structName} {idx} {ei}"
  | .fvar id => throw s!"unexpected fvar {id.name}"
  | .mvar id => throw s!"unexpected mvar {id.name}"

def serExprDag (e : Expr) : Except String String := do
  let (rootIdx, st) ← (serExprNode e).run ({} : DagState)
  let allLines := st.lines.push s!"r {rootIdx}"
  return String.intercalate "\n" allLines.toList

def serTargetType (cinfo : ConstantInfo) : Except String String := do
  let lparams := String.intercalate "," (cinfo.levelParams.map toString)
  let tyDag ← serExprDag cinfo.type
  return String.intercalate "\n--\n" [lparams, tyDag]

partial def exprConsts (e : Expr) (acc : Array Name := #[]) : Array Name :=
  match e with
  | .mdata _ b => exprConsts b acc
  | .const n _ => acc.push n
  | .app f x => exprConsts x (exprConsts f acc)
  | .lam _ t b _ => exprConsts b (exprConsts t acc)
  | .forallE _ t b _ => exprConsts b (exprConsts t acc)
  | .letE _ t v b _ => exprConsts b (exprConsts v (exprConsts t acc))
  | .proj structName _ b => (exprConsts b acc).push structName
  | _ => acc

partial def collectAuxDefs (env : Environment) (rootName : Name) (seed : Array Name) : Array ConstantInfo := Id.run do
  let mut queue : Array Name := seed
  let mut qIdx : Nat := 0
  let mut visited : Std.HashSet Name := ({} : Std.HashSet Name).insert rootName
  let mut out : Array ConstantInfo := #[]
  while h : qIdx < queue.size do
    let c := queue[qIdx]'h
    qIdx := qIdx + 1
    if !visited.contains c && rootName.isPrefixOf c then
      visited := visited.insert c
      if let some ci := env.find? c then
        out := out.push ci
        match ci with
        | .defnInfo v => queue := queue ++ exprConsts v.value (exprConsts ci.type)
        | .opaqueInfo v => queue := queue ++ exprConsts v.value (exprConsts ci.type)
        | _ => queue := queue ++ exprConsts ci.type
  return out.qsort (fun a b => a.name.toString < b.name.toString)

def serConstant (env : Environment) (cinfo : ConstantInfo) : Except String String := do
  let k := kindOfConst cinfo
  let lparams := String.intercalate "," (cinfo.levelParams.map toString)
  let tyDag ← serExprDag cinfo.type
  let mut parts : List String := [k, lparams, tyDag]
  match cinfo with
  | .defnInfo v =>
    let valDag ← serExprDag v.value
    parts := parts ++ [valDag]
    let seed := (exprConsts v.value).push (cinfo.name.str "_sunfold")
    for aux in collectAuxDefs env cinfo.name seed do
      match aux with
      | .defnInfo av =>
        let aty ← serExprDag aux.type
        let aval ← serExprDag av.value
        parts := parts ++ [aux.name.toString, aty, aval]
      | _ => pure ()
  | .opaqueInfo v =>
    let valDag ← serExprDag v.value
    parts := parts ++ [valDag]
  | .inductInfo v =>
    let ctors := String.intercalate "," (v.ctors.map toString)
    parts := parts ++ [s!"{v.numParams},{v.numIndices}:{ctors}"]
  | .ctorInfo v =>
    parts := parts ++ [s!"{v.induct},{v.cidx},{v.numParams},{v.numFields}"]
  | .recInfo v =>
    let mut rparts : List String := [String.intercalate "," (v.all.map toString)]
    for r in v.rules do
      let rhsDag ← serExprDag r.rhs
      rparts := rparts ++ [s!"{r.ctor}:{r.nfields}\n{rhsDag}"]
    parts := parts ++ rparts
  | .thmInfo _ | .axiomInfo _ | .quotInfo _ => pure ()
  return String.intercalate "\n--\n" parts

abbrev ConstMemo := Std.HashMap Name (Json × Array Name)

partial def buildClosureMemo (env : Environment) (seed : Array Name) (memoIn : ConstMemo) :
    Except String (List (String × Json) × ConstMemo) := do
  let mut queue : Array Name := seed
  let mut qIdx : Nat := 0
  let mut visited : Std.HashSet Name := {}
  let mut entries : Array (String × Json) := #[]
  let mut memo := memoIn

  while h : qIdx < queue.size do
    let c := queue[qIdx]'h
    qIdx := qIdx + 1
    if !visited.contains c then
      visited := visited.insert c
      if let some (obj, nextConsts) := memo[c]? then
        entries := entries.push (c.toString, obj)
        for nc in nextConsts do
          queue := queue.push nc
      else
        match env.find? c with
        | none => throw s!"unknown constant {c}"
        | some cinfo =>
          let modName := moduleOfConst env c
          if !modName.startsWith "Apc." then
            let obj := Json.mkObj [
              ("module", Json.str modName),
              ("ext", Json.bool true),
              ("ser", Json.null)
            ]
            memo := memo.insert c (obj, #[])
            entries := entries.push (c.toString, obj)
          else
            let ser ← serConstant env cinfo
            let obj := Json.mkObj [
              ("module", Json.str modName),
              ("ext", Json.bool false),
              ("ser", Json.str ser)
            ]
            let mut nextConsts := exprConsts cinfo.type
            match cinfo with
            | .defnInfo v => nextConsts := exprConsts v.value nextConsts
            | .opaqueInfo v => nextConsts := exprConsts v.value nextConsts
            | .thmInfo _ | .axiomInfo _ | .quotInfo _ => pure ()
            | .inductInfo v => nextConsts := nextConsts ++ v.ctors.toArray
            | .ctorInfo v => nextConsts := nextConsts.push v.induct
            | .recInfo v =>
              nextConsts := nextConsts ++ v.all.toArray
              for r in v.rules do
                nextConsts := exprConsts r.rhs nextConsts
            memo := memo.insert c (obj, nextConsts)
            entries := entries.push (c.toString, obj)
            for nc in nextConsts do
              queue := queue.push nc
  return (entries.toList, memo)

def fingerprintOneMemo (env : Environment) (n : Name) (memo : ConstMemo) :
    MetaM ((String × Json) × ConstMemo) := do
  match env.find? n with
  | none => throwError s!"missing constant {n}"
  | some cinfo =>
    let k := kindOfConst cinfo
    let modName := moduleOfConst env n
    let tySer ← match serTargetType cinfo with
      | .ok s => pure s
      | .error msg => throwError msg
    let (closEntries, memo') ← match buildClosureMemo env (exprConsts cinfo.type) memo with
      | .ok res => pure res
      | .error msg => throwError msg
    let axs ← Lean.collectAxioms n
    let axStrs := (axs.map toString).qsort (· < ·)
    let obj := Json.mkObj [
      ("kind", Json.str k),
      ("module", Json.str modName),
      ("type", Json.str tySer),
      ("axioms", Json.arr (axStrs.map Json.str)),
      ("closure", Json.mkObj closEntries)
    ]
    return ((n.toString, obj), memo')

def findChallengeTargets (env : Environment) : Array Name := Id.run do
  let targetMod : Name := (`Apc).str "Challenge"
  let mut out : Array Name := #[]
  for i in [:env.header.moduleNames.size] do
    if env.header.moduleNames[i]! == targetMod then
      let md := env.header.moduleData[i]!
      for cinfo in md.constants do
        if !cinfo.name.isInternal then
          match cinfo with
          | .thmInfo _ => out := out.push cinfo.name
          | _ => pure ()
  return out

def main (mode : String) (names : List String) : MetaM Unit := do
  let env ← getEnv
  if mode == "targets" then
    let tnames := findChallengeTargets env
    let mut pairs : Array (String × Json) := #[]
    let mut memo : ConstMemo := {}
    for n in tnames do
      let (p, memo') ← fingerprintOneMemo env n memo
      memo := memo'
      pairs := pairs.push p
    let j := Json.mkObj pairs.toList
    IO.println s!"APCFP {j.compress}"
  else if mode == "closure" then
    let mut pairs : Array (String × Json) := #[]
    let mut memo : ConstMemo := {}
    for s in names do
      let n := s.toName
      if (env.find? n).isSome then
        let (p, memo') ← fingerprintOneMemo env n memo
        memo := memo'
        pairs := pairs.push p
    let j := Json.mkObj pairs.toList
    IO.println s!"APCFP {j.compress}"
  else if mode == "axioms" then
    let mut pairs : Array (String × Json) := #[]
    for s in names do
      let n := s.toName
      match env.find? n with
      | none =>
        let obj := Json.mkObj [
          ("exists", Json.bool false),
          ("kind", Json.str ""),
          ("axioms", Json.arr #[]),
          ("statement", Json.null)
        ]
        pairs := pairs.push (s, obj)
      | some cinfo =>
        let k := kindOfConst cinfo
        let axs ← Lean.collectAxioms n
        let axStrs := (axs.map toString).qsort (· < ·)
        let fmt ← Meta.ppExpr cinfo.type
        let stmt := toString fmt
        let obj := Json.mkObj [
          ("exists", Json.bool true),
          ("kind", Json.str k),
          ("axioms", Json.arr (axStrs.map Json.str)),
          ("statement", Json.str stmt)
        ]
        pairs := pairs.push (s, obj)
    let j := Json.mkObj pairs.toList
    IO.println s!"APCFP {j.compress}"
  else
    throwError s!"unknown mode {mode}"

end ApcTool
