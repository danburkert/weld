use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::BTreeMap;

use easy_ll;

use weld_common::WeldRuntimeErrno;

use super::ast::*;
use super::ast::Type::*;
use super::ast::LiteralKind::*;
use super::ast::ScalarKind::*;
use super::ast::BuilderKind::*;
use super::code_builder::CodeBuilder;
use super::conf::LogLevel;
use super::error::*;
use super::macro_processor;
use super::passes::*;
use super::pretty_print::*;
use super::program::Program;
use super::sir;
use super::sir::*;
use super::sir::Statement::*;
use super::sir::Terminator::*;
use super::transforms;
use super::type_inference;
use super::util::IdGenerator;
use super::util::MERGER_BC;

#[cfg(test)]
use super::parser::*;

static PRELUDE_CODE: &'static str = include_str!("resources/prelude.ll");
static VECTOR_CODE: &'static str = include_str!("resources/vector.ll");
static VVECTOR_CODE: &'static str = include_str!("resources/vvector.ll");
static MERGER_CODE: &'static str = include_str!("resources/merger/merger.ll");
static DICTIONARY_CODE: &'static str = include_str!("resources/dictionary.ll");
static DICTMERGER_CODE: &'static str = include_str!("resources/dictmerger.ll");
static GROUPMERGER_CODE: &'static str = include_str!("resources/groupbuilder.ll");

/// A wrapper for a struct passed as input to the Weld runtime.
#[derive(Clone, Debug)]
#[repr(C)]
pub struct WeldInputArgs {
    pub input: i64,
    pub nworkers: i32,
    pub mem_limit: i64,
}

/// A wrapper for outputs passed out of the Weld runtime.
#[derive(Clone, Debug)]
#[repr(C)]
pub struct WeldOutputArgs {
    pub output: i64,
    pub run_id: i64,
    pub errno: WeldRuntimeErrno,
}

/// Generate a compiled LLVM module from a program whose body is a function.
pub fn compile_program(program: &Program,
                       opt_passes: &Vec<Pass>,
                       log_level: LogLevel)
                       -> WeldResult<easy_ll::CompiledModule> {
    let mut expr = try!(macro_processor::process_program(program));
    if log_level >= LogLevel::Debug {
        println!("After macro substitution:\n{}\n", print_expr(&expr));
    }

    let _ = try!(transforms::uniquify(&mut expr));
    try!(type_inference::infer_types(&mut expr));
    let mut expr = try!(expr.to_typed());
    if log_level >= LogLevel::Debug {
        println!("After type inference:\n{}\n", print_expr(&expr));
    }

    for pass in opt_passes {
        try!(pass.transform(&mut expr));
        if log_level >= LogLevel::Debug {
            println!("After {} pass:\n{}", pass.pass_name(), print_expr(&expr));
        }
    }

    try!(transforms::uniquify(&mut expr));
    if log_level >= LogLevel::Debug {
        println!("After uniquify:\n{}\n", print_expr(&expr));
    }

    let sir_prog = try!(sir::ast_to_sir(&expr));
    if log_level >= LogLevel::Debug {
        println!("SIR program:\n{}\n", &sir_prog);
    }

    let mut gen = LlvmGenerator::new();
    try!(gen.add_function_on_pointers("run", &sir_prog));
    let llvm_code = gen.result();
    if log_level >= LogLevel::Debug {
        println!("LLVM program:\n{}\n", &llvm_code);
    }

    Ok(try!(easy_ll::compile_module(&llvm_code, Some(MERGER_BC))))
}

/// Generates a small program which, when called with a `run_id`, frees
/// memory associated with the run ID.
pub fn generate_runtime_interface_module() -> WeldResult<easy_ll::CompiledModule> {
    let program = include_str!("resources/runtime_interface_module.ll");
    Ok(try!(easy_ll::compile_module(program, None)))
}

/// Generates LLVM code for one or more modules.
pub struct LlvmGenerator {
    /// LLVM type name of the form %s0, %s1, etc for each struct generated.
    struct_names: HashMap<Vec<Type>, String>,
    struct_ids: IdGenerator,

    /// LLVM type name of the form %v0, %v1, etc for each vec generated.
    vec_names: HashMap<Type, String>,
    vec_ids: IdGenerator,

    // LLVM type names for each merger type.
    merger_names: HashMap<Type, String>,
    merger_ids: IdGenerator,

    /// LLVM type name of the form %d0, %d1, etc for each dict generated.
    dict_names: HashMap<Type, String>,
    dict_ids: IdGenerator,

    /// LLVM type names for various builder types
    bld_names: HashMap<BuilderKind, String>,

    /// LLVM SIMD vector names for various scalar types.
    simd_names: HashMap<ScalarKind, String>,

    /// A CodeBuilder and ID generator for prelude functions such as type and struct definitions.
    prelude_code: CodeBuilder,
    prelude_var_ids: IdGenerator,

    /// A CodeBuilder for body functions in the module.
    body_code: CodeBuilder,

    /// Functions we have already visited when generating code.
    visited: HashSet<sir::FunctionId>,
}

impl LlvmGenerator {
    pub fn new() -> LlvmGenerator {
        let mut generator = LlvmGenerator {
            struct_names: HashMap::new(),
            struct_ids: IdGenerator::new("%s"),
            vec_names: HashMap::new(),
            vec_ids: IdGenerator::new("%v"),
            merger_names: HashMap::new(),
            merger_ids: IdGenerator::new("%m"),
            dict_names: HashMap::new(),
            dict_ids: IdGenerator::new("%d"),
            simd_names: HashMap::new(),
            bld_names: HashMap::new(),
            prelude_code: CodeBuilder::new(),
            prelude_var_ids: IdGenerator::new("%p.p"),
            body_code: CodeBuilder::new(),
            visited: HashSet::new(),
        };
        generator.prelude_code.add(PRELUDE_CODE);
        generator.prelude_code.add("\n");
        generator
    }

    /// Return all the code generated so far.
    pub fn result(&mut self) -> String {
        format!("; PRELUDE:\n\n{}\n; BODY:\n\n{}", self.prelude_code.result(), self.body_code.result())
    }

    fn get_arg_str(&mut self, params: &HashMap<Symbol, Type>, suffix: &str) -> WeldResult<String> {
        let mut arg_types = String::new();
        let params_sorted: BTreeMap<&Symbol, &Type> = params.iter().collect();
        for (arg, ty) in params_sorted.iter() {
            let arg_str = format!("{} {}{}, ", try!(self.llvm_type(&ty)), llvm_symbol(&arg), suffix);
            arg_types.push_str(&arg_str);
        }
        arg_types.push_str("%work_t* %cur.work");
        Ok(arg_types)
    }

    fn unload_arg_struct(&mut self, params: &HashMap<Symbol, Type>, ctx: &mut FunctionContext) -> WeldResult<()> {
        let params_sorted: BTreeMap<&Symbol, &Type> = params.iter().collect();
        let ll_ty = try!(self.llvm_type(&Struct(params_sorted.iter().map(|p| p.1.clone()).cloned().collect())));
        let storage_typed = ctx.var_ids.next();
        let storage = ctx.var_ids.next();
        let work_data_ptr = ctx.var_ids.next();
        let work_data = ctx.var_ids.next();
        ctx.code.add(format!("{} = getelementptr %work_t, %work_t* %cur.work, i32 0, i32 0", work_data_ptr));
        ctx.code.add(format!("{} = load i8*, i8** {}", work_data, work_data_ptr));
        ctx.code.add(format!("{} = bitcast i8* {} to {}*", storage_typed, work_data, ll_ty));
        ctx.code.add(format!("{} = load {}, {}* {}", storage, ll_ty, ll_ty, storage_typed));
        for (i, (arg, _)) in params_sorted.iter().enumerate() {
            ctx.code.add(format!("{} = extractvalue {} {}, {}", llvm_symbol(arg), ll_ty, storage, i));
        }
        Ok(())
    }

    fn create_new_pieces(&mut self, params: &HashMap<Symbol, Type>, ctx: &mut FunctionContext) -> WeldResult<()> {
        let full_task_ptr = ctx.var_ids.next();
        let full_task_int = ctx.var_ids.next();
        let full_task_bit = ctx.var_ids.next();
        ctx.code.add(format!("{} = getelementptr %work_t, %work_t* %cur.work, i32 0, i32 4", full_task_ptr));
        ctx.code.add(format!("{} = load i32, i32* {}", full_task_int, full_task_ptr));
        ctx.code.add(format!("{} = trunc i32 {} to i1", full_task_bit, full_task_int));
        ctx.code.add(format!("br i1 {}, label %new_pieces, label %fn_call", full_task_bit));
        ctx.code.add("new_pieces:");
        let params_sorted: BTreeMap<&Symbol, &Type> = params.iter().collect();
        for (arg, ty) in params_sorted.iter() {
            match **ty {
                Builder(ref bk, _) => {
                    match *bk {
                        Appender(_) => {
                            let bld_ty_str = try!(self.llvm_type(ty)).to_string();
                            let bld_prefix = format!("@{}", bld_ty_str.replace("%", ""));
                            ctx.code.add(format!("call void {}.newPiece({} {}, %work_t* %cur.work)",
                                                 bld_prefix,
                                                 bld_ty_str,
                                                 llvm_symbol(arg)));
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
        ctx.code.add("br label %fn_call");
        Ok(())
    }

    fn get_arg_struct(&mut self, params: &HashMap<Symbol, Type>, ctx: &mut FunctionContext) -> WeldResult<String> {
        let params_sorted: BTreeMap<&Symbol, &Type> = params.iter().collect();
        let mut prev_ref = String::from("undef");
        let ll_ty = try!(self.llvm_type(&Struct(params_sorted.iter().map(|p| p.1.clone()).cloned().collect())))
            .to_string();
        for (i, (arg, ty)) in params_sorted.iter().enumerate() {
            let next_ref = ctx.var_ids.next();
            ctx.code.add(format!("{} = insertvalue {} {}, {} {}, {}",
                                 next_ref,
                                 ll_ty,
                                 prev_ref,
                                 try!(self.llvm_type(&ty)),
                                 llvm_symbol(arg),
                                 i));
            prev_ref.clear();
            prev_ref.push_str(&next_ref);
        }
        let struct_size_ptr = ctx.var_ids.next();
        let struct_size = ctx.var_ids.next();
        let struct_storage = ctx.var_ids.next();
        let struct_storage_typed = ctx.var_ids.next();
        ctx.code.add(format!("{} = getelementptr {}, {}* null, i32 1", struct_size_ptr, ll_ty, ll_ty));
        ctx.code.add(format!("{} = ptrtoint {}* {} to i64", struct_size, ll_ty, struct_size_ptr));
        // we use regular malloc here because this pointer will always be freed by parlib
        ctx.code.add(format!("{} = call i8* @malloc(i64 {})", struct_storage, struct_size));
        ctx.code.add(format!("{} = bitcast i8* {} to {}*", struct_storage_typed, struct_storage, ll_ty));
        ctx.code.add(format!("store {} {}, {}* {}", ll_ty, prev_ref, ll_ty, struct_storage_typed));
        Ok(struct_storage)
    }

    /// Add a function to the generated program.
    pub fn add_function(&mut self,
                        sir: &SirProgram,
                        func: &SirFunction,
                        // non-None only if func is loop body
                        containing_loop: Option<ParallelForData>)
                        -> WeldResult<()> {
        if !self.visited.insert(func.id) {
            return Ok(());
        }

        let mut ctx = &mut FunctionContext::new();
        let mut arg_types = try!(self.get_arg_str(&func.params, ".in"));
        if containing_loop.is_some() {
            arg_types.push_str(", i64 %lower.idx, i64 %upper.idx");
        }

        // Start the entry block by defining the function and storing all its arguments on the
        // stack (this makes them consistent with other local variables). Later, expressions may
        // add more local variables to alloca_code.
        ctx.alloca_code.add(format!("define void @f{}({}) {{", func.id, arg_types));
        ctx.alloca_code.add(format!("fn.entry:"));
        for (arg, ty) in func.params.iter() {
            let arg_str = llvm_symbol(&arg);
            let ty_str = self.llvm_type(&ty)?.to_string();
            ctx.add_alloca(&arg_str, &ty_str)?;
            ctx.code.add(format!("store {} {}.in, {}* {}", ty_str, arg_str, ty_str, arg_str));
        }
        for (arg, ty) in func.locals.iter() {
            let arg_str = llvm_symbol(&arg);
            let ty_str = try!(self.llvm_type(&ty)).to_string();
            ctx.add_alloca(&arg_str, &ty_str)?;
        }

        // Get the current thread ID
        ctx.code.add(format!("%cur.tid = call i32 @my_id_public()"));

        // If we're in a loop, generate loop iteration code
        if containing_loop.is_some() {
            let par_for = containing_loop.clone().unwrap();
            let bld_ty_str = self.llvm_type(func.params.get(&par_for.builder).unwrap())?.to_string();
            let bld_param_str = llvm_symbol(&par_for.builder);
            let bld_arg_str = llvm_symbol(&par_for.builder_arg);
            ctx.code.add(format!("store {} {}.in, {}* {}", &bld_ty_str, bld_param_str, &bld_ty_str, bld_arg_str));
            ctx.add_alloca("%cur.idx", "i64")?;
            ctx.code.add("store i64 %lower.idx, i64* %cur.idx");
            ctx.code.add("br label %loop.start");
            ctx.code.add("loop.start:");
            let idx_tmp = self.load_var("%cur.idx", "i64", ctx)?;
            if !par_for.innermost {
                let work_idx_ptr = ctx.var_ids.next();
                ctx.code.add(format!(
                    "{} = getelementptr %work_t, %work_t* %cur.work, i32 0, i32 3",
                    work_idx_ptr
                ));
                ctx.code.add(format!("store i64 {}, i64* {}", idx_tmp, work_idx_ptr));
            }

            let elem_ty = func.locals.get(&par_for.data_arg).unwrap();

            let idx_cmp = ctx.var_ids.next();

            if par_for.data[0].kind == IterKind::SimdIter {
                let check_with_vec = ctx.var_ids.next();
                let vector_len = format!("{}", vec_size(&elem_ty)?);
                // Would need to compute stride, etc. here.
                ctx.code.add(format!("{} = add i64 {}, {}", check_with_vec, idx_tmp, vector_len));
                ctx.code.add(format!("{} = icmp ule i64 {}, %upper.idx", idx_cmp, check_with_vec));
            } else {
                ctx.code.add(format!("{} = icmp ult i64 {}, %upper.idx", idx_cmp, idx_tmp));
            }
            ctx.code.add(format!("br i1 {}, label %loop.body, label %loop.end", idx_cmp));
            ctx.code.add("loop.body:");
            let mut prev_ref = String::from("undef");
            let elem_ty_str = self.llvm_type(&elem_ty)?.to_string();
            for (i, iter) in par_for.data.iter().enumerate() {
                let data_ty_str = self.llvm_type(func.params.get(&iter.data).unwrap())?.to_string();
                let data_str = self.load_var(llvm_symbol(&iter.data).as_str(), &data_ty_str, ctx)?;
                let data_prefix = format!("@{}", data_ty_str.replace("%", ""));
                let inner_elem_tmp_ptr = ctx.var_ids.next();
                let inner_elem_ty_str = if par_for.data.len() == 1 {
                    elem_ty_str.clone()
                } else {
                    match *elem_ty {
                        Struct(ref v) => self.llvm_type(&v[i])?.to_string(),
                        _ => weld_err!("Internal error: invalid element type {}", print_type(elem_ty))?,
                    }
                };

                let arr_idx = if iter.start.is_some() {
                    // TODO(shoumik) implement. This needs to be a gather instead of a
                    // sequential load.
                    if iter.kind == IterKind::SimdIter {
                        return weld_err!("Unimplemented: vectorized iterators do not support non-unit stride.");
                    }
                    let offset = ctx.var_ids.next();
                    let stride_str = self.load_var(llvm_symbol(&iter.stride.clone().unwrap()).as_str(), "i64", ctx)?;
                    let start_str = self.load_var(llvm_symbol(&iter.start.clone().unwrap()).as_str(), "i64", ctx)?;
                    ctx.code.add(format!("{} = mul i64 {}, {}", offset, idx_tmp, stride_str));
                    let final_idx = ctx.var_ids.next();
                    ctx.code.add(format!("{} = add i64 {}, {}", final_idx, start_str, offset));
                    final_idx
                } else {
                    if iter.kind == IterKind::FringeIter {
                        let vector_len = format!("{}", vec_size(&elem_ty)?);
                        let tmp = ctx.var_ids.next();
                        let arr_len = ctx.var_ids.next();
                        let offset = ctx.var_ids.next();
                        let final_idx = ctx.var_ids.next();

                        ctx.code.add(format!("{} = call i64 {}.size({} {})",
                                                arr_len,
                                                data_prefix,
                                                &data_ty_str,
                                                data_str));

                        ctx.code.add(format!("{} = udiv i64 {}, {}", tmp, arr_len, vector_len));

                        // tmp2 is also where the iteration for the FringeIter starts (the
                        // offset).
                        ctx.code.add(format!("{} = mul i64 {}, {}", offset, tmp, vector_len));

                        // Compute the number of iterations.
                        ctx.code.add(format!("{} = add i64 {}, {}", final_idx, offset, idx_tmp));

                        final_idx
                    } else {
                        idx_tmp.clone()
                    }
                };

                match iter.kind {
                    IterKind::ScalarIter | IterKind::FringeIter => {
                        ctx.code.add(format!("{} = call {}* {}.at({} {}, i64 {})",
                                                inner_elem_tmp_ptr,
                                                &inner_elem_ty_str,
                                                data_prefix,
                                                &data_ty_str,
                                                data_str,
                                                arr_idx));
                    }
                    IterKind::SimdIter => {
                        ctx.code.add(format!("{} = call {}* {}.vat({} {}, i64 {})",
                                                inner_elem_tmp_ptr,
                                                &inner_elem_ty_str,
                                                data_prefix,
                                                &data_ty_str,
                                                data_str,
                                                arr_idx));
                    }
                };
                let inner_elem_tmp = try!(self.load_var(&inner_elem_tmp_ptr, &inner_elem_ty_str, ctx));
                if par_for.data.len() == 1 {
                    prev_ref.clear();
                    prev_ref.push_str(&inner_elem_tmp);
                } else {
                    let elem_tmp = ctx.var_ids.next();
                    ctx.code.add(format!("{} = insertvalue {} {}, {} {}, {}",
                                            elem_tmp,
                                            elem_ty_str,
                                            prev_ref,
                                            inner_elem_ty_str,
                                            inner_elem_tmp,
                                            i));
                    prev_ref.clear();
                    prev_ref.push_str(&elem_tmp);
                }
            }
            let elem_str = llvm_symbol(&par_for.data_arg);
            ctx.code.add(format!("store {} {}, {}* {}", &elem_ty_str, prev_ref, &elem_ty_str, elem_str));
            ctx.code.add(format!("store i64 {}, i64* {}", idx_tmp, llvm_symbol(&par_for.idx_arg)));
        }
        
        // Jump to block 0.
        ctx.code.add(format!("br label %b.b{}", func.blocks[0].id));

        // Generate an expression for the function body.
        self.gen_function(sir, func, ctx)?;
        ctx.code.add("body.end:");
        if containing_loop.is_some() {
            // TODO - should take the minimum vector size of all elements here?
            let vectorized = containing_loop.as_ref().unwrap().data[0].kind == IterKind::SimdIter;
            let fetch_width = if vectorized {
                vec_size(func.locals.get(&containing_loop.as_ref().unwrap().data_arg).unwrap())?
            } else {
                1
            };

            ctx.code.add("br label %loop.terminator");
            ctx.code.add("loop.terminator:");
            let idx_tmp = self.load_var("%cur.idx", "i64", ctx)?;
            let idx_inc = ctx.var_ids.next();
            ctx.code.add(format!("{} = add i64 {}, {}", idx_inc, idx_tmp, format!("{}", fetch_width)));
            ctx.code.add(format!("store i64 {}, i64* %cur.idx", idx_inc));
            ctx.code.add("br label %loop.start");
            ctx.code.add("loop.end:");
        }
        ctx.code.add("ret void");
        ctx.code.add("}\n\n");

        self.body_code.add(&ctx.alloca_code.result());
        self.body_code.add(&ctx.code.result());

        // if we'er in a loop, generaet wrapper function.
        if containing_loop.is_some() {
            let par_for = containing_loop.clone().unwrap();
            let mut wrap_ctx = &mut FunctionContext::new();
            let serial_arg_types = try!(self.get_arg_str(&get_combined_params(sir, &par_for), ""));
            wrap_ctx.code.add(format!("define void @f{}_wrapper({}) {{", func.id, serial_arg_types));
            wrap_ctx.code.add(format!("fn.entry:"));

            // Use the first data to compute the indexing.
            let first_data = &par_for.data[0].data;
            let data_str = llvm_symbol(&first_data);
            let data_ty_str = try!(self.llvm_type(func.params.get(&first_data).unwrap())).to_string();
            let data_prefix = format!("@{}", data_ty_str.replace("%", ""));

            let num_iters_str = wrap_ctx.var_ids.next();
            let mut fringe_start_str = None;

            if par_for.data[0].kind == IterKind::SimdIter || par_for.data[0].kind == IterKind::ScalarIter {
                if par_for.data[0].start.is_none() {
                    // set num_iters_str to len(first_data)
                    wrap_ctx.code.add(format!("{} = call i64 {}.size({} {})",
                                                num_iters_str,
                                                data_prefix,
                                                data_ty_str,
                                                data_str));
                } else {
                    // TODO(shoumik): Don't support non-unit stride right now.
                    if par_for.data[0].kind == IterKind::SimdIter {
                        return weld_err!("vector iterator does not support non-unit stride");
                    }
                    // set num_iters_str to (end - start) / stride
                    let start_str = llvm_symbol(&par_for.data[0].start.clone().unwrap());
                    let end_str = llvm_symbol(&par_for.data[0].end.clone().unwrap());
                    let stride_str = llvm_symbol(&par_for.data[0].stride.clone().unwrap());
                    let diff_tmp = wrap_ctx.var_ids.next();
                    wrap_ctx.code.add(format!("{} = sub i64 {}, {}", diff_tmp, end_str, start_str));
                    wrap_ctx.code.add(format!("{} = udiv i64 {}, {}", num_iters_str, diff_tmp, stride_str));
                }
            } else {
                // FringeIter
                // TODO(shoumik): Don't support non-unit stride right now.
                if par_for.data[0].start.is_some() {
                    return weld_err!("fringe iterator does not support non-unit stride");
                }
                let arr_len = wrap_ctx.var_ids.next();
                let tmp = wrap_ctx.var_ids.next();
                let tmp2 = wrap_ctx.var_ids.next();
                let vector_len = format!("{}", vec_size(get_sym_ty(func, &first_data)?)?);

                wrap_ctx
                    .code
                    .add(format!("{} = call i64 {}.size({} {})", arr_len, data_prefix, data_ty_str, data_str));

                // Compute the number of iterations:
                // tmp = arr_len / vec_size
                // tmp2 = tmp * vec_size
                // num_iters = arr_len - vec_size
                wrap_ctx.code.add(format!("{} = udiv i64 {}, {}", tmp, arr_len, vector_len));
                // tmp2 is also where the iteration for the FringeIter starts.
                wrap_ctx.code.add(format!("{} = mul i64 {}, {}", tmp2, tmp, vector_len));
                // Compute the number of iterations.
                wrap_ctx.code.add(format!("{} = sub i64 {}, {}", num_iters_str, arr_len, tmp2));

                fringe_start_str = Some(tmp2);
            }

            // Perform a bounds check on each of the data items before launching the loop
            for iter in par_for.data.iter() {
                // Vector LLVM information for the current iter.
                let data_str = llvm_symbol(&iter.data);
                let data_ty_str = try!(self.llvm_type(func.params.get(&iter.data).unwrap())).to_string();
                let data_prefix = format!("@{}", data_ty_str.replace("%", ""));
                let vec_size_str = wrap_ctx.var_ids.next();
                wrap_ctx
                    .code
                    .add(format!("{} = call i64 {}.size({} {})", vec_size_str, data_prefix, data_ty_str, data_str));

                let (start_str, stride_str) = if iter.start.is_none() {
                    // We already checked to make sure the FringeIter doesn't have a start,
                    // etc.
                    let start_str = if iter.kind == IterKind::FringeIter {
                        fringe_start_str.as_ref().unwrap().to_string()
                    } else {
                        "0".to_string()
                    };
                    let stride_str = "1".to_string();
                    (start_str, stride_str)
                } else {
                    (llvm_symbol(iter.start.as_ref().unwrap()), llvm_symbol(iter.stride.as_ref().unwrap()))
                };

                let t0 = wrap_ctx.var_ids.next();
                let t1 = wrap_ctx.var_ids.next();
                let t2 = wrap_ctx.var_ids.next();
                let cond = wrap_ctx.var_ids.next();
                let next_bounds_check_label = wrap_ctx.var_ids.next();

                // TODO just compare against end here...this computation is redundant.
                // t0 = sub i64 num_iters, 1
                // t1 = mul i64 stride, t0
                // t2 = add i64 t1, start
                // cond = icmp lte i64 t1, size
                // br i1 cond, label %nextCheck, label %checkFailed
                // nextCheck:
                // (loop)
                wrap_ctx.code.add(format!("{} = sub i64 {}, 1", t0, num_iters_str));
                wrap_ctx.code.add(format!("{} = mul i64 {}, {}", t1, stride_str, t0));
                wrap_ctx.code.add(format!("{} = add i64 {}, {}", t2, t1, start_str));
                wrap_ctx.code.add(format!("{} = icmp ult i64 {}, {}", cond, t2, vec_size_str));
                wrap_ctx
                    .code
                    .add(format!("br i1 {}, label {}, label %fn.boundcheckfailed", cond, next_bounds_check_label));
                wrap_ctx.code.add(format!("{}:", next_bounds_check_label.replace("%", "")));
            }
            // If we get here, the bounds check passed.
            wrap_ctx.code.add(format!("br label %fn.boundcheckpassed"));
            // Handle a bounds check fail.
            wrap_ctx.code.add(format!("fn.boundcheckfailed:"));
            let errno = WeldRuntimeErrno::BadIteratorLength;
            let run_id = wrap_ctx.var_ids.next();
            wrap_ctx.code.add(format!("{} = call i64 @get_runid()", run_id));
            wrap_ctx.code.add(format!("call void @weld_rt_set_errno(i64 {}, i64 {})", run_id, errno as i64));
            wrap_ctx.code.add(format!("call void @weld_abort_thread()"));
            wrap_ctx.code.add(format!("; Unreachable!"));
            wrap_ctx.code.add(format!("br label %fn.end"));
            wrap_ctx.code.add(format!("fn.boundcheckpassed:"));

            let bound_cmp = wrap_ctx.var_ids.next();
            let mut grain_size = 4096;
            if par_for.innermost {
                wrap_ctx.code.add(format!("{} = icmp ule i64 {}, {}", bound_cmp, num_iters_str, grain_size));
                wrap_ctx.code.add(format!("br i1 {}, label %for.ser, label %for.par", bound_cmp));
                wrap_ctx.code.add(format!("for.ser:"));
                let mut body_arg_types = try!(self.get_arg_str(&func.params, ""));
                body_arg_types.push_str(format!(", i64 0, i64 {}", num_iters_str).as_str());
                wrap_ctx.code.add(format!("call void @f{}({})", func.id, body_arg_types));
                let cont_arg_types = try!(self.get_arg_str(&sir.funcs[par_for.cont].params, ""));
                wrap_ctx.code.add(format!("call void @f{}({})", par_for.cont, cont_arg_types));
                wrap_ctx.code.add(format!("br label %fn.end"));
            } else {
                wrap_ctx.code.add("br label %for.par");
                grain_size = 1;
            }
            wrap_ctx.code.add(format!("for.par:"));
            let body_struct = try!(self.get_arg_struct(&func.params, &mut wrap_ctx));
            let cont_struct = try!(self.get_arg_struct(&sir.funcs[par_for.cont].params, &mut wrap_ctx));
            wrap_ctx.code.add(format!(
                "call void @pl_start_loop(%work_t* %cur.work, i8* {}, i8* {}, \
                                void (%work_t*)* @f{}_par, void (%work_t*)* @f{}_par, i64 0, \
                                i64 {}, i32 {})",
                body_struct,
                cont_struct,
                func.id,
                par_for.cont,
                num_iters_str,
                grain_size
            ));
            wrap_ctx.code.add(format!("br label %fn.end"));
            wrap_ctx.code.add("fn.end:");
            wrap_ctx.code.add("ret void");
            wrap_ctx.code.add("}\n\n");
            self.body_code.add(&wrap_ctx.code.result());

            let mut par_body_ctx = &mut FunctionContext::new();
            par_body_ctx.code.add(format!("define void @f{}_par(%work_t* %cur.work) {{", func.id));
            par_body_ctx.code.add("entry:");
            try!(self.unload_arg_struct(&func.params, &mut par_body_ctx));
            let lower_bound_ptr = par_body_ctx.var_ids.next();
            let lower_bound = par_body_ctx.var_ids.next();
            let upper_bound_ptr = par_body_ctx.var_ids.next();
            let upper_bound = par_body_ctx.var_ids.next();
            par_body_ctx
                .code
                .add(format!("{} = getelementptr %work_t, %work_t* %cur.work, i32 0, i32 1", lower_bound_ptr));
            par_body_ctx.code.add(format!("{} = load i64, i64* {}", lower_bound, lower_bound_ptr));
            par_body_ctx
                .code
                .add(format!("{} = getelementptr %work_t, %work_t* %cur.work, i32 0, i32 2", upper_bound_ptr));
            par_body_ctx.code.add(format!("{} = load i64, i64* {}", upper_bound, upper_bound_ptr));
            let body_arg_types = try!(self.get_arg_str(&func.params, ""));
            try!(self.create_new_pieces(&func.params, &mut par_body_ctx));
            par_body_ctx.code.add("fn_call:");
            par_body_ctx.code.add(format!("call void @f{}({}, i64 {}, i64 {})",
                                            func.id,
                                            body_arg_types,
                                            lower_bound,
                                            upper_bound));
            par_body_ctx.code.add("ret void");
            par_body_ctx.code.add("}\n\n");
            self.body_code.add(&par_body_ctx.code.result());

            let mut par_cont_ctx = &mut FunctionContext::new();
            par_cont_ctx.code.add(format!("define void @f{}_par(%work_t* %cur.work) {{", par_for.cont));
            par_cont_ctx.code.add("entry:");
            try!(self.unload_arg_struct(&sir.funcs[par_for.cont].params, &mut par_cont_ctx));
            try!(self.create_new_pieces(&sir.funcs[par_for.cont].params, &mut par_cont_ctx));
            par_cont_ctx.code.add("fn_call:");
            let cont_arg_types = try!(self.get_arg_str(&sir.funcs[par_for.cont].params, ""));
            par_cont_ctx.code.add(format!("call void @f{}({})", par_for.cont, cont_arg_types));
            par_cont_ctx.code.add("ret void");
            par_cont_ctx.code.add("}\n\n");
            self.body_code.add(&par_cont_ctx.code.result());
        }

        if func.id == 0 {
            let mut par_top_ctx = &mut FunctionContext::new();
            par_top_ctx.code.add("define void @f0_par(%work_t* %cur.work) {");
            try!(self.unload_arg_struct(&sir.funcs[0].params, &mut par_top_ctx));
            let top_arg_types = try!(self.get_arg_str(&sir.funcs[0].params, ""));
            par_top_ctx.code.add(format!("call void @f0({})", top_arg_types));
            par_top_ctx.code.add("ret void");
            par_top_ctx.code.add("}\n\n");
            self.body_code.add(&par_top_ctx.code.result());
        }

        Ok(())
    }

    /// Add a function to the generated program, passing its parameters and return value through
    /// pointers encoded as i64. This is used for the main entry point function into Weld modules
    /// to pass them arbitrary structures.
    pub fn add_function_on_pointers(&mut self, name: &str, sir: &SirProgram) -> WeldResult<()> {
        // First add the function on raw values, which we'll call from the pointer version.
        try!(self.add_function(sir, &sir.funcs[0], None));

        // Define a struct with all the argument types as fields
        let args_struct = Struct(sir.top_params.iter().map(|a| a.ty.clone()).collect());
        let args_type = try!(self.llvm_type(&args_struct)).to_string();

        let mut run_ctx = &mut FunctionContext::new();

        run_ctx.code.add(format!("define i64 @{}(i64 %r.input) {{", name));
        // Unpack the input, which is always struct defined by the type %input_arg_t in prelude.ll.
        run_ctx.code.add(format!("%r.inp_typed = inttoptr i64 %r.input to %input_arg_t*"));
        run_ctx.code.add(format!("%r.inp_val = load %input_arg_t, %input_arg_t* %r.inp_typed"));
        run_ctx.code.add(format!("%r.args = extractvalue %input_arg_t %r.inp_val, 0"));
        run_ctx.code.add(format!("%r.nworkers = extractvalue %input_arg_t %r.inp_val, 1"));
        run_ctx.code.add(format!("%r.memlimit = extractvalue %input_arg_t %r.inp_val, 2"));
        run_ctx.code.add(format!("call void @set_nworkers(i32 %r.nworkers)"));
        run_ctx.code.add(format!("call void @weld_rt_init(i64 %r.memlimit)"));
        // Code to load args and call function
        run_ctx.code.add(format!(
            "%r.args_typed = inttoptr i64 %r.args to {args_type}*
             %r.args_val = load {args_type}, {args_type}* %r.args_typed",
            args_type = args_type
        ));

        let mut arg_pos_map: HashMap<Symbol, usize> = HashMap::new();
        for (i, a) in sir.top_params.iter().enumerate() {
            arg_pos_map.insert(a.name.clone(), i);
        }
        for (arg, _) in sir.funcs[0].params.iter() {
            let idx = arg_pos_map.get(arg).unwrap();
            run_ctx.code.add(format!("{} = extractvalue {} %r.args_val, {}", llvm_symbol(arg), args_type, idx));
        }
        let run_struct = try!(self.get_arg_struct(&sir.funcs[0].params, &mut run_ctx));

        let rid = run_ctx.var_ids.next();
        let errno = run_ctx.var_ids.next();
        let tmp0 = run_ctx.var_ids.next();
        let tmp1 = run_ctx.var_ids.next();
        let tmp2 = run_ctx.var_ids.next();
        let size_ptr = run_ctx.var_ids.next();
        let size = run_ctx.var_ids.next();
        let bytes = run_ctx.var_ids.next();
        let typed_out_ptr = run_ctx.var_ids.next();
        let final_address = run_ctx.var_ids.next();

        run_ctx.code.add(format!(
            "call void @execute(void (%work_t*)* @f0_par, i8* {run_struct})
             %res_ptr = call i8* @get_result()
             %res_address = ptrtoint i8* %res_ptr to i64
             {rid} = call i64 @get_runid()
             {errno} = call i64 @weld_rt_get_errno(i64 {rid})
             {tmp0} = insertvalue %output_arg_t undef, i64 %res_address, 0
             {tmp1} = insertvalue %output_arg_t {tmp0}, i64 {rid}, 1
             {tmp2} = insertvalue %output_arg_t {tmp1}, i64 {errno}, 2
             {size_ptr} = getelementptr %output_arg_t, %output_arg_t* null, i32 1
             {size} = ptrtoint %output_arg_t* {size_ptr} to i64
             {bytes} = call i8* @malloc(i64 {size})
             {typed_out_ptr} = bitcast i8* {bytes} to %output_arg_t*
             store %output_arg_t {tmp2}, %output_arg_t* {typed_out_ptr}
             {final_address} = ptrtoint %output_arg_t* {typed_out_ptr} to i64
             ret i64 {final_address}",
            run_struct = run_struct,
            rid = rid,
            errno = errno,
            tmp0 = tmp0,
            tmp1 = tmp1,
            tmp2 = tmp2,
            size_ptr = size_ptr,
            size = size,
            bytes = bytes,
            typed_out_ptr = typed_out_ptr,
            final_address = final_address
        ));
        run_ctx.code.add("}\n\n");

        self.body_code.add(&run_ctx.code.result());
        Ok(())
    }

    /// Return the LLVM type name corresponding to a Weld type.
    fn llvm_type(&mut self, ty: &Type) -> WeldResult<&str> {
        match *ty {
            Scalar(Bool) => Ok("i1"),
            Scalar(I8) => Ok("i8"),
            Scalar(I32) => Ok("i32"),
            Scalar(I64) => Ok("i64"),
            Scalar(F32) => Ok("float"),
            Scalar(F64) => Ok("double"),

            Simd(Bool) => Ok(self.simd_names.entry(Bool).or_insert(format!("<{} x i1>", vec_size(&Scalar(Bool))?))),
            Simd(I8) => Ok(self.simd_names.entry(I8).or_insert(format!("<{} x i8>", vec_size(&Scalar(I8))?))),
            Simd(I32) => Ok(self.simd_names.entry(I32).or_insert(format!("<{} x i32>", vec_size(&Scalar(I32))?))),
            Simd(I64) => Ok(self.simd_names.entry(I64).or_insert(format!("<{} x i64>", vec_size(&Scalar(I64))?))),
            Simd(F32) => Ok(self.simd_names.entry(F32).or_insert(format!("<{} x float>", vec_size(&Scalar(F32))?))),
            Simd(F64) => Ok(self.simd_names.entry(F64).or_insert(format!("<{} x double>", vec_size(&Scalar(F64))?))),

            Struct(ref fields) => {
                if self.struct_names.get(fields) == None {
                    // Declare the struct in prelude_code
                    let name = self.struct_ids.next();
                    let mut field_types: Vec<String> = Vec::new();
                    for f in fields {
                        field_types.push(try!(self.llvm_type(f)).to_string());
                    }
                    let field_types_str = field_types.join(", ");
                    self.prelude_code.add(format!("{} = type {{ {} }}", name, field_types_str));

                    // Generate hash function for the struct.
                    self.prelude_code
                        .add_line(format!("define i64 {}.hash({} %value) {{", name.replace("%", "@"), name));
                    let mut res = "0".to_string();
                    for i in 0..field_types.len() {
                        // TODO(shoumik): hack to prevent incorrect code gen for vectors.
                        if let Simd(_) = fields[i] {
                            continue;
                        }
                        let field = self.prelude_var_ids.next();
                        let hash = self.prelude_var_ids.next();
                        let new_res = self.prelude_var_ids.next();
                        let field_ty_str = &field_types[i];
                        let field_prefix_str = format!("@{}", field_ty_str.replace("%", ""));
                        self.prelude_code.add_line(format!("{} = extractvalue {} %value, {}", field, name, i));
                        self.prelude_code.add_line(format!("{} = call i64 {}.hash({} {})",
                                                           hash,
                                                           field_prefix_str,
                                                           field_ty_str,
                                                           field));
                        self.prelude_code
                            .add_line(format!("{} = call i64 @hash_combine(i64 {}, i64 {})", new_res, res, hash));
                        res = new_res;
                    }
                    self.prelude_code.add_line(format!("ret i64 {}", res));
                    self.prelude_code.add_line(format!("}}"));
                    self.prelude_code.add_line(format!(""));

                    self.prelude_code
                        .add_line(format!("define i32 {}.cmp({} %a, {} %b) {{", name.replace("%", "@"), name, name));
                    let mut label_ids = IdGenerator::new("%l");
                    for i in 0..field_types.len() {
                        // TODO(shoumik): hack to prevent incorrect code gen for vectors.
                        if let Simd(_) = fields[i] {
                            continue;
                        }
                        let a_field = self.prelude_var_ids.next();
                        let b_field = self.prelude_var_ids.next();
                        let cmp = self.prelude_var_ids.next();
                        let ne = self.prelude_var_ids.next();
                        let field_ty_str = &field_types[i];
                        let ret_label = label_ids.next();
                        let post_label = label_ids.next();
                        let field_prefix_str = format!("@{}", field_ty_str.replace("%", ""));
                        self.prelude_code.add_line(format!("{} = extractvalue {} %a , {}", a_field, name, i));
                        self.prelude_code.add_line(format!("{} = extractvalue {} %b, {}", b_field, name, i));
                        self.prelude_code.add_line(format!("{} = call i32 {}.cmp({} {}, {} {})",
                                                           cmp,
                                                           field_prefix_str,
                                                           field_ty_str,
                                                           a_field,
                                                           field_ty_str,
                                                           b_field));
                        self.prelude_code.add_line(format!("{} = icmp ne i32 {}, 0", ne, cmp));
                        self.prelude_code.add_line(format!("br i1 {}, label {}, label {}", ne, ret_label, post_label));
                        self.prelude_code.add_line(format!("{}:", ret_label.replace("%", "")));
                        self.prelude_code.add_line(format!("ret i32 {}", cmp));
                        self.prelude_code.add_line(format!("{}:", post_label.replace("%", "")));
                    }
                    self.prelude_code.add_line(format!("ret i32 0"));
                    self.prelude_code.add_line(format!("}}"));
                    self.prelude_code.add_line(format!(""));

                    // Add it into our map so we remember its name
                    self.struct_names.insert(fields.clone(), name);
                }
                Ok(self.struct_names.get(fields).unwrap())
            }

            Vector(ref elem) => {
                if self.vec_names.get(elem) == None {
                    let elem_ty = try!(self.llvm_type(elem)).to_string();
                    let elem_prefix = format!("@{}", elem_ty.replace("%", ""));
                    let name = self.vec_ids.next();
                    self.vec_names.insert(*elem.clone(), name.clone());
                    let prefix_replaced = VECTOR_CODE.replace("$ELEM_PREFIX", &elem_prefix);
                    let elem_replaced = prefix_replaced.replace("$ELEM", &elem_ty);
                    let name_replaced = elem_replaced.replace("$NAME", &name.replace("%", ""));
                    self.prelude_code.add(&name_replaced);
                    self.prelude_code.add("\n");

                    // Supports vectorization, so splice in the vector extensions.
                    if let Scalar(_) = *elem.as_ref() {
                        let replaced = VVECTOR_CODE.replace("$ELEM_PREFIX", &elem_prefix);
                        let replaced = replaced.replace("$ELEM", &elem_ty);
                        let replaced = replaced.replace("$VECSIZE", &format!("{}", vec_size(elem)?));
                        let replaced = replaced.replace("$NAME", &name.replace("%", ""));
                        self.prelude_code.add(&replaced);
                        self.prelude_code.add("\n");
                    }
                }
                Ok(self.vec_names.get(elem).unwrap())
            }

            Dict(ref key, ref value) => {
                let elem = Box::new(Struct(vec![*key.clone(), *value.clone()]));
                if self.dict_names.get(&elem) == None {
                    let key_ty = try!(self.llvm_type(key)).to_string();
                    let value_ty = try!(self.llvm_type(value)).to_string();
                    let key_prefix = format!("@{}", key_ty.replace("%", ""));
                    let name = self.dict_ids.next();
                    self.dict_names.insert(*elem.clone(), name.clone());
                    let kv_struct_ty = try!(self.llvm_type(&elem)).to_string();
                    let kv_vec = Box::new(Vector(elem.clone()));
                    let kv_vec_ty = try!(self.llvm_type(&kv_vec)).to_string();
                    let kv_vec_prefix = format!("@{}", &kv_vec_ty.replace("%", ""));
                    let key_prefix_replaced = DICTIONARY_CODE.replace("$KEY_PREFIX", &key_prefix);
                    let name_replaced = key_prefix_replaced.replace("$NAME", &name.replace("%", ""));
                    let key_ty_replaced = name_replaced.replace("$KEY", &key_ty);
                    let value_ty_replaced = key_ty_replaced.replace("$VALUE", &value_ty);
                    let kv_struct_replaced = value_ty_replaced.replace("$KV_STRUCT", &kv_struct_ty);
                    let kv_vec_prefix_replaced = kv_struct_replaced.replace("$KV_VEC_PREFIX", &kv_vec_prefix);
                    let kv_vec_ty_replaced = kv_vec_prefix_replaced.replace("$KV_VEC", &kv_vec_ty);
                    self.prelude_code.add(&kv_vec_ty_replaced);
                    self.prelude_code.add("\n");
                }
                Ok(self.dict_names.get(&elem).unwrap())
            }

            Builder(ref bk, _) => {
                // TODO(Deepak): Do something with annotations here...
                if self.bld_names.get(bk) == None {
                    match *bk {
                        Appender(ref t) => {
                            let bld_ty = Vector(t.clone());
                            let bld_ty_str = try!(self.llvm_type(&bld_ty)).to_string();
                            self.bld_names.insert(bk.clone(), format!("{}.bld", bld_ty_str));
                        }
                        Merger(ref t, _) => {
                            if self.merger_names.get(t) == None {
                                let elem_ty = self.llvm_type(t)?.to_string();
                                let elem_prefix = format!("@{}", elem_ty.replace("%", ""));
                                let name = self.merger_ids.next();
                                self.merger_names.insert(*t.clone(), name.clone());
                                let prefix_replaced = MERGER_CODE.replace("$ELEM_PREFIX", &elem_prefix);
                                let elem_replaced = prefix_replaced.replace("$ELEM", &elem_ty);
                                // TODO!
                                let vecsize_replaced = elem_replaced.replace("$VECSIZE", &format!("{}", vec_size(t)?));
                                let name_replaced = vecsize_replaced.replace("$NAME", &name.replace("%", ""));
                                self.prelude_code.add(&name_replaced);
                                self.prelude_code.add("\n");
                            }
                            let bld_ty_str = self.merger_names.get(t).unwrap();
                            self.bld_names.insert(bk.clone(), format!("{}.bld", bld_ty_str));
                        }
                        DictMerger(ref kt, ref vt, ref op) => {
                            let bld_ty = Dict(kt.clone(), vt.clone());
                            let bld_ty_str = try!(self.llvm_type(&bld_ty)).to_string();
                            let elem = Box::new(Struct(vec![*kt.clone(), *vt.clone()]));
                            let kv_struct_ty = try!(self.llvm_type(&elem)).to_string();
                            let key_ty = try!(self.llvm_type(kt)).to_string();
                            let value_ty = try!(self.llvm_type(vt)).to_string();
                            let kv_vec = Box::new(Vector(elem.clone()));
                            let kv_vec_ty = try!(self.llvm_type(&kv_vec)).to_string();
                            let kv_vec_prefix = format!("@{}", &kv_vec_ty.replace("%", ""));
                            let name_replaced = DICTMERGER_CODE.replace("$NAME", &bld_ty_str.replace("%", ""));
                            let key_ty_replaced = name_replaced.replace("$KEY", &key_ty);
                            let value_ty_replaced = key_ty_replaced.replace("$VALUE", &value_ty);
                            let kv_struct_replaced = value_ty_replaced
                                .replace("$KV_STRUCT", &kv_struct_ty.replace("%", ""));
                            let op_replaced = kv_struct_replaced.replace("$OP", &llvm_binop(*op, vt)?);
                            let kv_vec_prefix_replaced = op_replaced.replace("$KV_VEC_PREFIX", &kv_vec_prefix);
                            let kv_vec_ty_replaced = kv_vec_prefix_replaced.replace("$KV_VEC", &kv_vec_ty);
                            self.prelude_code.add(&kv_vec_ty_replaced);
                            self.prelude_code.add("\n");
                            self.bld_names.insert(bk.clone(), format!("{}.bld", bld_ty_str));
                        }
                        GroupMerger(ref kt, ref vt) => {
                            let elem = Box::new(Struct(vec![*kt.clone(), *vt.clone()]));
                            let bld_ty = Vector(elem.clone());
                            let bld_ty_str = try!(self.llvm_type(&bld_ty)).to_string();
                            self.bld_names.insert(bk.clone(), format!("{}.bld", bld_ty_str));
                        }
                        VecMerger(ref elem, _) => {
                            let bld_ty = Vector(elem.clone());
                            let bld_ty_str = try!(self.llvm_type(&bld_ty)).to_string();
                            self.bld_names.insert(bk.clone(), format!("{}.vm.bld", bld_ty_str));
                        }
                    }
                }
                Ok(self.bld_names.get(bk).unwrap())
            }

            _ => weld_err!("Unsupported type {}", print_type(ty))?,
        }
    }

    /// Generate code to load a symbol sym with LLVM type ty into a local variable, and return the variable's name.
    fn load_var(&mut self, sym: &str, ty: &str, ctx: &mut FunctionContext) -> WeldResult<String> {
        let var = ctx.var_ids.next();
        // Hacky...but need an aligned load for vectors to prevent strange segfaults.
        let is_vector = ty.contains("<") && ty.contains(">") && ty.contains("x");
        if is_vector {
            ctx.code.add(format!("{} = load {}, {}* {}, align 1", var, ty, ty, sym));
        } else {
            ctx.code.add(format!("{} = load {}, {}* {}", var, ty, ty, sym));
        }
        Ok(var)
    }

    fn generate_vector_literal(&mut self,
                               output: &str,
                               value: &LiteralKind,
                               vec_ty: &Type,
                               ctx: &mut FunctionContext)
                               -> WeldResult<()> {
        let size = vec_size(vec_ty)?;
        let vec_ty_str = self.llvm_type(vec_ty)?.to_string();
        let size_str = format!("{}", size);
        let insert_str = match *value {
            BoolLiteral(l) => {
                format!("insertelement <{} x i1> $NAME, i1 {}, i32 $INDEX", size_str, if l { 1 } else { 0 })
            }
            I8Literal(l) => format!("insertelement <{} x i8> $NAME, i8 {}, i32 $INDEX", size_str, l),
            I32Literal(l) => format!("insertelement <{} x i32> $NAME, i32 {}, i32 $INDEX", size_str, l),
            I64Literal(l) => format!("insertelement <{} x i64> $NAME, i64 {}, i32 $INDEX", size_str, l),
            F32Literal(l) => format!("insertelement <{} x float> $NAME, float {:.30e}, i32 $INDEX", size_str, l),
            F64Literal(l) => format!("insertelement <{} x double> $NAME, double {:.30e}, i32 $INDEX", size_str, l),
        };

        let mut prev_name = "undef".to_string();
        for i in 0..size {
            let replaced = insert_str.replace("$NAME", &prev_name);
            let replaced = replaced.replace("$INDEX", &format!("{}", i));
            let name = ctx.var_ids.next().to_string();
            ctx.code.add(format!("{} = {}", name, replaced));
            prev_name = name;
        }

        ctx.code.add(format!("store {vec_ty_str} {prev_name}, {vec_ty_str}* {output}",
                             vec_ty_str = vec_ty_str,
                             output = output,
                             prev_name = prev_name));

        Ok(())
    }

    /// Given a pointer to a some data retrieved from a builder, generates code to merge a value
    /// into the builder using a binary operation. The result will be stored back into the
    /// pointer to complete the merge. `builder_ptr` is the pointer into which the original value
    /// is read and the new value will be stored. `merge_value` is the value to merge in.
    fn gen_merge_op(&mut self,
                    builder_ptr: &str,
                    merge_value: &str,
                    merge_ty_str: &str,
                    bin_op: &BinOpKind,
                    merge_ty: &Type,
                    ctx: &mut FunctionContext)
                    -> WeldResult<()> {
        let builder_value = ctx.var_ids.next();
        let mut res = ctx.var_ids.next();
        ctx.code.add(format!("{} = load {}, {}* {}", &builder_value, &merge_ty_str, &merge_ty_str, &builder_ptr));
        if let Scalar(_) = *merge_ty {
            ctx.code.add(format!("{} = {} {} {}, {}",
                                 &res,
                                 try!(llvm_binop(*bin_op, merge_ty)),
                                 &merge_ty_str,
                                 builder_value,
                                 merge_value));
        } else if let Struct(ref tys) = *merge_ty {
            let mut cur = "undef".to_string();
            for (i, ty) in tys.iter().enumerate() {
                let merge_elem = ctx.var_ids.next();
                let builder_elem = ctx.var_ids.next();
                let struct_name = ctx.var_ids.next();
                let binop_value = ctx.var_ids.next();
                let elem_ty_str = try!(self.llvm_type(ty)).to_string();
                ctx.code.add(format!("{} = extractvalue {} {}, {}", &merge_elem, &merge_ty_str, &merge_value, i));
                ctx.code.add(format!("{} = extractvalue {} {}, {}", &builder_elem, &merge_ty_str, &builder_value, i));
                ctx.code.add(format!("{} = {} {} {}, {}",
                                     &binop_value,
                                     try!(llvm_binop(*bin_op, ty)),
                                     &elem_ty_str,
                                     &merge_elem,
                                     &builder_elem));
                ctx.code.add(format!("{} = insertvalue {} {}, {} {}, {}",
                                     &struct_name,
                                     &merge_ty_str,
                                     &cur,
                                     &elem_ty_str,
                                     &binop_value,
                                     i));
                res = struct_name.clone();
                cur = struct_name.clone();
            }
        } else {
            unreachable!();
        }

        // Store the resulting merge value back into the builder pointer.
        ctx.code.add(format!("store {} {}, {}* {}", &merge_ty_str, &res, &merge_ty_str, &builder_ptr));
        Ok(())
    }

    /// Generate code to perform a unary operation on `child` and store the result in `output` (which should
    /// be a location on the stack).
    fn gen_unary_op(&mut self,
                    ctx: &mut FunctionContext,
                    func: &SirFunction,
                    output: &Symbol,
                    child: &Symbol,
                    op_kind: UnaryOpKind)
                    -> WeldResult<()> {
        let child_ty = try!(get_sym_ty(func, child));
        if let Scalar(ref ty) = *child_ty {
            let child_ll_ty = try!(self.llvm_type(&child_ty)).to_string();
            let child_tmp = try!(self.load_var(llvm_symbol(child).as_str(), &child_ll_ty, ctx));
            let res_tmp = ctx.var_ids.next();
            let op_name = try!(llvm_unaryop(op_kind, ty));
            ctx.code.add(format!("{} = call {} {} ({} {})", res_tmp, child_ll_ty, op_name, child_ll_ty, child_tmp));
            let out_ty = try!(get_sym_ty(func, output));
            let out_ty_str = try!(self.llvm_type(&out_ty)).to_string();
            ctx.code.add(format!("store {} {}, {}* {}", out_ty_str, res_tmp, out_ty_str, llvm_symbol(output)));
        } else {
            weld_err!("Illegal type {} in {}", print_type(child_ty), op_kind)?;
        }
        Ok(())
    }

    /// Generate code for a function and append it to its FunctionContext.
    fn gen_function(&mut self, sir: &SirProgram, func: &SirFunction, ctx: &mut FunctionContext) -> WeldResult<()> {
        for b in func.blocks.iter() {
            ctx.code.add(format!("b.b{}:", b.id));
            for s in b.statements.iter() {
                self.gen_statement(s, func, ctx)?
            }
            self.gen_terminator(&b.terminator, sir, func, ctx)?
        }
        Ok(())
    }

    /// Generate code for a single statement, appending it to the code in a FunctionContext.
    fn gen_statement(&mut self, statement: &Statement, func: &SirFunction, ctx: &mut FunctionContext) -> WeldResult<()> {
        match *statement {
            MakeStruct { ref output, ref elems } => {
                let mut cur = "undef".to_string();
                let struct_type = Struct(elems.iter().map(|e| e.1.clone()).collect::<Vec<_>>());
                let ll_ty = self.llvm_type(&struct_type)?.to_string();
                for (i, &(ref elem, ref ty)) in elems.iter().enumerate() {
                    let ll_elem_ty = try!(self.llvm_type(&ty)).to_string();
                    let tmp = try!(self.load_var(llvm_symbol(&elem).as_str(), &ll_elem_ty, ctx));
                    let struct_name = ctx.var_ids.next();
                    ctx.code.add(format!("{} = insertvalue {} {}, {} {}, {}",
                                            &struct_name,
                                            &ll_ty,
                                            &cur,
                                            &ll_elem_ty,
                                            &tmp,
                                            i));
                    cur = struct_name.clone();
                }
                ctx.code.add(format!("store {} {}, {}* {}", ll_ty, cur, ll_ty, llvm_symbol(output)));
            }

            CUDF { ref output, ref symbol_name, ref args } => {
                // TODO If function not declared
                if true {
                    // First, declare the function.
                    let mut arg_tys = vec![];
                    for ref arg in args {
                        arg_tys.push(format!("{}*", self.llvm_type(get_sym_ty(func, arg)?)?.to_string()));
                    }
                    arg_tys.push(format!("{}*", self.llvm_type(get_sym_ty(func, output)?)?.to_string()));
                    let arg_sig = arg_tys.join(", ");

                    self.prelude_code.add(format!("declare void @{name}({arg_sig});",
                                                    name = symbol_name,
                                                    arg_sig = arg_sig));
                }

                // Prepare the parameter list for the function
                let mut arg_tys = vec![];
                for ref arg in args {
                    let ll_ty = self.llvm_type(get_sym_ty(func, arg)?)?.to_string();
                    let arg_str = format!("{ll_ty}* {arg}", arg = llvm_symbol(arg), ll_ty = ll_ty);
                    arg_tys.push(arg_str);
                }
                arg_tys.push(format!("{}* {}",
                                        self.llvm_type(get_sym_ty(func, output)?)?.to_string(),
                                        llvm_symbol(output)));
                let param_sig = arg_tys.join(", ");

                ctx.code.add(
                    format!("call void @{name}({param_sig})", name = symbol_name, param_sig = param_sig));
            }

            MakeVector { ref output, ref elems, ref elem_ty } => {
                let elem_ll_ty = self.llvm_type(elem_ty)?.to_string();
                let vec_ll_ty = self.llvm_type(&Vector(Box::new(elem_ty.clone())))?.to_string();
                let vec_ll_prefix = vec_ll_ty.replace("%", "@");
                let vec = ctx.var_ids.next();
                let capacity_str = format!("{}", elems.len());
                ctx.code.add(format!("{vec} = call {vec_type} {prefix}.new(i64 {capacity})",
                                        vec = vec,
                                        vec_type = vec_ll_ty,
                                        prefix = vec_ll_prefix,
                                        capacity = capacity_str));
                for (i, elem) in elems.iter().enumerate() {
                    let e = self.load_var(llvm_symbol(&elem).as_str(), &elem_ll_ty, ctx)?.to_string();
                    let ptr = ctx.var_ids.next();
                    let idx_str = format!("{}", i);
                    ctx.code.add(format!("{ptr} = call {elem_ty}* \
                                            {prefix}.at({vec_type} {vec}, i64 {idx})",
                                            ptr = ptr,
                                            elem_ty = elem_ll_ty,
                                            prefix = vec_ll_prefix,
                                            vec_type = vec_ll_ty,
                                            vec = vec,
                                            idx = idx_str));
                    ctx.code.add(format!("store {elem_ty} {elem}, {elem_ty}* {ptr}",
                                            elem_ty = elem_ll_ty,
                                            elem = e,
                                            ptr = ptr));
                }
                ctx.code.add(format!("store {vec_ty} {vec}, {vec_ty}* {output}",
                                        vec_ty = vec_ll_ty,
                                        vec = vec,
                                        output = llvm_symbol(&output).as_str()));
            }

            BinOp { ref output, op, ref ty, ref left, ref right } => {
                let ll_ty = self.llvm_type(ty)?.to_string();
                let left_tmp = self.load_var(llvm_symbol(left).as_str(), &ll_ty, ctx)?;
                let right_tmp = self.load_var(llvm_symbol(right).as_str(), &ll_ty, ctx)?;
                let bin_tmp = ctx.var_ids.next();
                let out_ty = get_sym_ty(func, output)?;
                let out_ty_str = self.llvm_type(&out_ty)?.to_string();
                match *ty {
                    Scalar(_) | Simd(_) => {
                        let op_name = llvm_binop(op, ty)?;
                        ctx.code
                            .add(format!("{} = {} {} {}, {}", bin_tmp, op_name, ll_ty, left_tmp, right_tmp));
                        ctx.code.add(format!("store {} {}, {}* {}",
                                                out_ty_str,
                                                bin_tmp,
                                                out_ty_str,
                                                llvm_symbol(output)));
                    }
                    Vector(_) => {
                        // We support BinOps between vectors as long as they're comparison operators
                        let (op_name, value) = llvm_binop_vector(op, ty)?;
                        let tmp = ctx.var_ids.next();
                        let vec_prefix = format!("@{}", ll_ty.replace("%", ""));
                        ctx.code.add(format!("{} = call i32 {}.cmp({} {}, {} {})",
                                                tmp,
                                                vec_prefix,
                                                ll_ty,
                                                left_tmp,
                                                ll_ty,
                                                right_tmp));
                        ctx.code.add(format!("{} = icmp {} i32 {}, {}", bin_tmp, op_name, tmp, value));
                        ctx.code.add(format!("store {} {}, {}* {}",
                                                out_ty_str,
                                                bin_tmp,
                                                out_ty_str,
                                                llvm_symbol(output)));
                    }
                    _ => weld_err!("Illegal type {} in BinOp", print_type(ty))?,
                }
            }

            Broadcast { ref output, ref child } => {
                let ty = get_sym_ty(func, output)?;
                let elem_ty = get_sym_ty(func, child)?;

                let elem_ty_str = self.llvm_type(&elem_ty)?.to_string();
                let vec_ty_str = self.llvm_type(&ty)?.to_string();

                let elem = self.load_var(llvm_symbol(child).as_str(), &elem_ty_str, ctx)?;
                let size = vec_size(&elem_ty)?;

                let mut prev_name = "undef".to_string();
                for i in 0..size {
                    let next = ctx.var_ids.next();
                    ctx.code.add(format!("{next} = insertelement {vec_ty_str} {prev_name}, {elem_ty_str} {elem}, i32 {i}",
                                            next=next,
                                            vec_ty_str=vec_ty_str,
                                            prev_name=prev_name,
                                            elem_ty_str=elem_ty_str,
                                            elem=elem,
                                            i=i));
                    prev_name = next;
                }
                ctx.code.add(format!("store {vec_ty_str} {prev_name}, {vec_ty_str}* {output}, align 1",
                                        vec_ty_str = vec_ty_str,
                                        output = llvm_symbol(output).as_str(),
                                        prev_name = prev_name));
            }

            UnaryOp { ref output, op, ref child, } => {
                self.gen_unary_op(ctx, func, output, child, op)?
            }

            Negate { ref output, ref child } => {
                let out_ty = get_sym_ty(func, output)?;
                let ll_ty = self.llvm_type(out_ty)?.to_string();
                let child_tmp = self.load_var(llvm_symbol(child).as_str(), &ll_ty, ctx)?;
                let bin_tmp = ctx.var_ids.next();
                let out_ty_str = self.llvm_type(&out_ty)?.to_string();
                let op_name = llvm_binop(BinOpKind::Subtract, out_ty)?;

                let zero_str = match *out_ty {
                    Scalar(F32) | Scalar(F64) => "0.0",
                    _ => "0",
                };

                ctx.code.add(format!("{} = {} {} {}, {}", bin_tmp, op_name, ll_ty, zero_str, child_tmp));
                ctx.code.add(format!("store {} {}, {}* {}", out_ty_str, bin_tmp, out_ty_str, llvm_symbol(output)));
            }

            Cast { ref output, ref new_ty, ref child } => {
                let old_ty = try!(get_sym_ty(func, child));
                let old_ll_ty = try!(self.llvm_type(&old_ty)).to_string();
                if old_ty != new_ty {
                    let op_name = try!(llvm_castop(&old_ty, &new_ty));
                    let new_ll_ty = try!(self.llvm_type(&new_ty)).to_string();
                    let child_tmp = try!(self.load_var(llvm_symbol(child).as_str(), &old_ll_ty, ctx));
                    let cast_tmp = ctx.var_ids.next();
                    ctx.code.add(format!("{} = {} {} {} to {}",
                                            cast_tmp,
                                            op_name,
                                            old_ll_ty,
                                            child_tmp,
                                            new_ll_ty));
                    let out_ty = try!(get_sym_ty(func, output));
                    let out_ty_str = try!(self.llvm_type(&out_ty)).to_string();
                    ctx.code.add(format!("store {} {}, {}* {}",
                                            out_ty_str,
                                            cast_tmp,
                                            out_ty_str,
                                            llvm_symbol(output)));
                } else {
                    let child_tmp = try!(self.load_var(llvm_symbol(child).as_str(), &old_ll_ty, ctx));
                    ctx.code.add(format!("store {} {}, {}* {}",
                                            old_ll_ty,
                                            child_tmp,
                                            old_ll_ty,
                                            llvm_symbol(output)));
                }
            }

            Lookup { ref output, ref child, ref index } => {
                let child_ty = try!(get_sym_ty(func, child));
                match *child_ty {
                    Vector(_) => {
                        let child_ll_ty = try!(self.llvm_type(&child_ty)).to_string();
                        let output_ty = try!(get_sym_ty(func, output));
                        let output_ll_ty = try!(self.llvm_type(&output_ty)).to_string();
                        let vec_ll_ty = try!(self.llvm_type(&child_ty)).to_string();
                        let vec_prefix = format!("@{}", vec_ll_ty.replace("%", ""));
                        let child_tmp = try!(self.load_var(llvm_symbol(child).as_str(), &child_ll_ty, ctx));
                        let index_tmp = try!(self.load_var(llvm_symbol(index).as_str(), "i64", ctx));
                        let res_ptr = ctx.var_ids.next();
                        let res_tmp = ctx.var_ids.next();
                        ctx.code.add(format!("{} = call {}* {}.at({} {}, i64 {})",
                                                res_ptr,
                                                output_ll_ty,
                                                vec_prefix,
                                                vec_ll_ty,
                                                child_tmp,
                                                index_tmp));
                        ctx.code
                            .add(format!("{} = load {}, {}* {}", res_tmp, output_ll_ty, output_ll_ty, res_ptr));
                        ctx.code.add(format!("store {} {}, {}* {}",
                                                output_ll_ty,
                                                res_tmp,
                                                output_ll_ty,
                                                llvm_symbol(output)));
                    }
                    Dict(_, _) => {
                        let child_ll_ty = try!(self.llvm_type(&child_ty)).to_string();
                        let output_ty = try!(get_sym_ty(func, output));
                        let output_ll_ty = try!(self.llvm_type(&output_ty)).to_string();
                        let dict_ll_ty = try!(self.llvm_type(&child_ty)).to_string();
                        let index_ty = try!(get_sym_ty(func, index));
                        let index_ll_ty = try!(self.llvm_type(&index_ty)).to_string();
                        let dict_prefix = format!("@{}", dict_ll_ty.replace("%", ""));
                        let child_tmp = try!(self.load_var(llvm_symbol(child).as_str(), &child_ll_ty, ctx));
                        let index_tmp = try!(self.load_var(llvm_symbol(index).as_str(), &index_ll_ty, ctx));
                        let slot = ctx.var_ids.next();
                        let res_tmp = ctx.var_ids.next();
                        ctx.code.add(format!("{} = call {}.slot {}.lookup({} {}, {} {})",
                                                slot,
                                                dict_ll_ty,
                                                dict_prefix,
                                                dict_ll_ty,
                                                child_tmp,
                                                index_ll_ty,
                                                index_tmp));
                        ctx.code.add(format!("{} = call {} {}.slot.value({}.slot {})",
                                                res_tmp,
                                                output_ll_ty,
                                                dict_prefix,
                                                dict_ll_ty,
                                                slot));
                        ctx.code.add(format!("store {} {}, {}* {}",
                                                output_ll_ty,
                                                res_tmp,
                                                output_ll_ty,
                                                llvm_symbol(output)));
                    }
                    _ => weld_err!("Illegal type {} in Lookup", print_type(child_ty))?,
                }
            }

            KeyExists { ref output, ref child, ref key } => {
                let child_ty = try!(get_sym_ty(func, child));
                match *child_ty {
                    Dict(_, _) => {
                        let child_ll_ty = try!(self.llvm_type(&child_ty)).to_string();
                        let dict_ll_ty = try!(self.llvm_type(&child_ty)).to_string();
                        let key_ty = try!(get_sym_ty(func, key));
                        let key_ll_ty = try!(self.llvm_type(&key_ty)).to_string();
                        let dict_prefix = format!("@{}", dict_ll_ty.replace("%", ""));
                        let child_tmp = try!(self.load_var(llvm_symbol(child).as_str(), &child_ll_ty, ctx));
                        let key_tmp = try!(self.load_var(llvm_symbol(key).as_str(), &key_ll_ty, ctx));
                        let slot = ctx.var_ids.next();
                        let res_tmp = ctx.var_ids.next();
                        ctx.code.add(format!("{} = call {}.slot {}.lookup({} {}, {} {})",
                                                slot,
                                                dict_ll_ty,
                                                dict_prefix,
                                                dict_ll_ty,
                                                child_tmp,
                                                key_ll_ty,
                                                key_tmp));
                        ctx.code.add(format!("{} = call i1 {}.slot.filled({}.slot {})",
                                                res_tmp,
                                                dict_prefix,
                                                dict_ll_ty,
                                                slot));
                        ctx.code.add(format!("store i1 {}, i1* {}", res_tmp, llvm_symbol(output)));
                    }
                    _ => weld_err!("Illegal type {} in KeyExists", print_type(child_ty))?,
                }
            }

            Slice { ref output, ref child, ref index, ref size } => {
                let child_ty = try!(get_sym_ty(func, child));
                match *child_ty {
                    Vector(_) => {
                        let child_ll_ty = try!(self.llvm_type(&child_ty)).to_string();
                        let output_ty = try!(get_sym_ty(func, output));
                        let output_ll_ty = try!(self.llvm_type(&output_ty)).to_string();
                        let vec_ll_ty = try!(self.llvm_type(&child_ty)).to_string();
                        let vec_prefix = format!("@{}", vec_ll_ty.replace("%", ""));
                        let child_tmp = try!(self.load_var(llvm_symbol(child).as_str(), &child_ll_ty, ctx));
                        let index_tmp = try!(self.load_var(llvm_symbol(index).as_str(), "i64", ctx));
                        let size_tmp = try!(self.load_var(llvm_symbol(size).as_str(), "i64", ctx));
                        let res_ptr = ctx.var_ids.next();
                        ctx.code.add(format!("{} = call {} {}.slice({} {}, i64 {}, \
                                                i64{})",
                                                res_ptr,
                                                output_ll_ty,
                                                vec_prefix,
                                                vec_ll_ty,
                                                child_tmp,
                                                index_tmp,
                                                size_tmp));
                        let out_ty = try!(get_sym_ty(func, output));
                        let out_ty_str = try!(self.llvm_type(&out_ty)).to_string();
                        ctx.code.add(format!("store {} {}, {}* {}",
                                                out_ty_str,
                                                res_ptr,
                                                out_ty_str,
                                                llvm_symbol(output)))
                    }
                    _ => weld_err!("Illegal type {} in Slice", print_type(child_ty))?,
                }
            }

            Select { ref output, ref cond, ref on_true, ref on_false } => {
                let cond_ty_str = self.llvm_type(get_sym_ty(func, cond)?)?.to_string();
                let res_ty_str = self.llvm_type(get_sym_ty(func, on_true)?)?.to_string();

                let output_str = llvm_symbol(output).to_string();
                let cond_str = self.load_var(llvm_symbol(cond).as_str(), &cond_ty_str, ctx)?;
                let true_str = self.load_var(llvm_symbol(on_true).as_str(), &res_ty_str, ctx)?;
                let false_str = self.load_var(llvm_symbol(on_false).as_str(), &res_ty_str, ctx)?;


                let tmp = ctx.var_ids.next();
                ctx.code.add(format!("{tmp} = select {cond_ty_str} {cond_str}, {res_ty_str} {true_str}, {res_ty_str} {false_str}",
                                        tmp=tmp,
                                        cond_ty_str=cond_ty_str,
                                        cond_str=cond_str,
                                        res_ty_str=res_ty_str,
                                        true_str=true_str,
                                        false_str=false_str));
                ctx.code.add(format!("store {res_ty_str} {tmp}, {res_ty_str}* {output_str}",
                                        res_ty_str = res_ty_str,
                                        tmp = tmp,
                                        output_str = output_str));
            }

            ToVec { ref output, ref child } => {
                let old_ty = try!(get_sym_ty(func, child));
                let new_ty = try!(get_sym_ty(func, output));
                let old_ll_ty = try!(self.llvm_type(&old_ty)).to_string();
                let new_ll_ty = try!(self.llvm_type(&new_ty)).to_string();

                let dict_prefix = format!("@{}", old_ll_ty.replace("%", ""));
                let child_tmp = try!(self.load_var(llvm_symbol(child).as_str(), &old_ll_ty, ctx));
                let res_tmp = ctx.var_ids.next();
                ctx.code.add(format!("{} = call {} {}.tovec({} {})",
                                        res_tmp,
                                        new_ll_ty,
                                        dict_prefix,
                                        old_ll_ty,
                                        child_tmp));
                let out_ty = try!(get_sym_ty(func, output));
                let out_ty_str = try!(self.llvm_type(&out_ty)).to_string();
                ctx.code.add(format!("store {} {}, {}* {}", out_ty_str, res_tmp, out_ty_str, llvm_symbol(output)));
            }

            Length { ref output, ref child } => {
                let child_ty = try!(get_sym_ty(func, child));
                let child_ll_ty = try!(self.llvm_type(&child_ty)).to_string();
                let vec_prefix = format!("@{}", child_ll_ty.replace("%", ""));
                let child_tmp = try!(self.load_var(llvm_symbol(child).as_str(), &child_ll_ty, ctx));
                let res_tmp = ctx.var_ids.next();
                ctx.code.add(format!("{} = call i64 {}.size({} {})", res_tmp, vec_prefix, child_ll_ty, child_tmp));
                let out_ty = try!(get_sym_ty(func, output));
                let out_ty_str = try!(self.llvm_type(&out_ty)).to_string();
                ctx.code.add(format!("store {} {}, {}* {}", out_ty_str, res_tmp, out_ty_str, llvm_symbol(output)));
            }

            Assign { ref output, ref value } => {
                let ty = try!(get_sym_ty(func, output));
                let ll_ty = try!(self.llvm_type(&ty)).to_string();
                let val_tmp = try!(self.load_var(llvm_symbol(value).as_str(), &ll_ty, ctx));
                ctx.code.add(format!("store {} {}, {}* {}", ll_ty, val_tmp, ll_ty, llvm_symbol(output)));
            }

            GetField { ref output, ref value, index } => {
                let struct_ty = try!(self.llvm_type(try!(get_sym_ty(func, value)))).to_string();
                let field_ty = try!(self.llvm_type(try!(get_sym_ty(func, output)))).to_string();
                let struct_tmp = try!(self.load_var(llvm_symbol(value).as_str(), &struct_ty, ctx));
                let res_tmp = ctx.var_ids.next();
                ctx.code.add(format!("{} = extractvalue {} {}, {}", res_tmp, struct_ty, struct_tmp, index));
                ctx.code.add(format!("store {} {}, {}* {}", field_ty, res_tmp, field_ty, llvm_symbol(output)));
            }

            AssignLiteral { ref output, ref value } => {
                let ty = get_sym_ty(func, output)?;
                if let Simd(_) = *ty {
                    self.generate_vector_literal(&llvm_symbol(output), value, ty, ctx)?;
                } else {
                    match *value {
                        BoolLiteral(l) => {
                            ctx.code.add(format!("store i1 {}, i1* {}", if l { 1 } else { 0 }, llvm_symbol(output)))
                        }
                        I8Literal(l) => ctx.code.add(format!("store i8 {}, i8* {}", l, llvm_symbol(output))),
                        I32Literal(l) => ctx.code.add(format!("store i32 {}, i32* {}", l, llvm_symbol(output))),
                        I64Literal(l) => ctx.code.add(format!("store i64 {}, i64* {}", l, llvm_symbol(output))),
                        F32Literal(l) => ctx.code.add(format!("store float {:.30e}, float* {}", l, llvm_symbol(output))),
                        F64Literal(l) => ctx.code.add(format!("store double {:.30e}, double* {}", l, llvm_symbol(output)))
                    }
                }
            }

            Merge { ref builder, ref value } => {
                let bld_ty = get_sym_ty(func, builder)?;
                if let Builder(ref bld_kind, _) = *bld_ty {
                    self.gen_merge(bld_kind, builder, value, func, ctx)?;
                } else {
                    return weld_err!("Non builder type {} found in Merge", print_type(bld_ty))
                }
            }

            Res { ref output, ref builder } => {
                let bld_ty = try!(get_sym_ty(func, builder));
                if let Builder(ref bld_kind, _) = *bld_ty {
                    self.gen_result(bld_kind, builder, output, func, ctx)?;
                } else {
                    return weld_err!("Non builder type {} found in Res", print_type(bld_ty))
                }
            }

            NewBuilder { ref output, ref arg, ref ty } => {
                if let Builder(ref bld_kind, ref annotations) = *ty {
                    self.gen_new_builder(bld_kind, annotations, arg, output, func, ctx)?;
                } else {
                    return weld_err!("Non builder type {} found in NewBuilder", print_type(ty))
                }
            }
        }

        Ok(())
    }

    /// Generate code for a Merge instruction, appending it to the given FunctionContext.
    fn gen_merge(&mut self,
                 builder_kind: &BuilderKind,
                 builder: &Symbol,
                 value: &Symbol,
                 func: &SirFunction,
                 ctx: &mut FunctionContext)
                 -> WeldResult<()> {
        let bld_ty = get_sym_ty(func, builder)?;
        let bld_ty_str = self.llvm_type(&bld_ty)?.to_string();
        let bld_prefix = format!("@{}", bld_ty_str.replace("%", ""));

        // TODO(Deepak): Do something with annotations here too...
        match *builder_kind {
            Appender(ref t) => {
                let bld_tmp = try!(self.load_var(llvm_symbol(builder).as_str(), &bld_ty_str, ctx));
                let elem_ty_str = try!(self.llvm_type(t)).to_string();
                let elem_tmp = try!(self.load_var(llvm_symbol(value).as_str(), &elem_ty_str, ctx));
                ctx.code.add(format!("call {} {}.merge({} {}, {} {}, \
                                        i32 %cur.tid)",
                                        bld_ty_str,
                                        bld_prefix,
                                        bld_ty_str,
                                        bld_tmp,
                                        elem_ty_str,
                                        elem_tmp));
            }
            
            DictMerger(ref kt, ref vt, _) => {
                let bld_tmp = try!(self.load_var(llvm_symbol(builder).as_str(), &bld_ty_str, ctx));
                let elem_ty = Struct(vec![*kt.clone(), *vt.clone()]);
                let elem_ty_str = try!(self.llvm_type(&elem_ty)).to_string();
                let elem_tmp = try!(self.load_var(llvm_symbol(value).as_str(), &elem_ty_str, ctx));
                ctx.code.add(format!(
                    "call {} {}.merge({} {}, {} {}, i32 %cur.tid)",
                    bld_ty_str,
                    bld_prefix,
                    bld_ty_str,
                    bld_tmp,
                    elem_ty_str,
                    elem_tmp));
            }
            
            GroupMerger(ref kt, ref vt) => {
                let bld_tmp = try!(self.load_var(llvm_symbol(builder).as_str(), &bld_ty_str, ctx));
                let elem_ty = Struct(vec![*kt.clone(), *vt.clone()]);
                let elem_ty_str = try!(self.llvm_type(&elem_ty)).to_string();
                let elem_tmp = try!(self.load_var(llvm_symbol(value).as_str(), &elem_ty_str, ctx));
                ctx.code.add(format!(
                    "call {} {}.merge({} {}, {} {}, i32 %cur.tid)",
                    bld_ty_str,
                    bld_prefix,
                    bld_ty_str,
                    bld_tmp,
                    elem_ty_str,
                    elem_tmp));
            }

            Merger(ref t, ref op) => {
                let bld_tmp = self.load_var(llvm_symbol(builder).as_str(), &bld_ty_str, ctx)?;
                let value_ty = get_sym_ty(func, value)?;
                let elem_ty_str = self.llvm_type(value_ty)?.to_string();
                let elem_tmp = self.load_var(llvm_symbol(value).as_str(), &elem_ty_str, ctx)?;
                let bld_ptr_raw = ctx.var_ids.next();
                let bld_ptr = ctx.var_ids.next();
                ctx.code.add(format!(
                    "{bld_ptr_raw} = call {bld_ty_str} {bld_prefix}.getPtrIndexed({bld_ty_str} {bld_tmp}, i32 %cur.tid)",
                    bld_ptr_raw=bld_ptr_raw,
                    bld_ty_str=bld_ty_str,
                    bld_prefix=bld_prefix,
                    bld_tmp=bld_tmp));

                // If the argument is vectorized, load the vector element.
                if let Simd(_) = *value_ty {
                    ctx.code.add(format!(
                        "{bld_ptr} = call {elem_ty_str}* {bld_prefix}.vectorMergePtr({bld_ty_str} {bld_ptr_raw})",
                        bld_ptr=bld_ptr,
                        elem_ty_str=elem_ty_str,
                        bld_prefix=bld_prefix,
                        bld_ty_str=bld_ty_str,
                        bld_ptr_raw=bld_ptr_raw));

                } else {
                    ctx.code.add(format!(
                        "{bld_ptr} = call {elem_ty_str}* {bld_prefix}.scalarMergePtr({bld_ty_str} {bld_ptr_raw})",
                        bld_ptr=bld_ptr,
                        elem_ty_str=elem_ty_str,
                        bld_prefix=bld_prefix,
                        bld_ty_str=bld_ty_str,
                        bld_ptr_raw=bld_ptr_raw));
                }

                self.gen_merge_op(&bld_ptr, &elem_tmp, &elem_ty_str, op, t, ctx)?;
            }

            VecMerger(ref t, ref op) => {
                let elem_ty_str = self.llvm_type(t)?.to_string();
                let merge_ty = Struct(vec![Scalar(ScalarKind::I64), *t.clone()]);
                let merge_ty_str = self.llvm_type(&merge_ty)?.to_string();
                let bld_tmp = self.load_var(llvm_symbol(builder).as_str(), &bld_ty_str, ctx)?;
                let elem_tmp = self.load_var(llvm_symbol(value).as_str(), &merge_ty_str, ctx)?;
                let index_var = ctx.var_ids.next();
                let elem_var = ctx.var_ids.next();
                ctx.code.add(format!("{} = extractvalue {} {}, 0", index_var, merge_ty_str, elem_tmp));
                ctx.code.add(format!("{} = extractvalue {} {}, 1", elem_var, merge_ty_str, elem_tmp));
                let bld_ptr_raw = ctx.var_ids.next();
                let bld_ptr = ctx.var_ids.next();
                ctx.code.add(format!("{} = call i8* {}.merge_ptr({} {}, i64 {}, i32 %cur.tid)",
                                        bld_ptr_raw,
                                        bld_prefix,
                                        bld_ty_str,
                                        bld_tmp,
                                        index_var));
                ctx.code.add(format!("{} = bitcast i8* {} to {}*",
                                        bld_ptr,
                                        bld_ptr_raw,
                                        elem_ty_str));
                self.gen_merge_op(&bld_ptr, &elem_var, &elem_ty_str, op, t, ctx)?;
            }
        }
        
        Ok(())
    }

    /// Generate code to compute the result of a `builder` and store it in `output`, appending it to a FunctionContext.
    fn gen_result(&mut self,
                  builder_kind: &BuilderKind,
                  builder: &Symbol,
                  output: &Symbol,
                  func: &SirFunction,
                  ctx: &mut FunctionContext)
                  -> WeldResult<()> {
        let bld_ty = get_sym_ty(func, builder)?;
        let res_ty = get_sym_ty(func, output)?;

        match *builder_kind {
            Appender(_) => {
                let bld_ty_str = try!(self.llvm_type(&bld_ty)).to_string();
                let bld_prefix = format!("@{}", bld_ty_str.replace("%", ""));
                let res_ty_str = try!(self.llvm_type(&res_ty)).to_string();
                let bld_tmp =
                    try!(self.load_var(llvm_symbol(builder).as_str(), &bld_ty_str, ctx));
                let res_tmp = ctx.var_ids.next();
                ctx.code.add(format!("{} = call {} {}.result({} {})",
                                     res_tmp,
                                     res_ty_str,
                                     bld_prefix,
                                     bld_ty_str,
                                     bld_tmp));
                ctx.code.add(format!("store {} {}, {}* {}",
                                     res_ty_str,
                                     res_tmp,
                                     res_ty_str,
                                     llvm_symbol(output)));
            }

            Merger(ref t, ref op) => {
                // Type of element to merge.
                let elem_ty_str = self.llvm_type(t)?.to_string();

                let output_str = format!("%{}", output);

                // Vector type.
                let ref vec_type = if let Scalar(ref k) = **t {
                    Simd(k.clone())
                } else {
                    return weld_err!("Invalid non-scalar type in merger");
                };

                let elem_vec_ty_str = self.llvm_type(vec_type)?.to_string();

                // Builder type.
                let bld_ty_str = try!(self.llvm_type(&bld_ty)).to_string();
                // Prefix of the builder.
                let bld_prefix = format!("@{}", bld_ty_str.replace("%", ""));
                // Result type.
                let res_ty_str = try!(self.llvm_type(&res_ty)).to_string();
                // Temporary builder variable.
                let bld_tmp = try!(self.load_var(llvm_symbol(builder).as_str(), &bld_ty_str, ctx));

                // Generate names for all temporaries.
                let t0 = ctx.var_ids.next();
                let scalar_ptr = ctx.var_ids.next();
                let vector_ptr = ctx.var_ids.next();
                let first_scalar = ctx.var_ids.next();
                let first_vector = ctx.var_ids.next();
                let nworkers = ctx.var_ids.next();
                let cond = ctx.var_ids.next();
                let i = ctx.var_ids.next();
                let bld_ptr = ctx.var_ids.next();
                let val_scalar_ptr = ctx.var_ids.next();
                let val_vector_ptr = ctx.var_ids.next();
                let val_scalar = ctx.var_ids.next();
                let val_vector = ctx.var_ids.next();
                let i2 = ctx.var_ids.next();
                let cond2 = ctx.var_ids.next();
                let as_ptr = ctx.var_ids.next();

                // Generate label names.
                let label_base = ctx.var_ids.next();
                let mut label_ids = IdGenerator::new(&label_base.replace("%", ""));
                let entry_label = label_ids.next();
                let body_label = label_ids.next();
                let done_label = label_ids.next();

                // state for the vector collapse
                let i_v = ctx.var_ids.next();
                let val_v = ctx.var_ids.next();
                let i2_v = ctx.var_ids.next();
                let cond_v = ctx.var_ids.next();
                let cond2_v = ctx.var_ids.next();
                let final_val_vec = ctx.var_ids.next();
                let scalar_val_2 = ctx.var_ids.next();
                let entry_label_v = label_ids.next();
                let body_label_v = label_ids.next();
                let done_label_v = label_ids.next();
                let vector_width = format!("{}", vec_size(t)?);

                ctx.code.add(format!(include_str!("resources/merger/merger_result_start.ll"),
                                        t0 = t0,
                                        scalar_ptr = scalar_ptr,
                                        vector_ptr = vector_ptr,
                                        nworkers = nworkers,
                                        first_scalar = first_scalar,
                                        first_vector = first_vector,
                                        bld_tmp = bld_tmp,
                                        cond = cond,
                                        i = i,
                                        bld_ptr = bld_ptr,
                                        val_scalar_ptr = val_scalar_ptr,
                                        val_vector_ptr = val_vector_ptr,
                                        val_scalar = val_scalar,
                                        val_vector = val_vector,
                                        i2 = i2,
                                        elem_ty_str = elem_ty_str,
                                        elem_vec_ty_str = elem_vec_ty_str,
                                        bld_ty_str = bld_ty_str,
                                        bld_prefix = bld_prefix,
                                        entry = entry_label,
                                        body = body_label,
                                        done = done_label));

                // Add the scalar and vector values to the aggregate result.
                self.gen_merge_op(&scalar_ptr, &val_scalar, &elem_ty_str, op, t, ctx)?;
                self.gen_merge_op(&vector_ptr, &val_vector, &elem_vec_ty_str, op, t, ctx)?;

                ctx.code.add(format!(include_str!("resources/merger/merger_result_end_vectorized_1.ll"),
                        nworkers = nworkers,
                        i=i,
                        i2=i2,
                        cond2=cond2,
                        i_v=i_v,
                        i2_v=i2_v,
                        cond_v=cond_v,
                        res_ty_str=res_ty_str,
                        vector_ptr=vector_ptr,
                        scalar_ptr=scalar_ptr,
                        final_val_vec=final_val_vec,
                        scalar_val_2=scalar_val_2,
                        vector_width=vector_width,
                        elem_vec_ty_str=elem_vec_ty_str,
                        val_v=val_v,
                        body=body_label,
                        done=done_label,
                        entry_v=entry_label_v,
                        body_v=body_label_v,
                        done_v=done_label_v,
                        output=output_str));

                self.gen_merge_op(&output_str, &val_v, &res_ty_str, op, t, ctx)?;

                ctx.code.add(format!(include_str!("resources/merger/merger_result_end_vectorized_2.ll"),
                        i_v=i_v,
                        i2_v=i2_v,
                        cond2_v=cond2_v,
                        as_ptr=as_ptr,
                        bld_ty_str=bld_ty_str,
                        bld_tmp=bld_tmp,
                        body_v=body_label_v,
                        vector_width=vector_width,
                        done_v=done_label_v));
            }

            DictMerger(_, _, _) => {
                let bld_ty_str = try!(self.llvm_type(&bld_ty)).to_string();
                let bld_prefix = format!("@{}", bld_ty_str.replace("%", ""));
                let res_ty_str = try!(self.llvm_type(&res_ty)).to_string();
                let bld_tmp =
                    try!(self.load_var(llvm_symbol(builder).as_str(), &bld_ty_str, ctx));
                let res_tmp = ctx.var_ids.next();
                ctx.code.add(format!("{} = call {} {}.result({} {})",
                                        res_tmp,
                                        res_ty_str,
                                        bld_prefix,
                                        bld_ty_str,
                                        bld_tmp));
                ctx.code.add(format!("store {} {}, {}* {}",
                                        res_ty_str,
                                        res_tmp,
                                        res_ty_str,
                                        llvm_symbol(output)));
            }

            GroupMerger(ref kt, ref vt) => {
                let mut func_gen = IdGenerator::new("%func");
                let function_id = func_gen.next();
                let func_str = format!("@{}", &function_id.replace("%", ""));
                let bld_ty = Dict(kt.clone(), Box::new(Vector(vt.clone())));
                let elem = Box::new(Struct(vec![*kt.clone(), *vt.clone()]));
                let bld_ty_str = try!(self.llvm_type(&bld_ty)).to_string();
                let kv_struct_ty = try!(self.llvm_type(&elem)).to_string();
                let key_ty = try!(self.llvm_type(kt)).to_string();
                let value_ty = try!(self.llvm_type(vt)).to_string();
                let value_vec_ty = try!(self.llvm_type(&Box::new(Vector(vt.clone())))).to_string();
                let kv_vec = Box::new(Vector(elem.clone()));
                let kv_vec_ty = try!(self.llvm_type(&kv_vec)).to_string();
                let kv_vec_builder_ty = format!("{}.bld", &kv_vec_ty);
                let key_prefix = format!("@{}", &key_ty.replace("%", ""));
                let kv_vec_prefix = format!("@{}", &kv_vec_ty.replace("%", ""));
                let value_vec_prefix = format!("@{}", &value_vec_ty.replace("%", ""));
                let dict_prefix = format!("@{}", &bld_ty_str.replace("%", ""));

                let name_replaced = GROUPMERGER_CODE.replace("$NAME", &function_id.replace("%", ""));
                let key_prefix_replaced = name_replaced.replace("$KEY_PREFIX", &key_prefix);
                let key_ty_replaced = key_prefix_replaced.replace("$KEY", &key_ty);
                let value_vec_prefix_ty_replaced = key_ty_replaced.replace("$VALUE_VEC_PREFIX", &value_vec_prefix);
                let value_vec_ty_replaced = value_vec_prefix_ty_replaced.replace("$VALUE_VEC", &value_vec_ty);
                let value_ty_replaced = value_vec_ty_replaced.replace("$VALUE", &value_ty);
                let kv_struct_replaced = value_ty_replaced.replace("$KV_STRUCT", &kv_struct_ty.replace("%", ""));
                let kv_vec_prefix_replaced = kv_struct_replaced.replace("$KV_VEC_PREFIX", &kv_vec_prefix);
                let kv_vec_ty_replaced = kv_vec_prefix_replaced.replace("$KV_VEC", &kv_vec_ty);
                let dict_ty_prefix_replaced = kv_vec_ty_replaced.replace("$DICT_PREFIX", &dict_prefix);
                let dict_ty_replaced = dict_ty_prefix_replaced.replace("$DICT", &bld_ty_str);
                self.prelude_code.add(&dict_ty_replaced);

                let res_ty_str = try!(self.llvm_type(&res_ty)).to_string();

                let bld_tmp = try!(self.load_var(llvm_symbol(builder).as_str(), &kv_vec_builder_ty, ctx));
                let res_tmp = ctx.var_ids.next();

                ctx.code.add(format!("{} = call {} {}({} {})",
                                      res_tmp,
                                      bld_ty_str,
                                      func_str,
                                      kv_vec_builder_ty,
                                      bld_tmp));
                ctx.code.add(format!("store {} {}, {}* {}",
                                     res_ty_str,
                                     res_tmp,
                                     res_ty_str,
                                     llvm_symbol(output)));
            }

            VecMerger(ref t, ref op) => {
                // The builder type (special internal type).
                let bld_ty_str = try!(self.llvm_type(&bld_ty)).to_string();
                let bld_prefix = format!("@{}", bld_ty_str.replace("%", ""));
                // The result type (vec[elem_type])
                let res_ty_str = try!(self.llvm_type(&res_ty)).to_string();
                let res_prefix = format!("@{}", res_ty_str.replace("%", ""));
                // The element type
                let elem_ty_str = self.llvm_type(t)?.to_string();
                // The builder we operate on.
                let bld_ptr =
                    try!(self.load_var(llvm_symbol(builder).as_str(), &bld_ty_str, ctx));

                // Generate names for all temporaries.
                let nworkers = ctx.var_ids.next();
                let t0 = ctx.var_ids.next();
                let typed_ptr = ctx.var_ids.next();
                let first_vec = ctx.var_ids.next();
                let size = ctx.var_ids.next();
                let ret_value = ctx.var_ids.next();
                let cond = ctx.var_ids.next();
                let i = ctx.var_ids.next();
                let vec_ptr = ctx.var_ids.next();
                let cur_vec = ctx.var_ids.next();
                let copy_cond = ctx.var_ids.next();
                let j = ctx.var_ids.next();
                let elem_ptr = ctx.var_ids.next();
                let merge_value = ctx.var_ids.next();
                let merge_ptr = ctx.var_ids.next();
                let j2 = ctx.var_ids.next();
                let copy_cond2 = ctx.var_ids.next();
                let i2 = ctx.var_ids.next();
                let cond2 = ctx.var_ids.next();

                // Generate label names.
                let label_base = ctx.var_ids.next();
                let mut label_ids = IdGenerator::new(&label_base.replace("%", ""));
                let entry = label_ids.next();
                let body_label = label_ids.next();
                let copy_entry_label = label_ids.next();
                let copy_body_label = label_ids.next();
                let copy_done_label = label_ids.next();
                let done_label = label_ids.next();
                let raw_ptr = ctx.var_ids.next();

                ctx.code.add(format!(include_str!("resources/vecmerger/vecmerger_result_start.ll"),
                                    nworkers = nworkers,
                                    t0 = t0,
                                    buildPtr = bld_ptr,
                                    resType = res_ty_str,
                                    resPrefix = res_prefix,
                                    elemType = elem_ty_str,
                                    typedPtr = typed_ptr,
                                    firstVec = first_vec,
                                    size = size,
                                    retValue = ret_value,
                                    cond = cond,
                                    i = i,
                                    i2 = i2,
                                    vecPtr = vec_ptr,
                                    curVec = cur_vec,
                                    copyCond = copy_cond,
                                    j = j,
                                    j2 = j2,
                                    elemPtr = elem_ptr,
                                    mergeValue = merge_value,
                                    mergePtr = merge_ptr,
                                    entry = entry,
                                    bodyLabel = body_label,
                                    copyEntryLabel = copy_entry_label,
                                    copyBodyLabel = copy_body_label,
                                    copyDoneLabel = copy_done_label,
                                    doneLabel = done_label,
                                    bldType = bld_ty_str,
                                    bldPrefix = bld_prefix));

                self.gen_merge_op(&merge_ptr, &merge_value, &elem_ty_str, op, t, ctx)?;

                ctx.code.add(format!(include_str!("resources/vecmerger/vecmerger_result_end.ll"),
                                    j2 = j2,
                                    j = j,
                                    copyCond2 = copy_cond2,
                                    size = size,
                                    i2 = i2,
                                    i = i,
                                    cond2 = cond2,
                                    nworkers = nworkers,
                                    resType = res_ty_str,
                                    retValue = ret_value,
                                    copyBodyLabel = copy_body_label,
                                    copyDoneLabel = copy_done_label,
                                    doneLabel = done_label,
                                    bodyLabel = body_label,
                                    rawPtr = raw_ptr,
                                    buildPtr = bld_ptr,
                                    bldType = bld_ty_str,
                                    output = llvm_symbol(output)));
            }
        }

        Ok(())
    }

    /// Generate code for a NewBuilder statement, creating a builder of the given type with a given `arg` and
    /// storing the result in an `output` symbol. Appends the code to a given FunctionContext.
    fn gen_new_builder(&mut self,
                       builder_kind: &BuilderKind,
                       annotations: &Annotations,
                       arg: &Option<Symbol>,
                       output: &Symbol,
                       func: &SirFunction,
                       ctx: &mut FunctionContext)
                       -> WeldResult<()> {
        let bld_ty = get_sym_ty(func, output)?;
        let bld_ty_str = self.llvm_type(bld_ty)?.to_string();
        let bld_prefix = format!("@{}", bld_ty_str.replace("%", ""));

        let mut builder_size = 16;
        if let Some(ref e) = *annotations.size() {
            builder_size = e.clone();
        }

        // TODO(Deepak): Do more with annotations here...
        match *builder_kind {
            Appender(_) => {
                let bld_tmp = ctx.var_ids.next();
                ctx.code.add(format!(
                    "{} = call {} {}.new(i64 {}, %work_t* \
                                    %cur.work)",
                    bld_tmp,
                    bld_ty_str,
                    bld_prefix,
                    builder_size
                ));
                ctx.code.add(format!("store {} {}, {}* {}",
                                        bld_ty_str,
                                        bld_tmp,
                                        bld_ty_str,
                                        llvm_symbol(output)));
            }
            Merger(ref elem_ty, ref op) => {
                let elem_type = (self.llvm_type(elem_ty)?).to_string();
                let bld_tmp = ctx.var_ids.next();
                ctx.code.add(format!("{} = call {} {}.new()", bld_tmp, bld_ty_str, bld_prefix));

                // Generate code to initialize the builder.
                let iden_elem = binop_identity(*op, elem_ty.as_ref())?;
                let init_elem = match *arg {
                    Some(ref s) => {
                        let arg_str = self.load_var(llvm_symbol(s).as_str(), &elem_type, ctx)?;
                        arg_str
                    }
                    _ => iden_elem.clone(),
                };

                let first = ctx.var_ids.next();
                let first_raw = ctx.var_ids.next();
                let nworkers = ctx.var_ids.next();
                let i = ctx.var_ids.next();
                let cur_ptr = ctx.var_ids.next();
                let cur_bld_ptr = ctx.var_ids.next();
                let i2 = ctx.var_ids.next();
                let cond = ctx.var_ids.next();
                let cond2 = ctx.var_ids.next();

                // Generate label names.
                let label_base = ctx.var_ids.next();
                let mut label_ids = IdGenerator::new(&label_base.replace("%", ""));
                let entry = label_ids.next();
                let body = label_ids.next();
                let done = label_ids.next();

                ctx.code.add(format!(include_str!("resources/merger/init_merger.ll"),
                                        first = first,
                                        first_raw = first_raw,
                                        nworkers = nworkers,
                                        bld_ty_str = bld_ty_str,
                                        bld_prefix = bld_prefix,
                                        init_elem = init_elem,
                                        elem_type = elem_type,
                                        cond = cond,
                                        iden_elem = iden_elem,
                                        bld_inp = bld_tmp,
                                        i = i,
                                        cur_ptr = cur_ptr,
                                        cur_bld_ptr = cur_bld_ptr,
                                        i2 = i2,
                                        cond2 = cond2,
                                        entry = entry,
                                        body = body,
                                        done = done));

                ctx.code.add(format!("store {} {}, {}* {}",
                                        bld_ty_str,
                                        bld_tmp,
                                        bld_ty_str,
                                        llvm_symbol(output)));
            }
            DictMerger(_, _, _) => {
                let bld_tmp = ctx.var_ids.next();
                ctx.code.add(format!("{} = call {} {}.new(i64 {})",
                                        bld_tmp,
                                        bld_ty_str,
                                        bld_prefix,
                                        builder_size));
                ctx.code.add(format!("store {} {}, {}* {}",
                                        bld_ty_str,
                                        bld_tmp,
                                        bld_ty_str,
                                        llvm_symbol(output)));
            }
            GroupMerger(_, _) => {
                let bld_tmp = ctx.var_ids.next();
                ctx.code.add(format!(
                    "{} = call {} {}.new(i64 {}, %work_t* \
                                    %cur.work)",
                    bld_tmp,
                    bld_ty_str,
                    bld_prefix,
                    builder_size
                ));
                ctx.code.add(format!("store {} {}, {}* {}",
                                        bld_ty_str,
                                        bld_tmp,
                                        bld_ty_str,
                                        llvm_symbol(output)));
            }
            VecMerger(ref elem, ref op) => {
                if *op != BinOpKind::Add {
                    return weld_err!("VecMerger only supports +");
                }
                match *arg {
                    Some(ref s) => {
                        let arg_ty = try!(self.llvm_type(&Vector(elem.clone()))).to_string();
                        let arg_ty_str = arg_ty.to_string();
                        let arg_str = self.load_var(llvm_symbol(s).as_str(), &arg_ty_str, ctx)?;
                        let bld_tmp = ctx.var_ids.next();
                        ctx.code.add(format!("{} = call {} {}.new({} \
                                                {})",
                                                bld_tmp,
                                                bld_ty_str,
                                                bld_prefix,
                                                arg_ty_str,
                                                arg_str));
                        ctx.code.add(format!("store {} {}, {}* {}",
                                                bld_ty_str,
                                                bld_tmp,
                                                bld_ty_str,
                                                llvm_symbol(output)));
                    }
                    None => {
                        weld_err!("Internal error: NewBuilder(VecMerger) \
                                    expected argument in LLVM codegen")?
                    }
                }
            }
        }

        Ok(())
    }

    /// Generate code for a basic block's terminator, appending it to the given FunctionContext.
    fn gen_terminator(&mut self,
                      terminator: &Terminator,
                      sir: &SirProgram,
                      func: &SirFunction,
                      ctx: &mut FunctionContext)
                      -> WeldResult<()> {
        match *terminator {
            Branch { ref cond, on_true, on_false } => {
                let cond_tmp = try!(self.load_var(llvm_symbol(cond).as_str(), "i1", ctx));
                ctx.code.add(format!("br i1 {}, label %b.b{}, label %b.b{}", cond_tmp, on_true, on_false));
            }

            ParallelFor(ref pf) => {
                try!(self.add_function(sir, &sir.funcs[pf.cont], None));
                try!(self.add_function(sir, &sir.funcs[pf.body], Some(pf.clone())));
                // TODO add parallel wrapper call
                let params = get_combined_params(sir, pf);
                let params_sorted: BTreeMap<&Symbol, &Type> = params.iter().collect();
                let mut arg_types = String::new();
                for (arg, ty) in params_sorted.iter() {
                    let ll_ty = try!(self.llvm_type(&ty)).to_string();
                    let arg_tmp = try!(self.load_var(llvm_symbol(arg).as_str(), &ll_ty, ctx));
                    let arg_str = format!("{} {}, ", &ll_ty, arg_tmp);
                    arg_types.push_str(&arg_str);
                }
                arg_types.push_str("%work_t* %cur.work");
                ctx.code.add(format!("call void @f{}_wrapper({})", pf.body, arg_types));
                ctx.code.add("br label %body.end");
            }

            JumpBlock(block) => {
                ctx.code.add(format!("br label %b.b{}", block));
            }

            JumpFunction(func) => {
                try!(self.add_function(sir, &sir.funcs[func], None));
                let params_sorted: BTreeMap<&Symbol, &Type> = sir.funcs[func].params.iter().collect();
                let mut arg_types = String::new();
                for (arg, ty) in params_sorted.iter() {
                    let ll_ty = try!(self.llvm_type(&ty)).to_string();
                    let arg_tmp = try!(self.load_var(llvm_symbol(arg).as_str(), &ll_ty, ctx));
                    let arg_str = format!("{} {}, ", ll_ty, arg_tmp);
                    arg_types.push_str(&arg_str);
                }
                arg_types.push_str("%work_t* %cur.work");
                ctx.code.add(format!("call void @f{}({})", func, arg_types));
                ctx.code.add("br label %body.end");
            }

            ProgramReturn(ref sym) => {
                let ty = try!(get_sym_ty(func, sym));
                let ty_str = try!(self.llvm_type(ty)).to_string();
                let res_tmp = try!(self.load_var(llvm_symbol(sym).as_str(), &ty_str, ctx));
                let elem_size_ptr = ctx.var_ids.next();
                let elem_size = ctx.var_ids.next();
                let elem_storage = ctx.var_ids.next();
                let elem_storage_typed = ctx.var_ids.next();
                let run_id = ctx.var_ids.next();
                ctx.code.add(format!("{} = getelementptr {}, {}* null, i32 1", &elem_size_ptr, &ty_str, &ty_str));
                ctx.code.add(format!("{} = ptrtoint {}* {} to i64", &elem_size, &ty_str, &elem_size_ptr));

                ctx.code.add(format!("{} = call i64 @get_runid()", run_id));
                ctx.code.add(format!("{} = call i8* @weld_rt_malloc(i64 {}, i64 {})",
                                        &elem_storage,
                                        &run_id,
                                        &elem_size));
                ctx.code.add(format!("{} = bitcast i8* {} to {}*", &elem_storage_typed, &elem_storage, &ty_str));
                ctx.code.add(format!("store {} {}, {}* {}", &ty_str, res_tmp, &ty_str, &elem_storage_typed));
                ctx.code.add(format!("call void @set_result(i8* {})", elem_storage));
                ctx.code.add("br label %body.end");
            }

            EndFunction => {
                ctx.code.add("br label %body.end");
            }

            Crash => {
                let errno = WeldRuntimeErrno::Unknown as i64;
                let run_id = ctx.var_ids.next();
                ctx.code.add(format!("call void @weld_rt_set_errno(i64 {}, i64 {})", run_id, errno));
            }
        }

        Ok(())
    }
}

/// Return the LLVM version of a Weld symbol (encoding any special characters for LLVM).
fn llvm_symbol(symbol: &Symbol) -> String {
    if symbol.id == 0 { format!("%{}", symbol.name) } else { format!("%{}.{}", symbol.name, symbol.id) }
}

fn binop_identity(op_kind: BinOpKind, ty: &Type) -> WeldResult<String> {
    match (op_kind, ty) {
        (BinOpKind::Add, &Scalar(I8)) => Ok("0".to_string()),
        (BinOpKind::Add, &Scalar(I32)) => Ok("0".to_string()),
        (BinOpKind::Add, &Scalar(I64)) => Ok("0".to_string()),
        (BinOpKind::Add, &Scalar(F32)) => Ok("0.0".to_string()),
        (BinOpKind::Add, &Scalar(F64)) => Ok("0.0".to_string()),

        (BinOpKind::Multiply, &Scalar(I8)) => Ok("1".to_string()),
        (BinOpKind::Multiply, &Scalar(I32)) => Ok("1".to_string()),
        (BinOpKind::Multiply, &Scalar(I64)) => Ok("1".to_string()),
        (BinOpKind::Multiply, &Scalar(F32)) => Ok("1.0".to_string()),
        (BinOpKind::Multiply, &Scalar(F64)) => Ok("1.0".to_string()),

        _ => weld_err!("Unsupported identity for binary op: {} on {}", op_kind, print_type(ty)),
    }
}

/// Return the name of the LLVM instruction for a binary operation on a specific type.
fn llvm_binop(op_kind: BinOpKind, ty: &Type) -> WeldResult<&'static str> {
    match (op_kind, ty) {
        (BinOpKind::Add, &Scalar(I8)) => Ok("add"),
        (BinOpKind::Add, &Scalar(I32)) => Ok("add"),
        (BinOpKind::Add, &Scalar(I64)) => Ok("add"),
        (BinOpKind::Add, &Scalar(F32)) => Ok("fadd"),
        (BinOpKind::Add, &Scalar(F64)) => Ok("fadd"),
        (BinOpKind::Add, &Simd(I8)) => Ok("add"),
        (BinOpKind::Add, &Simd(I32)) => Ok("add"),
        (BinOpKind::Add, &Simd(I64)) => Ok("add"),
        (BinOpKind::Add, &Simd(F32)) => Ok("fadd"),
        (BinOpKind::Add, &Simd(F64)) => Ok("fadd"),

        (BinOpKind::Subtract, &Scalar(I8)) => Ok("sub"),
        (BinOpKind::Subtract, &Scalar(I32)) => Ok("sub"),
        (BinOpKind::Subtract, &Scalar(I64)) => Ok("sub"),
        (BinOpKind::Subtract, &Scalar(F32)) => Ok("fsub"),
        (BinOpKind::Subtract, &Scalar(F64)) => Ok("fsub"),
        (BinOpKind::Subtract, &Simd(I8)) => Ok("sub"),
        (BinOpKind::Subtract, &Simd(I32)) => Ok("sub"),
        (BinOpKind::Subtract, &Simd(I64)) => Ok("sub"),
        (BinOpKind::Subtract, &Simd(F32)) => Ok("fsub"),
        (BinOpKind::Subtract, &Simd(F64)) => Ok("fsub"),

        (BinOpKind::Multiply, &Scalar(I8)) => Ok("mul"),
        (BinOpKind::Multiply, &Scalar(I32)) => Ok("mul"),
        (BinOpKind::Multiply, &Scalar(I64)) => Ok("mul"),
        (BinOpKind::Multiply, &Scalar(F32)) => Ok("fmul"),
        (BinOpKind::Multiply, &Scalar(F64)) => Ok("fmul"),
        (BinOpKind::Multiply, &Simd(I8)) => Ok("mul"),
        (BinOpKind::Multiply, &Simd(I32)) => Ok("mul"),
        (BinOpKind::Multiply, &Simd(I64)) => Ok("mul"),
        (BinOpKind::Multiply, &Simd(F32)) => Ok("fmul"),
        (BinOpKind::Multiply, &Simd(F64)) => Ok("fmul"),

        (BinOpKind::Divide, &Scalar(I8)) => Ok("sdiv"),
        (BinOpKind::Divide, &Scalar(I32)) => Ok("sdiv"),
        (BinOpKind::Divide, &Scalar(I64)) => Ok("sdiv"),
        (BinOpKind::Divide, &Scalar(F32)) => Ok("fdiv"),
        (BinOpKind::Divide, &Scalar(F64)) => Ok("fdiv"),
        (BinOpKind::Divide, &Simd(I8)) => Ok("sdiv"),
        (BinOpKind::Divide, &Simd(I32)) => Ok("sdiv"),
        (BinOpKind::Divide, &Simd(I64)) => Ok("sdiv"),
        (BinOpKind::Divide, &Simd(F32)) => Ok("fdiv"),
        (BinOpKind::Divide, &Simd(F64)) => Ok("fdiv"),

        (BinOpKind::Equal, &Scalar(Bool)) => Ok("icmp eq"),
        (BinOpKind::Equal, &Scalar(I8)) => Ok("icmp eq"),
        (BinOpKind::Equal, &Scalar(I32)) => Ok("icmp eq"),
        (BinOpKind::Equal, &Scalar(I64)) => Ok("icmp eq"),
        (BinOpKind::Equal, &Scalar(F32)) => Ok("fcmp oeq"),
        (BinOpKind::Equal, &Scalar(F64)) => Ok("fcmp oeq"),
        (BinOpKind::Equal, &Simd(Bool)) => Ok("icmp eq"),
        (BinOpKind::Equal, &Simd(I8)) => Ok("icmp eq"),
        (BinOpKind::Equal, &Simd(I32)) => Ok("icmp eq"),
        (BinOpKind::Equal, &Simd(I64)) => Ok("icmp eq"),
        (BinOpKind::Equal, &Simd(F32)) => Ok("fcmp oeq"),
        (BinOpKind::Equal, &Simd(F64)) => Ok("fcmp oeq"),

        (BinOpKind::NotEqual, &Scalar(Bool)) => Ok("icmp ne"),
        (BinOpKind::NotEqual, &Scalar(I8)) => Ok("icmp ne"),
        (BinOpKind::NotEqual, &Scalar(I32)) => Ok("icmp ne"),
        (BinOpKind::NotEqual, &Scalar(I64)) => Ok("icmp ne"),
        (BinOpKind::NotEqual, &Scalar(F32)) => Ok("fcmp one"),
        (BinOpKind::NotEqual, &Scalar(F64)) => Ok("fcmp one"),

        (BinOpKind::LessThan, &Scalar(I8)) => Ok("icmp slt"),
        (BinOpKind::LessThan, &Scalar(I32)) => Ok("icmp slt"),
        (BinOpKind::LessThan, &Scalar(I64)) => Ok("icmp slt"),
        (BinOpKind::LessThan, &Scalar(F32)) => Ok("fcmp olt"),
        (BinOpKind::LessThan, &Scalar(F64)) => Ok("fcmp olt"),
        (BinOpKind::LessThan, &Simd(I8)) => Ok("icmp slt"),
        (BinOpKind::LessThan, &Simd(I32)) => Ok("icmp slt"),
        (BinOpKind::LessThan, &Simd(I64)) => Ok("icmp slt"),
        (BinOpKind::LessThan, &Simd(F32)) => Ok("fcmp olt"),
        (BinOpKind::LessThan, &Simd(F64)) => Ok("fcmp olt"),

        (BinOpKind::LessThanOrEqual, &Scalar(I8)) => Ok("icmp sle"),
        (BinOpKind::LessThanOrEqual, &Scalar(I32)) => Ok("icmp sle"),
        (BinOpKind::LessThanOrEqual, &Scalar(I64)) => Ok("icmp sle"),
        (BinOpKind::LessThanOrEqual, &Scalar(F32)) => Ok("fcmp ole"),
        (BinOpKind::LessThanOrEqual, &Scalar(F64)) => Ok("fcmp ole"),
        (BinOpKind::LessThanOrEqual, &Simd(I8)) => Ok("icmp sle"),
        (BinOpKind::LessThanOrEqual, &Simd(I32)) => Ok("icmp sle"),
        (BinOpKind::LessThanOrEqual, &Simd(I64)) => Ok("icmp sle"),
        (BinOpKind::LessThanOrEqual, &Simd(F32)) => Ok("fcmp ole"),
        (BinOpKind::LessThanOrEqual, &Simd(F64)) => Ok("fcmp ole"),

        (BinOpKind::GreaterThan, &Scalar(I8)) => Ok("icmp sgt"),
        (BinOpKind::GreaterThan, &Scalar(I32)) => Ok("icmp sgt"),
        (BinOpKind::GreaterThan, &Scalar(I64)) => Ok("icmp sgt"),
        (BinOpKind::GreaterThan, &Scalar(F32)) => Ok("fcmp ogt"),
        (BinOpKind::GreaterThan, &Scalar(F64)) => Ok("fcmp ogt"),
        (BinOpKind::GreaterThan, &Simd(I8)) => Ok("icmp sgt"),
        (BinOpKind::GreaterThan, &Simd(I32)) => Ok("icmp sgt"),
        (BinOpKind::GreaterThan, &Simd(I64)) => Ok("icmp sgt"),
        (BinOpKind::GreaterThan, &Simd(F32)) => Ok("fcmp ogt"),
        (BinOpKind::GreaterThan, &Simd(F64)) => Ok("fcmp ogt"),

        (BinOpKind::GreaterThanOrEqual, &Scalar(I8)) => Ok("icmp sge"),
        (BinOpKind::GreaterThanOrEqual, &Scalar(I32)) => Ok("icmp sge"),
        (BinOpKind::GreaterThanOrEqual, &Scalar(I64)) => Ok("icmp sge"),
        (BinOpKind::GreaterThanOrEqual, &Scalar(F32)) => Ok("fcmp oge"),
        (BinOpKind::GreaterThanOrEqual, &Scalar(F64)) => Ok("fcmp oge"),
        (BinOpKind::GreaterThanOrEqual, &Simd(I8)) => Ok("icmp sge"),
        (BinOpKind::GreaterThanOrEqual, &Simd(I32)) => Ok("icmp sge"),
        (BinOpKind::GreaterThanOrEqual, &Simd(I64)) => Ok("icmp sge"),
        (BinOpKind::GreaterThanOrEqual, &Simd(F32)) => Ok("fcmp oge"),
        (BinOpKind::GreaterThanOrEqual, &Simd(F64)) => Ok("fcmp oge"),

        (BinOpKind::LogicalAnd, &Scalar(Bool)) => Ok("and"),
        (BinOpKind::BitwiseAnd, &Scalar(Bool)) => Ok("and"),
        (BinOpKind::BitwiseAnd, &Scalar(I8)) => Ok("and"),
        (BinOpKind::BitwiseAnd, &Scalar(I32)) => Ok("and"),
        (BinOpKind::BitwiseAnd, &Scalar(I64)) => Ok("and"),
        (BinOpKind::BitwiseAnd, &Simd(Bool)) => Ok("and"),
        (BinOpKind::BitwiseAnd, &Simd(I8)) => Ok("and"),
        (BinOpKind::BitwiseAnd, &Simd(I32)) => Ok("and"),
        (BinOpKind::BitwiseAnd, &Simd(I64)) => Ok("and"),

        (BinOpKind::LogicalOr, &Scalar(Bool)) => Ok("or"),
        (BinOpKind::BitwiseOr, &Scalar(Bool)) => Ok("or"),
        (BinOpKind::BitwiseOr, &Scalar(I8)) => Ok("or"),
        (BinOpKind::BitwiseOr, &Scalar(I32)) => Ok("or"),
        (BinOpKind::BitwiseOr, &Scalar(I64)) => Ok("or"),
        (BinOpKind::BitwiseOr, &Simd(Bool)) => Ok("or"),
        (BinOpKind::BitwiseOr, &Simd(I8)) => Ok("or"),
        (BinOpKind::BitwiseOr, &Simd(I32)) => Ok("or"),
        (BinOpKind::BitwiseOr, &Simd(I64)) => Ok("or"),

        (BinOpKind::Xor, &Scalar(Bool)) => Ok("xor"),
        (BinOpKind::Xor, &Scalar(I8)) => Ok("xor"),
        (BinOpKind::Xor, &Scalar(I32)) => Ok("xor"),
        (BinOpKind::Xor, &Scalar(I64)) => Ok("xor"),
        (BinOpKind::Xor, &Simd(Bool)) => Ok("xor"),
        (BinOpKind::Xor, &Simd(I8)) => Ok("xor"),
        (BinOpKind::Xor, &Simd(I32)) => Ok("xor"),
        (BinOpKind::Xor, &Simd(I64)) => Ok("xor"),

        _ => weld_err!("Unsupported binary op: {} on {}", op_kind, print_type(ty)),
    }
}

/// Return the name of the LLVM instruction for the given operation and type.
fn llvm_unaryop(op_kind: UnaryOpKind, ty: &ScalarKind) -> WeldResult<&'static str> {
    match (op_kind, ty) {
        (UnaryOpKind::Log, &F32) => Ok("@llvm.log.f32"),
        (UnaryOpKind::Log, &F64) => Ok("@llvm.log.f64"),

        (UnaryOpKind::Exp, &F32) => Ok("@llvm.exp.f32"),
        (UnaryOpKind::Exp, &F64) => Ok("@llvm.exp.f64"),

        (UnaryOpKind::Sqrt, &F32) => Ok("@llvm.sqrt.f32"),
        (UnaryOpKind::Sqrt, &F64) => Ok("@llvm.sqrt.f64"),

        (UnaryOpKind::Erf, &F32) => Ok("@erff"),
        (UnaryOpKind::Erf, &F64) => Ok("@erf"),

        _ => weld_err!("Unsupported unary op: {} on {}", op_kind, ty),
    }
}

/// Return the name of the LLVM instruction for a binary operation between vectors.
fn llvm_binop_vector(op_kind: BinOpKind, ty: &Type) -> WeldResult<(&'static str, i32)> {
    match op_kind {
        BinOpKind::Equal => Ok(("eq", 0)),
        BinOpKind::NotEqual => Ok(("ne", 0)),
        BinOpKind::LessThan => Ok(("eq", -1)),
        BinOpKind::LessThanOrEqual => Ok(("ne", 1)),
        BinOpKind::GreaterThan => Ok(("eq", 1)),
        BinOpKind::GreaterThanOrEqual => Ok(("ne", -1)),

        _ => weld_err!("Unsupported binary op: {} on {}", op_kind, print_type(ty)),
    }
}

/// Return the name of hte LLVM instruction for a cast operation between specific types.
fn llvm_castop(ty1: &Type, ty2: &Type) -> WeldResult<&'static str> {
    match (ty1, ty2) {
        (&Scalar(F64), &Scalar(Bool)) => Ok("fptoui"),
        (&Scalar(F32), &Scalar(Bool)) => Ok("fptoui"),
        (&Scalar(Bool), &Scalar(F64)) => Ok("uitofp"),
        (&Scalar(Bool), &Scalar(F32)) => Ok("uitofp"),
        (&Scalar(F64), &Scalar(F32)) => Ok("fptrunc"),
        (&Scalar(F32), &Scalar(F64)) => Ok("fpext"),
        (&Scalar(F64), _) => Ok("fptosi"),
        (&Scalar(F32), _) => Ok("fptosi"),
        (_, &Scalar(F64)) => Ok("sitofp"),
        (_, &Scalar(F32)) => Ok("sitofp"),
        (&Scalar(Bool), _) => Ok("zext"),
        (_, &Scalar(I64)) => Ok("sext"),
        _ => Ok("trunc"),
    }
}

/// Struct used to track state while generating a function.
struct FunctionContext {
    /// Code section at the start of the function with alloca instructions for local symbols
    alloca_code: CodeBuilder,
    /// Other code in function
    code: CodeBuilder,
    defined_symbols: HashSet<String>,
    var_ids: IdGenerator,
}

impl FunctionContext {
    fn new() -> FunctionContext {
        FunctionContext {
            alloca_code: CodeBuilder::new(),
            code: CodeBuilder::new(),
            var_ids: IdGenerator::new("%t.t"),
            defined_symbols: HashSet::new(),
        }
    }

    fn add_alloca(&mut self, symbol: &str, ty: &str) -> WeldResult<()> {
        if !self.defined_symbols.insert(symbol.to_string()) {
            weld_err!("Symbol already defined in function: {}", symbol)
        } else {
            self.alloca_code.add(format!("{} = alloca {}", symbol, ty));
            Ok(())
        }
    }
}

fn get_combined_params(sir: &SirProgram, par_for: &ParallelForData) -> HashMap<Symbol, Type> {
    let mut body_params = sir.funcs[par_for.body].params.clone();
    for (arg, ty) in sir.funcs[par_for.cont].params.iter() {
        body_params.insert(arg.clone(), ty.clone());
    }
    body_params
}

fn get_sym_ty<'a>(func: &'a SirFunction, sym: &Symbol) -> WeldResult<&'a Type> {
    if func.locals.get(sym).is_some() {
        Ok(func.locals.get(sym).unwrap())
    } else if func.params.get(sym).is_some() {
        Ok(func.params.get(sym).unwrap())
    } else {
        weld_err!("Can't find symbol {}#{}", sym.name, sym.id)
    }
}

/// Returns a vector size for a type. If a Vetor is passed in, returns the vector size of the
/// element type.
///
/// TODO for now just returning 4 for all types.
fn vec_size(_: &Type) -> WeldResult<u32> {
    Ok(4)
}

#[test]
fn types() {
    let mut gen = LlvmGenerator::new();

    assert_eq!(gen.llvm_type(&Scalar(I32)).unwrap(), "i32");
    assert_eq!(gen.llvm_type(&Scalar(I64)).unwrap(), "i64");
    assert_eq!(gen.llvm_type(&Scalar(F32)).unwrap(), "float");
    assert_eq!(gen.llvm_type(&Scalar(F64)).unwrap(), "double");
    assert_eq!(gen.llvm_type(&Scalar(I8)).unwrap(), "i8");
    assert_eq!(gen.llvm_type(&Scalar(Bool)).unwrap(), "i1");

    let struct1 = parse_type("{i32,bool,i32}").unwrap().to_type().unwrap();
    assert_eq!(gen.llvm_type(&struct1).unwrap(), "%s0");
    assert_eq!(gen.llvm_type(&struct1).unwrap(), "%s0"); // Name is reused for same struct

    let struct2 = parse_type("{i32,bool}").unwrap().to_type().unwrap();
    assert_eq!(gen.llvm_type(&struct2).unwrap(), "%s1");
}
