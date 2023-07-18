//! This module defines the defunctionalization pass for the SSA IR.
//! The purpose of this pass is to transforms all functions used as values into
//! constant numbers (fields) that represent the function id. That way all calls
//! with a non-literal target can be replaced with a call to an apply function.
//! The apply function is a dispatch function that takes the function id as a parameter
//! and dispatches to the correct target.
use std::collections::{HashMap, HashSet};

use acvm::FieldElement;
use iter_extended::vecmap;

use crate::ssa_refactor::{
    ir::{
        basic_block::BasicBlockId,
        function::{Function, FunctionId, RuntimeType, Signature},
        instruction::{BinaryOp, Instruction},
        types::{NumericType, Type},
        value::{Value, ValueId},
    },
    ssa_builder::FunctionBuilder,
    ssa_gen::Ssa,
};

/// Finds the common signature of a list of compatible signatures
fn common_signature(signatures: Vec<Signature>) -> Signature {
    fn common_types(types_a: Vec<Type>, types_b: Vec<Type>) -> Vec<Type> {
        types_a
            .into_iter()
            .zip(types_b.into_iter())
            .map(|(type_a, type_b)| {
                type_a
                    .cast_to(&type_b)
                    .or_else(|| type_b.cast_to(&type_a))
                    .expect("ICE: Failed to find common types")
            })
            .collect()
    }

    if signatures.is_empty() {
        unreachable!("ICE: Cannot find common signature of signature list")
    }

    signatures
        .into_iter()
        .reduce(|signature_a, signature_b| {
            let common_params = common_types(signature_a.params, signature_b.params);
            let common_returns = common_types(signature_a.returns, signature_b.returns);
            Signature { params: common_params, returns: common_returns }
        })
        .unwrap()
}
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CallSignature {
    params: Vec<Type>,
    returns: Vec<Type>,
}

impl CallSignature {
    fn from_call_params(caller: &Function, params: &[ValueId], results: &[ValueId]) -> Self {
        let params = vecmap(params, |param| caller.dfg.type_of_value(*param));
        let returns = vecmap(results, |result| caller.dfg.type_of_value(*result));
        CallSignature { params, returns }
    }

    fn can_call(&self, target: &Signature) -> bool {
        self.params.len() == target.params.len()
            && self.returns.len() == target.returns.len()
            && self.params.iter().enumerate().all(|(index, param)| param.is(&target.params[index]))
            && self.returns.iter().enumerate().all(|(index, ret)| target.returns[index].is(ret))
    }
}

/// Represents an 'apply' function created by this pass to dispatch higher order functions to.
/// Pseudocode of an `apply` function is given below:
/// ```text
/// fn apply(function_id: Field, arg1: Field, arg2: Field) -> Field {
///     match function_id {
///         0 -> function0(arg1, arg2),
///         1 -> function0(arg1, arg2),
///         ...
///         N -> functionN(arg1, arg2),
///     }
/// }
/// ```
/// Apply functions generally take the function to apply as their first parameter. This is a Field value
/// obtained by converting the FunctionId into a Field. The remaining parameters of apply are the
/// arguments to forward to this function when calling it internally.
#[derive(Debug, Clone, Copy)]
struct ApplyFunction {
    id: FunctionId,
    dispatches_to_multiple_functions: bool,
}

/// Performs defunctionalization on all functions
/// This is done by changing all functions as value to be a number (FieldElement)
/// And creating apply functions that dispatch to the correct target by runtime comparisons with constants
#[derive(Debug, Clone)]
struct DefunctionalizationContext {
    fn_to_runtime: HashMap<FunctionId, RuntimeType>,
    apply_functions: HashMap<CallSignature, ApplyFunction>,
}

impl Ssa {
    pub(crate) fn defunctionalize(mut self) -> Ssa {
        // Find all functions used as value that share the same signature
        let variants = find_variants(&self);

        let apply_functions = create_apply_functions(&mut self, &variants);
        let fn_to_runtime =
            self.functions.iter().map(|(func_id, func)| (*func_id, func.runtime())).collect();

        let context = DefunctionalizationContext { fn_to_runtime, apply_functions };

        context.defunctionalize_all(&mut self);
        self
    }
}

impl DefunctionalizationContext {
    /// Defunctionalize all functions in the Ssa
    fn defunctionalize_all(mut self, ssa: &mut Ssa) {
        for function in ssa.functions.values_mut() {
            self.defunctionalize(function);
        }
    }

    /// Defunctionalize a single function
    fn defunctionalize(&mut self, func: &mut Function) {
        let mut call_target_values = HashSet::new();

        for block_id in func.reachable_blocks() {
            let block = &func.dfg[block_id];
            let instructions = block.instructions().to_vec();

            for instruction_id in instructions {
                let instruction = func.dfg[instruction_id].clone();
                let mut replacement_instruction = None;
                // Operate on call instructions
                let (target_func_id, mut arguments) = match instruction {
                    Instruction::Call { func: target_func_id, arguments } => {
                        (target_func_id, arguments)
                    }
                    _ => continue,
                };

                match func.dfg[target_func_id] {
                    // If the target is a function used as value
                    Value::Param { .. } | Value::Instruction { .. } => {
                        // Find the correct apply function
                        let apply_function =
                            self.get_apply_function(&CallSignature::from_call_params(
                                func,
                                &arguments,
                                func.dfg.instruction_results(instruction_id),
                            ));

                        // Replace the instruction with a call to apply
                        let apply_function_value_id = func.dfg.import_function(apply_function.id);
                        if apply_function.dispatches_to_multiple_functions {
                            arguments.insert(0, target_func_id);
                        }
                        let func = apply_function_value_id;
                        call_target_values.insert(func);

                        replacement_instruction = Some(Instruction::Call { func, arguments });
                    }
                    Value::Function(..) => {
                        call_target_values.insert(target_func_id);
                    }
                    _ => {}
                }
                if let Some(new_instruction) = replacement_instruction {
                    func.dfg[instruction_id] = new_instruction;
                }
            }
        }

        // Change the type of all the values that are not call targets to NativeField
        let value_ids = vecmap(func.dfg.values_iter(), |(id, _)| id);
        for value_id in value_ids {
            if let Type::Function = &func.dfg[value_id].get_type() {
                match &func.dfg[value_id] {
                    // If the value is a static function, transform it to the function id
                    Value::Function(id) => {
                        if !call_target_values.contains(&value_id) {
                            let new_value =
                                func.dfg.make_constant(function_id_to_field(*id), Type::field());
                            func.dfg.set_value_from_id(value_id, new_value);
                        }
                    }
                    // If the value is a function used as value, just change the type of it
                    Value::Instruction { .. } | Value::Param { .. } => {
                        func.dfg.set_type_of_value(value_id, Type::field());
                    }
                    _ => {}
                }
            }
        }
    }

    /// Returns the apply function for the given signature
    fn get_apply_function(&self, signature: &CallSignature) -> ApplyFunction {
        *self.apply_functions.get(signature).expect("Could not find apply function")
    }
}

/// Collects all functions that can be called by their signatures
fn find_variants(ssa: &Ssa) -> HashMap<CallSignature, Vec<FunctionId>> {
    let mut dynamic_dispatches: HashSet<CallSignature> = HashSet::new();
    let mut functions_as_values: HashSet<FunctionId> = HashSet::new();

    for function in ssa.functions.values() {
        functions_as_values.extend(find_functions_as_values(function));
        dynamic_dispatches.extend(find_dynamic_dispatches(function));
    }

    let mut signature_to_functions_as_value: HashMap<Signature, Vec<FunctionId>> = HashMap::new();

    for function_id in functions_as_values {
        let signature = Signature::from(&ssa.functions[&function_id]);
        signature_to_functions_as_value.entry(signature).or_default().push(function_id);
    }

    let mut variants = HashMap::new();

    for dispatch_signature in dynamic_dispatches {
        let mut target_fns = vec![];
        for (target_signature, functions) in &signature_to_functions_as_value {
            if dispatch_signature.can_call(target_signature) {
                target_fns.extend(functions);
            }
        }
        variants.insert(dispatch_signature, target_fns);
    }

    variants
}

/// Finds all literal functions used as values in the given function
fn find_functions_as_values(func: &Function) -> HashSet<FunctionId> {
    let mut functions_as_values: HashSet<FunctionId> = HashSet::new();

    for block_id in func.reachable_blocks() {
        let block = &func.dfg[block_id];
        for instruction_id in block.instructions() {
            let instruction = &func.dfg[*instruction_id];
            match instruction {
                Instruction::Call { arguments, .. } => {
                    for argument_id in arguments {
                        if let Value::Function(id) = &func.dfg[*argument_id] {
                            functions_as_values.insert(*id);
                        }
                    }
                }
                Instruction::Store { value, .. } => {
                    if let Value::Function(id) = &func.dfg[*value] {
                        functions_as_values.insert(*id);
                    }
                }
                _ => continue,
            };
        }
    }

    functions_as_values
}

/// Finds all dynamic dispatch signatures in the given function
fn find_dynamic_dispatches(func: &Function) -> HashSet<CallSignature> {
    let mut dispatches = HashSet::new();

    for block_id in func.reachable_blocks() {
        let block = &func.dfg[block_id];
        for instruction_id in block.instructions() {
            let instruction = &func.dfg[*instruction_id];
            match instruction {
                Instruction::Call { func: target, arguments } => {
                    if let Value::Param { .. } | Value::Instruction { .. } = &func.dfg[*target] {
                        dispatches.insert(CallSignature::from_call_params(
                            func,
                            arguments,
                            func.dfg.instruction_results(*instruction_id),
                        ));
                    }
                }
                _ => continue,
            };
        }
    }
    dispatches
}

fn create_apply_functions(
    ssa: &mut Ssa,
    variants_map: &HashMap<CallSignature, Vec<FunctionId>>,
) -> HashMap<CallSignature, ApplyFunction> {
    let mut apply_functions = HashMap::new();
    for (signature, variants) in variants_map.iter() {
        assert!(
            !variants.is_empty(),
            "ICE: at least one variant should exist for a dynamic call {:?}",
            signature
        );
        let dispatches_to_multiple_functions = variants.len() > 1;

        let target_signatures: Vec<Signature> =
            vecmap(variants, |function_id| (&ssa.functions[function_id]).into());

        let function_signature = common_signature(target_signatures);

        let id = if dispatches_to_multiple_functions {
            create_apply_function(ssa, function_signature, variants)
        } else {
            variants[0]
        };
        apply_functions
            .insert(signature.clone(), ApplyFunction { id, dispatches_to_multiple_functions });
    }
    apply_functions
}

fn function_id_to_field(function_id: FunctionId) -> FieldElement {
    (function_id.to_usize() as u128).into()
}

/// Creates an apply function for the given signature and variants
fn create_apply_function(
    ssa: &mut Ssa,
    signature: Signature,
    function_ids: &[FunctionId],
) -> FunctionId {
    assert!(!function_ids.is_empty());
    ssa.add_fn(|id| {
        let mut function_builder = FunctionBuilder::new("apply".to_string(), id, RuntimeType::Acir);
        let target_id = function_builder.add_parameter(Type::field());
        let params_ids = vecmap(signature.params, |typ| function_builder.add_parameter(typ));

        let mut previous_target_block = None;
        for (index, function_id) in function_ids.iter().enumerate() {
            let is_last = index == function_ids.len() - 1;
            let mut next_function_block = None;

            let function_id_constant = function_builder.numeric_constant(
                function_id_to_field(*function_id),
                Type::Numeric(NumericType::NativeField),
            );
            let condition =
                function_builder.insert_binary(target_id, BinaryOp::Eq, function_id_constant);

            // If it's not the last function to dispatch, create an if statement
            if !is_last {
                next_function_block = Some(function_builder.insert_block());
                let executor_block = function_builder.insert_block();

                function_builder.terminate_with_jmpif(
                    condition,
                    executor_block,
                    next_function_block.unwrap(),
                );
                function_builder.switch_to_block(executor_block);
            } else {
                // Else just constrain the condition
                function_builder.insert_constrain(condition);
            }
            // Find the target block or build it if necessary
            let current_block = function_builder.current_block();

            let target_block = build_return_block(
                &mut function_builder,
                current_block,
                &signature.returns,
                previous_target_block,
            );
            previous_target_block = Some(target_block);

            // Call the function
            let target_function_value = function_builder.import_function(*function_id);
            let call_results = function_builder
                .insert_call(target_function_value, params_ids.clone(), signature.returns.clone())
                .to_vec();

            // Jump to the target block for returning
            function_builder.terminate_with_jmp(target_block, call_results);

            if let Some(next_block) = next_function_block {
                // Switch to the next block for the else branch
                function_builder.switch_to_block(next_block);
            }
        }
        function_builder.current_function
    })
}

/// Crates a return block, if no previous return exists, it will create a final return
/// Else, it will create a bypass return block that points to the previous return block
fn build_return_block(
    builder: &mut FunctionBuilder,
    previous_block: BasicBlockId,
    passed_types: &[Type],
    target: Option<BasicBlockId>,
) -> BasicBlockId {
    let return_block = builder.insert_block();
    builder.switch_to_block(return_block);

    let params = vecmap(passed_types, |typ| builder.add_block_parameter(return_block, typ.clone()));
    match target {
        None => builder.terminate_with_return(params),
        Some(target) => builder.terminate_with_jmp(target, params),
    }
    builder.switch_to_block(previous_block);
    return_block
}
