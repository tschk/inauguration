//! Fallible scalar Core IR → typed MIR. This is not a machine lowering pass.
use crate::compiler::mir::*;
use crate::core_ir::{Decl, Expr, LoopKind, Stmt, Typ, UnifiedModule};
use std::collections::HashMap;
macro_rules! need {
    ($builder:ident, $expr:expr, $abrupt:expr) => {
        match $builder.required($expr)? {
            Some(value) => value,
            None => return Ok($abrupt),
        }
    };
}

/// Lower the admitted scalar subset, refusing every unrepresented source operation.
/// The boot path retains native_emit for globals, data and ABI features that
/// are not represented by the scalar typed-to-machine pass.
pub fn lower_to_mir(module: &UnifiedModule) -> Result<MirModule, String> {
    crate::core_ir_verifier::check_execution_contracts(module)?;
    if crate::core_ir::type_proofs::metadata_present(
        &module.decls,
        crate::core_ir::type_proofs::MetadataKind::RustCallables,
    ) {
        return Err(
            "pending Rust callable source metadata requires normalization before emission".into(),
        );
    }
    let verified = crate::core_ir_verifier::verify_module(
        module,
        &crate::core_ir_verifier::VerifyOptions {
            entry: None,
            require_entry: false,
        },
    );
    if !verified.ok {
        return Err(format!(
            "typed-mir: {}: {}",
            verified.reason_code.unwrap_or_default(),
            verified.reason.unwrap_or_default()
        ));
    }
    let mut signatures = HashMap::new();
    for decl in &module.decls {
        let Decl::Function {
            name,
            params,
            ret,
            type_params,
            ..
        } = decl
        else {
            return Err("typed-mir: non-function declarations are not represented yet".into());
        };
        if crate::core_atomic::signature(name).is_some() {
            return Err(format!(
                "typed-mir: canonical atomic intrinsic name is redeclared: {name}"
            ));
        }
        if crate::inrt::is_inrt_builtin(name) {
            return Err(format!(
                "typed-mir: runtime intrinsic name is redeclared: {name}"
            ));
        }
        if !type_params.is_empty() {
            return Err("typed-mir: generic functions require specialization".into());
        }
        let params = params
            .iter()
            .map(|(_, typ)| scalar_type(typ))
            .collect::<Result<Vec<_>, _>>()?;
        if params.contains(&Typ::Void) {
            return Err("typed-mir: Void parameter".into());
        }
        let ret = scalar_type(ret)?;
        if signatures.insert(name.clone(), (params, ret)).is_some() {
            return Err(format!("typed-mir: duplicate function {name}"));
        }
    }
    let mut result = MirModule::new();
    for decl in &module.decls {
        let Decl::Function {
            name, params, body, ..
        } = decl
        else {
            unreachable!()
        };
        let (param_types, ret) = &signatures[name];
        let mut builder = ScalarBuilder {
            function: MirFunction {
                name: name.clone(),
                instructions: vec![],
                vreg_count: 0,
                frame_size: 0,
                return_type: Some(ret.clone()),
                param_types: param_types.clone(),
                var_map: vec![],
                value_types: vec![],
                local_types: vec![],
                is_external: body.is_empty(),
            },
            signatures: &signatures,
            scopes: vec![HashMap::new()],
            loops: vec![],
            labels: 0,
            open: true,
        };
        for ((name, _), typ) in params.iter().zip(param_types) {
            let reg = builder.value(typ.clone())?;
            let local = builder.local(name, typ.clone())?;
            if !body.is_empty() {
                builder.emit(TypedOp::LocalStore { local, src: reg });
            }
        }
        if !body.is_empty() {
            builder.body(body, false)?;
            if builder.open {
                if *ret == Typ::Void {
                    builder.emit(TypedOp::ReturnValue(None));
                }
            }
        }
        result.functions.push(builder.function);
    }
    result.verify(None).map_err(|errors| errors.join("; "))?;
    Ok(result)
}

struct ScalarBuilder<'a> {
    function: MirFunction,
    signatures: &'a HashMap<String, (Vec<Typ>, Typ)>,
    scopes: Vec<HashMap<String, u32>>,
    loops: Vec<(String, String)>,
    labels: u32,
    open: bool,
}
impl ScalarBuilder<'_> {
    fn emit(&mut self, op: TypedOp) {
        self.function
            .instructions
            .push(mir(MirOp::Typed(op), vec![]));
    }
    fn value(&mut self, typ: Typ) -> Result<VReg, String> {
        let reg = u32::try_from(self.function.value_types.len())
            .map_err(|_| "typed-mir: too many values")?;
        self.function.value_types.push(typ);
        self.function.vreg_count = reg + 1;
        Ok(reg)
    }
    fn temp_local(&mut self, typ: Typ) -> Result<u32, String> {
        let id = u32::try_from(self.function.local_types.len())
            .map_err(|_| "typed-mir: too many locals")?;
        self.function.local_types.push(typ);
        Ok(id)
    }
    fn local(&mut self, name: &str, typ: Typ) -> Result<u32, String> {
        let id = self.temp_local(typ)?;
        self.scopes
            .last_mut()
            .ok_or("typed-mir: missing scope")?
            .insert(name.into(), id);
        self.function.var_map.push((name.into(), id));
        Ok(id)
    }
    fn lookup(&self, name: &str) -> Result<u32, String> {
        self.scopes
            .iter()
            .rev()
            .find_map(|scope| scope.get(name).copied())
            .ok_or_else(|| format!("typed-mir: unknown local {name}"))
    }
    fn label(&mut self) -> Result<String, String> {
        let id = self.labels;
        self.labels = id.checked_add(1).ok_or("typed-mir: too many labels")?;
        Ok(format!("block{id}"))
    }
    fn start(&mut self, label: String) {
        self.function
            .instructions
            .push(mir(MirOp::Label(label), vec![]));
        self.open = true;
    }
    fn jump(&mut self, label: String) {
        self.emit(TypedOp::Jump(label));
        self.open = false;
    }
    fn branch(&mut self, cond: VReg, yes: String, no: String) {
        self.emit(TypedOp::BranchIf {
            cond,
            then_label: yes,
            else_label: no,
        });
        self.open = false;
    }
    fn require(&self, reg: VReg, typ: &Typ) -> Result<(), String> {
        if self.function.value_types.get(reg as usize) == Some(typ) {
            Ok(())
        } else {
            Err(format!("typed-mir: expected {typ:?} value"))
        }
    }
    fn required(&mut self, expr: &Expr) -> Result<Option<VReg>, String> {
        let value = self.expr(expr)?;
        if value.is_none() && self.open {
            return Err("typed-mir: Void expression used as a value".into());
        }
        Ok(value)
    }
    fn body(&mut self, body: &[Stmt], scoped: bool) -> Result<(), String> {
        if scoped {
            self.scopes.push(HashMap::new());
        }
        for statement in body {
            // Validate even unreachable source shapes, never silently drop them.
            validate_statement(statement)?;
            if self.open {
                self.stmt(statement)?;
            }
        }
        if scoped {
            self.scopes.pop();
        }
        Ok(())
    }
    fn stmt(&mut self, statement: &Stmt) -> Result<(), String> {
        match statement {
            Stmt::Declare(name, typ) => {
                let typ = scalar_type(typ)?;
                if typ == Typ::Void {
                    return Err("typed-mir: unit declaration has no local storage".into());
                }
                self.local(name, typ)?;
            }
            Stmt::Let(name, annotation, value) => {
                let reg = need!(self, value, ());
                if !self.open {
                    return Ok(());
                }
                let actual = self.function.value_types[reg as usize].clone();
                let typ = annotation
                    .as_ref()
                    .map(scalar_type)
                    .transpose()?
                    .unwrap_or(actual.clone());
                if actual != typ || typ == Typ::Void {
                    return Err("typed-mir: local type mismatch".into());
                }
                let local = self.local(name, typ)?;
                self.emit(TypedOp::LocalStore { local, src: reg });
            }
            Stmt::Assign(name, value) => {
                let local = self.lookup(name)?;
                let reg = need!(self, value, ());
                if self.open {
                    self.require(reg, &self.function.local_types[local as usize])?;
                    self.emit(TypedOp::LocalStore { local, src: reg });
                }
            }
            Stmt::Expr(expr) => {
                self.expr(expr)?;
            }
            Stmt::Return(value) => {
                let value = if let Some(expr) = value {
                    self.expr(expr)?
                } else {
                    None
                };
                if self.open {
                    let ret = self.function.return_type.clone().unwrap_or(Typ::Void);
                    match value {
                        Some(reg) => self.require(reg, &ret)?,
                        None if ret == Typ::Void => {}
                        None => return Err("typed-mir: return value required".into()),
                    }
                    self.emit(TypedOp::ReturnValue(value));
                    self.open = false;
                }
            }
            Stmt::If {
                cond,
                then_body,
                else_body,
            } => {
                let reg = need!(self, cond, ());
                if !self.open {
                    return Ok(());
                }
                self.require(reg, &Typ::Bool)?;
                let yes = self.label()?;
                let no = self.label()?;
                let join = self.label()?;
                self.branch(reg, yes.clone(), no.clone());
                self.start(yes);
                self.body(then_body, true)?;
                let yes_open = self.open;
                if yes_open {
                    self.jump(join.clone());
                }
                self.start(no);
                self.body(else_body, true)?;
                let no_open = self.open;
                if no_open {
                    self.jump(join.clone());
                }
                if yes_open || no_open {
                    self.start(join);
                } else {
                    self.open = false;
                }
            }
            Stmt::Loop { kind, cond, body } => {
                if matches!(kind, LoopKind::For { .. }) {
                    return Err("typed-mir: For requires iterator lowering".into());
                }
                let header = self.label()?;
                let run = self.label()?;
                let end = self.label()?;
                self.jump(header.clone());
                self.start(header.clone());
                match cond {
                    Some(cond) => {
                        let reg = need!(self, cond, ());
                        if self.open {
                            self.require(reg, &Typ::Bool)?;
                            self.branch(reg, run.clone(), end.clone());
                        }
                    }
                    None => self.jump(run.clone()),
                }
                self.loops.push((header.clone(), end.clone()));
                self.start(run);
                self.body(body, true)?;
                if self.open {
                    self.jump(header);
                }
                self.loops.pop();
                self.start(end);
            }
            Stmt::Break | Stmt::Continue => {
                let (header, end) = self
                    .loops
                    .last()
                    .ok_or("typed-mir: jump outside loop")?
                    .clone();
                self.jump(if matches!(statement, Stmt::Break) {
                    end
                } else {
                    header
                });
            }
            _ => return Err("typed-mir: unsupported statement".into()),
        }
        Ok(())
    }
    fn expr(&mut self, expression: &Expr) -> Result<Option<VReg>, String> {
        match expression {
            Expr::IntLit(value) => {
                let dst = self.value(Typ::Int)?;
                self.emit(TypedOp::ScalarInt { dst, value: *value });
                Ok(Some(dst))
            }
            Expr::BoolLit(value) => {
                let dst = self.value(Typ::Bool)?;
                self.emit(TypedOp::ScalarBool { dst, value: *value });
                Ok(Some(dst))
            }
            Expr::UnitLit => Ok(None),
            Expr::Ident(name) => {
                let local = self.lookup(name)?;
                let dst = self.value(self.function.local_types[local as usize].clone())?;
                self.emit(TypedOp::LocalLoad { dst, local });
                Ok(Some(dst))
            }
            Expr::Unary { op, expr } => {
                let src = need!(self, expr, None);
                if !self.open {
                    return Ok(Some(src));
                }
                let typ = scalar_unary_result(op, &self.function.value_types[src as usize])?;
                let dst = self.value(typ)?;
                self.emit(TypedOp::Unary {
                    dst,
                    op: op.clone(),
                    src,
                });
                Ok(Some(dst))
            }
            Expr::Binary { op, lhs, rhs } if matches!(op.as_str(), "&&" | "and" | "||" | "or") => {
                let left = need!(self, lhs, None);
                if !self.open {
                    return Ok(Some(left));
                }
                self.require(left, &Typ::Bool)?;
                let result = self.temp_local(Typ::Bool)?;
                self.emit(TypedOp::LocalStore {
                    local: result,
                    src: left,
                });
                let evaluate = self.label()?;
                let join = self.label()?;
                if matches!(op.as_str(), "&&" | "and") {
                    self.branch(left, evaluate.clone(), join.clone());
                } else {
                    self.branch(left, join.clone(), evaluate.clone());
                }
                self.start(evaluate);
                let right = self.expr(rhs)?;
                if self.open {
                    let right = right.ok_or("typed-mir: lazy RHS must produce Bool")?;
                    self.require(right, &Typ::Bool)?;
                    self.emit(TypedOp::LocalStore {
                        local: result,
                        src: right,
                    });
                    self.jump(join.clone());
                }
                self.start(join);
                let dst = self.value(Typ::Bool)?;
                self.emit(TypedOp::LocalLoad { dst, local: result });
                Ok(Some(dst))
            }
            Expr::Binary { op, lhs, rhs } => {
                let left = need!(self, lhs, None);
                if !self.open {
                    return Ok(Some(left));
                }
                let right = need!(self, rhs, None);
                if !self.open {
                    return Ok(Some(right));
                }
                let lt = self.function.value_types[left as usize].clone();
                self.require(right, &lt)?;
                let typ = scalar_binary_type(op, &lt)?;
                let dst = self.value(typ)?;
                self.emit(TypedOp::Binary {
                    dst,
                    op: op.clone(),
                    lhs: left,
                    rhs: right,
                });
                Ok(Some(dst))
            }
            Expr::Call { callee, args } => {
                let Expr::Ident(target) = callee.as_ref() else {
                    return Err("typed-mir: indirect calls unsupported".into());
                };
                if target == crate::core_ir::COMPILER_UNREACHABLE {
                    if !args.is_empty() {
                        return Err("typed-mir: unreachable marker takes no arguments".into());
                    }
                    self.emit(TypedOp::Trap);
                    self.open = false;
                    return Ok(None);
                }
                if self.scopes.iter().any(|scope| scope.contains_key(target)) {
                    return Err("typed-mir: lexical callee is indirect".into());
                }
                let (params, ret) = self
                    .signatures
                    .get(target)
                    .cloned()
                    .ok_or_else(|| format!("typed-mir: unknown call {target}"))?;
                if params.len() != args.len() {
                    return Err("typed-mir: call arity mismatch".into());
                }
                let mut values = vec![];
                for (arg, typ) in args.iter().zip(params) {
                    let reg = need!(self, arg, None);
                    if !self.open {
                        return Ok(Some(reg));
                    }
                    self.require(reg, &typ)?;
                    values.push(reg);
                }
                let dst = if ret == Typ::Void {
                    None
                } else {
                    Some(self.value(ret)?)
                };
                self.emit(TypedOp::DirectCall {
                    dst,
                    target: target.clone(),
                    args: values,
                });
                Ok(dst)
            }
            Expr::Block { body, result } => {
                self.scopes.push(HashMap::new());
                self.body(body, false)?;
                let value = if self.open {
                    result
                        .as_ref()
                        .map(|expr| self.expr(expr))
                        .transpose()?
                        .flatten()
                } else {
                    None
                };
                self.scopes.pop();
                Ok(value)
            }
            Expr::Conditional {
                cond,
                then_expr,
                else_expr,
            } => {
                let cond = need!(self, cond, None);
                if !self.open {
                    return Ok(Some(cond));
                }
                self.require(cond, &Typ::Bool)?;
                let yes = self.label()?;
                let no = self.label()?;
                let join = self.label()?;
                self.branch(cond, yes.clone(), no.clone());
                self.start(yes);
                self.scopes.push(HashMap::new());
                let left = self.expr(then_expr)?;
                self.scopes.pop();
                let left_open = self.open;
                let result = if left_open {
                    left.map(|reg| self.temp_local(self.function.value_types[reg as usize].clone()))
                        .transpose()?
                } else {
                    None
                };
                if left_open {
                    if let (Some(local), Some(src)) = (result, left) {
                        self.emit(TypedOp::LocalStore { local, src });
                    }
                    self.jump(join.clone());
                }
                self.start(no);
                self.scopes.push(HashMap::new());
                let right = self.expr(else_expr)?;
                self.scopes.pop();
                let right_open = self.open;
                let mut result = result;
                if right_open {
                    match (result, left_open, right) {
                        (Some(local), _, Some(src)) => {
                            self.require(src, &self.function.local_types[local as usize])?;
                            self.emit(TypedOp::LocalStore { local, src });
                        }
                        (None, false, Some(src)) => {
                            let local =
                                self.temp_local(self.function.value_types[src as usize].clone())?;
                            self.emit(TypedOp::LocalStore { local, src });
                            result = Some(local);
                        }
                        (None, _, None) => {}
                        _ => return Err("typed-mir: conditional value mismatch".into()),
                    }
                    self.jump(join.clone());
                }
                if left_open || right_open {
                    self.start(join);
                    if let Some(local) = result {
                        let dst = self.value(self.function.local_types[local as usize].clone())?;
                        self.emit(TypedOp::LocalLoad { dst, local });
                        Ok(Some(dst))
                    } else {
                        Ok(None)
                    }
                } else {
                    self.open = false;
                    Ok(None)
                }
            }
            _ => Err("typed-mir: unsupported expression".into()),
        }
    }
}
fn validate_statement(statement: &Stmt) -> Result<(), String> {
    match statement {
        Stmt::Declare(_, typ) => {
            let typ = scalar_type(typ)?;
            if typ == Typ::Void {
                Err("typed-mir: unit declaration has no local storage".into())
            } else {
                Ok(())
            }
        }
        Stmt::Let(_, annotation, value) => {
            if let Some(typ) = annotation {
                scalar_type(typ)?;
            }
            validate_expression(value)
        }
        Stmt::Assign(_, value) | Stmt::Expr(value) => validate_expression(value),
        Stmt::Return(value) => value.as_ref().map_or(Ok(()), validate_expression),
        Stmt::If {
            cond,
            then_body,
            else_body,
        } => {
            validate_expression(cond)?;
            for stmt in then_body.iter().chain(else_body) {
                validate_statement(stmt)?;
            }
            Ok(())
        }
        Stmt::Loop { kind, cond, body } => {
            if matches!(kind, LoopKind::For { .. }) {
                return Err("typed-mir: For requires iterator lowering".into());
            }
            if let Some(cond) = cond {
                validate_expression(cond)?;
            }
            for stmt in body {
                validate_statement(stmt)?;
            }
            Ok(())
        }
        Stmt::Break | Stmt::Continue => Ok(()),
        _ => Err("typed-mir: unsupported statement shape".into()),
    }
}
fn validate_expression(expression: &Expr) -> Result<(), String> {
    match expression {
        Expr::IntLit(_) | Expr::BoolLit(_) | Expr::UnitLit | Expr::Ident(_) => Ok(()),
        Expr::Unary { op, expr } => {
            if !matches!(op.as_str(), "-" | "neg" | "~" | "bitnot" | "!" | "not") {
                return Err("typed-mir: unsupported unary operator".into());
            }
            validate_expression(expr)
        }
        Expr::Binary { op, lhs, rhs } => {
            if !matches!(
                op.as_str(),
                "+" | "add"
                    | "-"
                    | "sub"
                    | "*"
                    | "mul"
                    | "/"
                    | "div"
                    | "%"
                    | "rem"
                    | "udiv"
                    | "urem"
                    | "&"
                    | "bitand"
                    | "|"
                    | "bitor"
                    | "^"
                    | "xor"
                    | "bitxor"
                    | "<<"
                    | "shl"
                    | ">>"
                    | "shr"
                    | "lshr"
                    | "<"
                    | "<="
                    | ">"
                    | ">="
                    | "=="
                    | "eq"
                    | "!="
                    | "neq"
                    | "&&"
                    | "and"
                    | "||"
                    | "or"
            ) {
                return Err("typed-mir: unsupported binary operator".into());
            }
            validate_expression(lhs)?;
            validate_expression(rhs)
        }
        Expr::Call { callee, args } => {
            if !matches!(callee.as_ref(), Expr::Ident(_)) {
                return Err("typed-mir: indirect call".into());
            }
            for arg in args {
                validate_expression(arg)?;
            }
            Ok(())
        }
        Expr::Block { body, result } => {
            for stmt in body {
                validate_statement(stmt)?;
            }
            result
                .as_ref()
                .map_or(Ok(()), |expr| validate_expression(expr))
        }
        Expr::Conditional {
            cond,
            then_expr,
            else_expr,
        } => {
            validate_expression(cond)?;
            validate_expression(then_expr)?;
            validate_expression(else_expr)
        }
        _ => Err("typed-mir: unsupported expression shape".into()),
    }
}

#[cfg(test)]
#[path = "mir_typed_tests.rs"]
mod scalar_typed_tests;

/// Lower a Core IR module for the boot image: MIR plus freestanding x86_64
/// (or AArch64) machine code with C-string literals, and the module's
/// function export table as `(name, code offset)` pairs for the kernel's
/// boot-header export table.
pub fn lower_boot_image(
    module: &UnifiedModule,
    entry: &str,
    target_triple: Option<&str>,
) -> Result<(MirModule, Vec<u8>, Vec<(String, u32)>, Vec<u8>), String> {
    lower_boot_image_with_data_base(module, entry, target_triple, 0x200000)
}

/// Boot packagers choose a page-aligned writable section after code/exports.
/// The backend owns both instruction references and nested payload fixups.
pub fn lower_boot_image_with_data_base(
    module: &UnifiedModule,
    entry: &str,
    target_triple: Option<&str>,
    data_base: u64,
) -> Result<(MirModule, Vec<u8>, Vec<(String, u32)>, Vec<u8>), String> {
    let triple = target_triple.unwrap_or("x86_64-unknown-none");
    let (code, exports, data) = if triple.contains("aarch64") || triple.contains("arm64") {
        let result = crate::native_emit::lower::lower_module(
            module,
            entry,
            crate::native_emit::NativeLinkage::Executable,
        )?;
        if !result.global_relocs.is_empty() || !result.data_relocations.is_empty() {
            return Err(
                "native-lower: AArch64 boot images do not yet relocate writable globals".into(),
            );
        }
        (
            result.code,
            result
                .exports
                .iter()
                .map(|s| (s.name.clone(), s.offset))
                .collect(),
            Vec::new(),
        )
    } else {
        let is_32bit = triple.starts_with("i386-") || triple.starts_with("i686-");
        crate::native_emit::x86_64::set_32bit(is_32bit);
        let result = crate::native_emit::x86_64_lower::lower_module_with_bases_layout(
            module,
            entry,
            0x101100,
            data_base,
            crate::native_emit::x86_64_lower::X86StringLayout::Cstring,
        )?;
        result.require_resolved_imports()?;
        if is_32bit && (!result.data.is_empty() || !result.global_relocs.is_empty()) {
            return Err(
                "native-lower: 32-bit x86 boot images do not yet relocate writable globals".into(),
            );
        }
        // References and payload pointers are already fixed to data_base by
        // the lowerer. The boot packager must load this data at that address.
        (result.code, result.exports, result.data)
    };
    let mir_module = MirModule::new();
    Ok((mir_module, code, exports, data))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boot_data_relocation_updates_instructions_and_nested_payloads() {
        let module=crate::in_lang_parse::parse_in_source(
            "var numbers:[Int]=[20,22];var texts:[String]=[\"left\",\"right\"];var text:String=\"λ\";fn main()->Int{return numbers[0]+numbers[1];}"
        ).unwrap();
        let (_, original, exports, data) =
            lower_boot_image_with_data_base(&module, "main", Some("x86_64-unknown-none"), 0x200000)
                .unwrap();
        let (_, relocated, new_exports, new_data) =
            lower_boot_image_with_data_base(&module, "main", Some("x86_64-unknown-none"), 0x301000)
                .unwrap();
        assert_eq!(original.len(), relocated.len());
        assert_eq!(exports, new_exports);
        assert_eq!(data.len(), new_data.len());
        assert_ne!(original, relocated);
        let mut pointers = 0;
        for (old, new) in data
            .as_chunks::<8>()
            .0
            .iter()
            .zip(new_data.as_chunks::<8>().0)
        {
            let old = u64::from_le_bytes(*old);
            let new = u64::from_le_bytes(*new);
            if (0x200000..0x200000 + data.len() as u64).contains(&old) {
                assert_eq!(new, old - 0x200000 + 0x301000);
                pointers += 1;
            } else {
                assert_eq!(old, new);
            }
        }
        assert!(
            pointers >= 3,
            "array-element and scalar string payload pointers must all relocate"
        );
    }

    #[test]
    fn scalar_mir_refuses_reserved_atomic_redeclarations() {
        for name in crate::core_atomic::names() {
            let source =
                format!("fn {name}()->Int{{return 42;}}fn main()->Int{{return {name}();}}");
            let module = crate::in_lang_parse::parse_in_source(&source).unwrap();
            let error = lower_to_mir(&module).unwrap_err();
            assert!(
                error.contains("canonical atomic intrinsic name is redeclared"),
                "{name}: {error}"
            );
        }
    }

    #[test]
    fn scalar_mir_refuses_runtime_intrinsic_redeclarations() {
        for name in crate::inrt::INRT_BUILTINS {
            let source =
                format!("fn {name}()->Int{{return 42;}}fn main()->Int{{return {name}();}}");
            let module = crate::in_lang_parse::parse_in_source(&source).unwrap();
            let error = lower_to_mir(&module).unwrap_err();
            assert!(
                error.contains("runtime intrinsic name is redeclared"),
                "{name}: {error}"
            );
        }
    }

    #[test]
    fn x86_boot_rejects_unresolved_imports() {
        let module = crate::in_lang_parse::parse_in_source(
            "extern c fn missing() -> Int;\nfn main() -> Int { return missing(); }",
        )
        .unwrap();
        let error = lower_boot_image(&module, "main", Some("x86_64-unknown-none")).unwrap_err();
        assert!(
            error.contains("unresolved runtime imports: missing"),
            "{error}"
        );
    }

    #[test]
    fn x86_boot_retains_initialized_scalars_and_pointer_payloads() {
        let scalar = crate::in_lang_parse::parse_in_source(
            "var count:Int=3;\nfn main()->Int{count=count+39;return count;}",
        )
        .unwrap();
        let (_, code, _, data) =
            lower_boot_image(&scalar, "main", Some("x86_64-unknown-none")).unwrap();
        assert!(!code.is_empty());
        assert_eq!(i64::from_le_bytes(data[..8].try_into().unwrap()), 3);
        let payload = crate::in_lang_parse::parse_in_source(
            "var text:String=\"λ\0x\";\nvar values:[String]=[\"same\",\"other\"];\nfn main()->Int{if text==\"λ\0x\" && values[0]==\"same\"{return 42;}return 0;}"
        )
        .unwrap();
        let (_, _, _, data) =
            lower_boot_image(&payload, "main", Some("x86_64-unknown-none")).unwrap();
        for (slot, expected) in [
            (0, "λ\0x\0".as_bytes()),
            (24, b"same\0".as_slice()),
            (32, b"other\0".as_slice()),
        ] {
            let pointer = u64::from_le_bytes(data[slot..slot + 8].try_into().unwrap());
            let offset = usize::try_from(pointer.checked_sub(0x200000).unwrap()).unwrap();
            assert_eq!(&data[offset..offset + expected.len()], expected);
        }
        assert_eq!(u64::from_le_bytes(data[8..16].try_into().unwrap()), 2);
    }

    #[test]
    fn x86_boot_preserves_32bit_writable_data_refusal() {
        let module = crate::in_lang_parse::parse_in_source(
            "var count:Int=3;\nfn main()->Int{return count;}",
        )
        .unwrap();
        let error = lower_boot_image(&module, "main", Some("i686-unknown-none")).unwrap_err();
        assert!(error.contains("32-bit x86 boot images"), "{error}");
    }

    #[test]
    fn aarch64_boot_rejects_unrelocated_globals() {
        let module = crate::in_lang_parse::parse_in_source(
            "var count: Int = 3;\nfn main() -> Int { return count; }",
        )
        .expect("parse");
        let error = lower_boot_image(&module, "main", Some("aarch64-unknown-none"))
            .expect_err("reject unmapped globals");
        assert!(
            error.contains("do not yet relocate writable globals"),
            "{error}"
        );
    }
}

#[cfg(test)]
mod declaration_storage_tests {
    use super::*;
    #[test]
    fn only_assignment_defines_a_declared_mir_local() {
        let mut module = crate::in_lang_parse::parse_in_source(
            "fn main()->Int{let value:Int=7;value=42;return value;}",
        )
        .unwrap();
        let Decl::Function { body, .. } = &mut module.decls[0] else {
            panic!("function")
        };
        body[0] = Stmt::Declare("value".into(), Typ::Int);
        let lowered = lower_to_mir(&module).unwrap();
        let function = &lowered.functions[0];
        assert_eq!(function.local_types, vec![Typ::Int]);
        assert_eq!(
            function
                .instructions
                .iter()
                .filter(|inst| matches!(inst.op, MirOp::Typed(TypedOp::LocalStore { .. })))
                .count(),
            1
        );
        assert!(
            !function
                .instructions
                .iter()
                .any(|inst| matches!(inst.op, MirOp::Typed(TypedOp::ScalarInt { value: 7, .. })))
        );
    }
}
