use std::collections::hash_map;
use std::ops::{Add, Sub};

use cairo_lang_sierra::extensions::gas::{BuiltinCostWithdrawGasLibfunc, CostTokenType};
use cairo_lang_sierra::ids::ConcreteLibfuncId;
use cairo_lang_sierra::program::{BranchInfo, Invocation, Program, Statement, StatementIdx};
use cairo_lang_utils::casts::IntoOrPanic;
use cairo_lang_utils::iterators::zip_eq3;
use cairo_lang_utils::ordered_hash_map::OrderedHashMap;
use cairo_lang_utils::ordered_hash_set::OrderedHashSet;
use cairo_lang_utils::unordered_hash_map::UnorderedHashMap;
use cairo_lang_utils::unordered_hash_set::UnorderedHashSet;
use itertools::zip_eq;

use crate::gas_info::GasInfo;
use crate::generate_equations::{calculate_reverse_topological_ordering, TopologicalOrderStatus};
use crate::objects::{BranchCost, ConstCost, PreCost};
use crate::CostError;

type VariableValues = OrderedHashMap<(StatementIdx, CostTokenType), i64>;

/// A trait for the cost type (either [PreCost] for pre-cost computation, or `i32` for the post-cost
/// computation).
pub trait CostTypeTrait:
    std::fmt::Debug + Default + Clone + Eq + Add<Output = Self> + Sub<Output = Self>
{
    /// Computes the minimum of the given two value (for each token type).
    ///
    /// Assumes that the arguments are non-negative.
    fn min2(value1: &Self, value2: &Self) -> Self;

    /// Computes the maximum of the given value (for each token type).
    ///
    /// Assumes that the arguments are non-negative.
    fn max(values: impl Iterator<Item = Self>) -> Self;

    /// For each token type, returns the value if it is non-negative and 0 otherwise.
    fn rectify(value: &Self) -> Self;
}

impl CostTypeTrait for i32 {
    fn min2(value1: &Self, value2: &Self) -> Self {
        *std::cmp::min(value1, value2)
    }

    fn max(values: impl Iterator<Item = Self>) -> Self {
        values.max().unwrap_or_default()
    }

    fn rectify(value: &Self) -> Self {
        std::cmp::max(*value, 0)
    }
}

impl CostTypeTrait for PreCost {
    fn min2(value1: &Self, value2: &Self) -> Self {
        let map_fn = |(token_type, val1)| {
            // The tokens that should appear are the tokens that appear in the intersection of both
            // parameters. Return `None` if the token does not appear in `value2`.
            let val2 = value2.0.get(token_type)?;
            Some((*token_type, *std::cmp::min(val1, val2)))
        };
        PreCost(value1.0.iter().filter_map(map_fn).collect())
    }

    fn max(values: impl Iterator<Item = Self>) -> Self {
        let mut res = Self::default();
        for value in values {
            for (token_type, val) in value.0 {
                res.0.insert(token_type, std::cmp::max(*res.0.get(&token_type).unwrap_or(&0), val));
            }
        }
        res
    }

    fn rectify(value: &Self) -> Self {
        let map_fn =
            |(token_type, val): (&CostTokenType, &i32)| (*token_type, std::cmp::max(*val, 0));
        PreCost(value.0.iter().map(map_fn).collect())
    }
}

/// Computes the [GasInfo] for a given program.
///
/// The `specific_cost_context` argument controls whether the computation is pre-cost or post-cost.
pub fn compute_costs<
    CostType: CostTypeTrait,
    SpecificCostContext: SpecificCostContextTrait<CostType>,
>(
    program: &Program,
    get_cost_fn: &dyn Fn(&ConcreteLibfuncId) -> Vec<BranchCost>,
    specific_cost_context: &SpecificCostContext,
) -> Result<GasInfo, CostError> {
    let mut context = CostContext { program, costs: UnorderedHashMap::default(), get_cost_fn };

    context.prepare_wallet(specific_cost_context)?;

    if SpecificCostContext::should_handle_excess() {
        // Compute the excess cost and the corresponding target value for each statement.
        context.compute_target_values(specific_cost_context)?;
    }

    let mut variable_values = VariableValues::default();
    for i in 0..program.statements.len() {
        analyze_gas_statements(
            &context,
            specific_cost_context,
            &StatementIdx(i),
            &mut variable_values,
        );
    }

    let function_costs = program
        .funcs
        .iter()
        .map(|func| {
            let res = SpecificCostContext::to_cost_map(context.wallet_at(&func.entry_point).value);
            (func.id.clone(), res)
        })
        .collect();

    Ok(GasInfo { variable_values, function_costs })
}

/// Returns the statements whose wallet value is needed by
/// [get_branch_requirements].
fn get_branch_requirements_dependencies(
    idx: &StatementIdx,
    invocation: &Invocation,
    libfunc_cost: &[BranchCost],
) -> OrderedHashSet<StatementIdx> {
    let mut res: OrderedHashSet<StatementIdx> = Default::default();
    for (branch_info, branch_cost) in zip_eq(&invocation.branches, libfunc_cost) {
        match branch_cost {
            BranchCost::FunctionCall { const_cost: _, function } => {
                res.insert(function.entry_point);
            }
            BranchCost::WithdrawGas { const_cost: _, success: true, with_builtin_costs: _ } => {
                // If withdraw_gas succeeds, we don't need to take future_wallet_value into account,
                // so we simply return.
                continue;
            }
            _ => {}
        }
        res.insert(idx.next(&branch_info.target));
    }

    res
}

/// Returns the required value for the wallet for each branch.
fn get_branch_requirements<
    CostType: CostTypeTrait,
    SpecificCostContext: SpecificCostContextTrait<CostType>,
>(
    specific_context: &SpecificCostContext,
    wallet_at_fn: &dyn Fn(&StatementIdx) -> WalletInfo<CostType>,
    idx: &StatementIdx,
    invocation: &Invocation,
    libfunc_cost: &[BranchCost],
) -> Vec<WalletInfo<CostType>> {
    zip_eq(&invocation.branches, libfunc_cost)
        .map(|(branch_info, branch_cost)| {
            specific_context.get_branch_requirement(wallet_at_fn, idx, branch_info, branch_cost)
        })
        .collect()
}

/// For every `branch_align` and `withdraw_gas` statements, computes the required cost variables.
///
/// * For `branch_align` this is the amount of cost *reduced* from the wallet.
/// * For `withdraw_gas` this is the amount that should be withdrawn and added to the wallet.
fn analyze_gas_statements<
    CostType: CostTypeTrait,
    SpecificCostContext: SpecificCostContextTrait<CostType>,
>(
    context: &CostContext<'_, CostType>,
    specific_context: &SpecificCostContext,
    idx: &StatementIdx,
    variable_values: &mut VariableValues,
) {
    let Statement::Invocation(invocation) = &context.program.get_statement(idx).unwrap() else {
        return;
    };
    let libfunc_cost: Vec<BranchCost> = context.get_cost(&invocation.libfunc_id);
    let branch_requirements: Vec<WalletInfo<CostType>> = get_branch_requirements(
        specific_context,
        &|statement_idx| context.wallet_at(statement_idx),
        idx,
        invocation,
        &libfunc_cost,
    );

    let wallet_value = context.wallet_at(idx).value;

    for (branch_info, branch_cost, branch_requirement) in
        zip_eq3(&invocation.branches, &libfunc_cost, &branch_requirements)
    {
        let future_wallet_value = context.wallet_at(&idx.next(&branch_info.target)).value;
        // TODO(lior): Consider checking that idx.next(&branch_info.target) is indeed branch
        //   align.
        if let BranchCost::WithdrawGas { success: true, .. } = branch_cost {
            let withdrawal = specific_context.get_gas_withdrawal(
                idx,
                branch_cost,
                &wallet_value,
                future_wallet_value,
            );
            for (token_type, amount) in SpecificCostContext::to_full_cost_map(withdrawal) {
                assert_eq!(
                    variable_values.insert((*idx, token_type), std::cmp::max(amount, 0)),
                    None
                );

                assert_eq!(
                    variable_values.insert(
                        (idx.next(&branch_info.target), token_type),
                        std::cmp::max(-amount, 0),
                    ),
                    None
                );
            }
        } else if invocation.branches.len() > 1 {
            let cost = wallet_value.clone() - branch_requirement.value.clone();
            for (token_type, amount) in SpecificCostContext::to_full_cost_map(cost) {
                assert_eq!(
                    variable_values.insert((idx.next(&branch_info.target), token_type), amount),
                    None
                );
            }
        }
    }
}

pub trait SpecificCostContextTrait<CostType: CostTypeTrait> {
    /// Returns `true` if excess cost should be computed and handled.
    fn should_handle_excess() -> bool;

    /// Converts a `CostType` to a [OrderedHashMap] from [CostTokenType] to i64.
    fn to_cost_map(cost: CostType) -> OrderedHashMap<CostTokenType, i64>;

    /// Converts a `CostType` to a [OrderedHashMap] from [CostTokenType] to i64.
    /// All relevant [CostTokenType] are included (even if their value is 0).
    fn to_full_cost_map(cost: CostType) -> OrderedHashMap<CostTokenType, i64>;

    /// Computes the value that should be withdrawn and added to the wallet.
    fn get_gas_withdrawal(
        &self,
        idx: &StatementIdx,
        branch_cost: &BranchCost,
        wallet_value: &CostType,
        future_wallet_value: CostType,
    ) -> CostType;

    /// Returns the required value for the wallet for a single branch.
    fn get_branch_requirement(
        &self,
        wallet_at_fn: &dyn Fn(&StatementIdx) -> WalletInfo<CostType>,
        idx: &StatementIdx,
        branch_info: &BranchInfo,
        branch_cost: &BranchCost,
    ) -> WalletInfo<CostType>;
}

/// The information about the wallet value at a given statement.
#[derive(Clone, Debug, Default)]
pub struct WalletInfo<CostType: CostTypeTrait> {
    /// The minimum wallet value before executing the statement.
    value: CostType,
}

impl<CostType: CostTypeTrait> WalletInfo<CostType> {
    fn merge(branches: Vec<Self>) -> Self {
        let max_value = CostType::max(branches.iter().map(|wallet_info| wallet_info.value.clone()));
        WalletInfo { value: max_value }
    }
}

/// Implements a cast from CostType to WalletInfo.
impl<CostType: CostTypeTrait> From<CostType> for WalletInfo<CostType> {
    fn from(value: CostType) -> Self {
        WalletInfo { value }
    }
}

/// Implements addition of WalletInfo.
impl<CostType: CostTypeTrait> std::ops::Add for WalletInfo<CostType> {
    type Output = Self;

    fn add(self, other: Self) -> Self {
        WalletInfo { value: self.value + other.value }
    }
}

/// Helper struct for computing the wallet value at each statement.
struct CostContext<'a, CostType: CostTypeTrait> {
    /// The Sierra program.
    program: &'a Program,
    /// A callback function returning the cost of a libfunc for every output branch.
    get_cost_fn: &'a dyn Fn(&ConcreteLibfuncId) -> Vec<BranchCost>,
    /// The cost before executing a Sierra statement.
    costs: UnorderedHashMap<StatementIdx, WalletInfo<CostType>>,
}
impl<'a, CostType: CostTypeTrait> CostContext<'a, CostType> {
    /// Returns the cost of a libfunc for every output branch.
    fn get_cost(&self, libfunc_id: &ConcreteLibfuncId) -> Vec<BranchCost> {
        (self.get_cost_fn)(libfunc_id)
    }

    /// Returns the required value in the wallet before executing statement `idx`.
    ///
    /// Assumes that [Self::prepare_wallet] was called before.
    ///
    /// For `branch_align` the function returns the result as if the alignment is zero (since the
    /// alignment is not know at this point).
    fn wallet_at(&self, idx: &StatementIdx) -> WalletInfo<CostType> {
        self.costs
            .get(idx)
            .unwrap_or_else(|| panic!("Wallet value for statement {idx} was not yet computed."))
            .clone()
    }

    /// Prepares the values for [Self::wallet_at].
    fn prepare_wallet<SpecificCostContext: SpecificCostContextTrait<CostType>>(
        &mut self,
        specific_cost_context: &SpecificCostContext,
    ) -> Result<(), CostError> {
        let topological_order =
            compute_topological_order(self.program.statements.len(), &|current_idx| {
                match &self.program.get_statement(current_idx).unwrap() {
                    Statement::Return(_) => {
                        // Return has no dependencies.
                        vec![]
                    }
                    Statement::Invocation(invocation) => {
                        let libfunc_cost: Vec<BranchCost> = self.get_cost(&invocation.libfunc_id);

                        get_branch_requirements_dependencies(current_idx, invocation, &libfunc_cost)
                            .into_iter()
                            .collect()
                    }
                }
            })?;

        for current_idx in topological_order {
            // The computation of the dependencies was completed.
            let res = self.no_cache_compute_wallet_at(&current_idx, specific_cost_context);
            // Update the cache with the result.
            self.costs.insert(current_idx, res.clone());
        }

        Ok(())
    }

    /// Helper function for `prepare_wallet()`.
    ///
    /// Assumes that the values was already computed for the dependencies.
    fn no_cache_compute_wallet_at<SpecificCostContext: SpecificCostContextTrait<CostType>>(
        &mut self,
        idx: &StatementIdx,
        specific_cost_context: &SpecificCostContext,
    ) -> WalletInfo<CostType> {
        match &self.program.get_statement(idx).unwrap() {
            Statement::Return(_) => Default::default(),
            Statement::Invocation(invocation) => {
                let libfunc_cost: Vec<BranchCost> = self.get_cost(&invocation.libfunc_id);

                // For each branch, compute the required value for the wallet.
                let branch_requirements: Vec<WalletInfo<CostType>> = get_branch_requirements(
                    specific_cost_context,
                    &|statement_idx| self.wallet_at(statement_idx),
                    idx,
                    invocation,
                    &libfunc_cost,
                );

                // The wallet value at the beginning of the statement is the maximal value
                // required by all the branches.
                WalletInfo::merge(branch_requirements)
            }
        }
    }

    /// Computes the target value for each statement. Rerunning `prepare_wallet` with these
    /// target values will try to set the values of statements such as `branch_align`,
    /// `withdraw_gas` and `redeposit_gas` to achieve these targets.
    fn compute_target_values<SpecificCostContext: SpecificCostContextTrait<CostType>>(
        &self,
        specific_cost_context: &SpecificCostContext,
    ) -> Result<UnorderedHashMap<StatementIdx, CostType>, CostError> {
        // Compute a topological order of the statements.
        // Unlike `prepare_wallet`:
        // * function calls are not treated as edges and
        // * the success branches of `withdraw_gas` are treated as edges.
        let topological_order =
            compute_topological_order(self.program.statements.len(), &|current_idx| {
                match &self.program.get_statement(current_idx).unwrap() {
                    Statement::Return(_) => {
                        // Return has no dependencies.
                        vec![]
                    }
                    Statement::Invocation(invocation) => invocation
                        .branches
                        .iter()
                        .map(|branch_info| current_idx.next(&branch_info.target))
                        .collect(),
                }
            })?;

        // Compute the excess mapping - additional amount of cost that, if possible, should be
        // added to the wallet value.
        let mut excess = UnorderedHashMap::<StatementIdx, CostType>::default();
        // The set of statements for which the excess value was already finalized.
        let mut finalized_excess_statements = UnorderedHashSet::<StatementIdx>::default();

        for idx in topological_order.iter().rev() {
            self.handle_excess_at(
                idx,
                specific_cost_context,
                &mut excess,
                &mut finalized_excess_statements,
            );
        }

        // Compute the target value for each statement by adding the excess to the wallet value.
        Ok((0..self.program.statements.len())
            .map(|i| {
                let idx = StatementIdx(i);
                (idx, self.wallet_at(&idx).value + excess.get(&idx).cloned().unwrap_or_default())
            })
            .collect())
    }

    /// Handles the excess at the given statement by pushing it to the next statement(s).
    ///
    /// * `redeposit_gas` - consumes all the excess, as it can be redeposited.
    /// * `branch_align` - adds the difference to the excess, so that it will be possible by a
    ///   future `redeposit_gas`.
    /// * `withdraw_gas` - removes the planned withdrawal from the excess, so that the excess will
    ///   be used instead of a withdrawal.
    fn handle_excess_at<SpecificCostContext: SpecificCostContextTrait<CostType>>(
        &self,
        idx: &StatementIdx,
        specific_cost_context: &SpecificCostContext,
        excess: &mut UnorderedHashMap<StatementIdx, CostType>,
        finalized_excess_statements: &mut UnorderedHashSet<StatementIdx>,
    ) {
        finalized_excess_statements.insert(*idx);

        let current_excess = excess.get(idx).cloned().unwrap_or_default();

        let invocation = match &self.program.get_statement(idx).unwrap() {
            Statement::Invocation(invocation) => invocation,
            Statement::Return(_) => {
                // Excess cannot be handled, simply drop it.
                return;
            }
        };

        let libfunc_cost: Vec<BranchCost> = self.get_cost(&invocation.libfunc_id);

        let branch_requirements = get_branch_requirements(
            specific_cost_context,
            &|statement_idx| self.wallet_at(statement_idx),
            idx,
            invocation,
            &libfunc_cost,
        );

        // Pass the excess to the branches.
        for (branch_info, _branch_cost, _branch_requirement) in
            zip_eq3(&invocation.branches, &libfunc_cost, branch_requirements)
        {
            let branch_statement = idx.next(&branch_info.target);
            if finalized_excess_statements.contains(&branch_statement) {
                // Don't update statements which were already visited.
                return;
            }

            let actual_excess = current_excess.clone();

            // TODO(lior): Modify actual_excess for statements such as `withdraw_gas`,
            //   `redeposit_gas` and `branch_align`.

            // Update the excess for `branch_statement` using the minimum of the existing excess and
            // `actual_excess`.
            match excess.entry(branch_statement) {
                hash_map::Entry::Occupied(mut entry) => {
                    let current_value = entry.get();
                    entry.insert(CostType::min2(current_value, &actual_excess));
                }
                hash_map::Entry::Vacant(entry) => {
                    entry.insert(actual_excess);
                }
            }
        }
    }
}

/// Generates a topological ordering of the statements according to the given dependencies_callback.
///
/// Each statement appears in the ordering after its dependencies.
fn compute_topological_order(
    n_statements: usize,
    dependencies_callback: &dyn Fn(&StatementIdx) -> Vec<StatementIdx>,
) -> Result<Vec<StatementIdx>, CostError> {
    let mut topological_order: Vec<StatementIdx> = Default::default();
    let mut status = vec![TopologicalOrderStatus::NotStarted; n_statements];
    for idx in 0..n_statements {
        calculate_reverse_topological_ordering(
            &mut topological_order,
            &mut status,
            &StatementIdx(idx),
            true,
            dependencies_callback,
        )?;
    }

    Ok(topological_order)
}

pub struct PreCostContext {}

impl SpecificCostContextTrait<PreCost> for PreCostContext {
    fn should_handle_excess() -> bool {
        false
    }

    fn to_cost_map(cost: PreCost) -> OrderedHashMap<CostTokenType, i64> {
        let res = cost.0;
        res.into_iter().map(|(token_type, val)| (token_type, val as i64)).collect()
    }

    fn to_full_cost_map(cost: PreCost) -> OrderedHashMap<CostTokenType, i64> {
        CostTokenType::iter_precost()
            .map(|token_type| (*token_type, (*cost.0.get(token_type).unwrap_or(&0)).into()))
            .collect()
    }

    fn get_gas_withdrawal(
        &self,
        _idx: &StatementIdx,
        _branch_cost: &BranchCost,
        wallet_value: &PreCost,
        future_wallet_value: PreCost,
    ) -> PreCost {
        future_wallet_value - wallet_value.clone()
    }

    fn get_branch_requirement(
        &self,
        wallet_at_fn: &dyn Fn(&StatementIdx) -> WalletInfo<PreCost>,
        idx: &StatementIdx,
        branch_info: &BranchInfo,
        branch_cost: &BranchCost,
    ) -> WalletInfo<PreCost> {
        let branch_cost = match branch_cost {
            BranchCost::Regular { const_cost: _, pre_cost } => pre_cost.clone(),
            BranchCost::BranchAlign => Default::default(),
            BranchCost::FunctionCall { const_cost: _, function } => {
                wallet_at_fn(&function.entry_point).value
            }
            BranchCost::WithdrawGas { const_cost: _, success, with_builtin_costs: _ } => {
                if *success {
                    // If withdraw_gas succeeds, we don't need to take
                    // future_wallet_value into account, so we simply return.
                    return Default::default();
                } else {
                    Default::default()
                }
            }
            BranchCost::RedepositGas => {
                // TODO(lior): Replace with actually redepositing the gas.
                Default::default()
            }
        };
        let future_wallet_value = wallet_at_fn(&idx.next(&branch_info.target));
        WalletInfo::from(branch_cost) + future_wallet_value
    }
}

pub struct PostcostContext<'a> {
    pub get_ap_change_fn: &'a dyn Fn(&StatementIdx) -> usize,
    pub precost_gas_info: &'a GasInfo,
}

impl<'a> SpecificCostContextTrait<i32> for PostcostContext<'a> {
    fn should_handle_excess() -> bool {
        true
    }

    fn to_cost_map(cost: i32) -> OrderedHashMap<CostTokenType, i64> {
        if cost == 0 { Default::default() } else { Self::to_full_cost_map(cost) }
    }

    fn to_full_cost_map(cost: i32) -> OrderedHashMap<CostTokenType, i64> {
        [(CostTokenType::Const, cost.into())].into_iter().collect()
    }

    fn get_gas_withdrawal(
        &self,
        idx: &StatementIdx,
        branch_cost: &BranchCost,
        wallet_value: &i32,
        future_wallet_value: i32,
    ) -> i32 {
        let BranchCost::WithdrawGas { const_cost, success: true, with_builtin_costs } = branch_cost
        else {
            panic!("Unexpected BranchCost: {:?}.", branch_cost);
        };

        let withdraw_gas_cost =
            self.compute_withdraw_gas_cost(idx, const_cost, *with_builtin_costs);
        future_wallet_value + withdraw_gas_cost - *wallet_value
    }

    fn get_branch_requirement(
        &self,
        wallet_at_fn: &dyn Fn(&StatementIdx) -> WalletInfo<i32>,
        idx: &StatementIdx,
        branch_info: &BranchInfo,
        branch_cost: &BranchCost,
    ) -> WalletInfo<i32> {
        let branch_cost_val = match branch_cost {
            BranchCost::Regular { const_cost, pre_cost: _ } => const_cost.cost(),
            BranchCost::BranchAlign => {
                let ap_change = (self.get_ap_change_fn)(idx);
                if ap_change == 0 {
                    0
                } else {
                    ConstCost { steps: 1, holes: ap_change as i32, range_checks: 0 }.cost()
                }
            }
            BranchCost::FunctionCall { const_cost, function } => {
                wallet_at_fn(&function.entry_point).value + const_cost.cost()
            }
            BranchCost::WithdrawGas { const_cost, success, with_builtin_costs } => {
                let cost = self.compute_withdraw_gas_cost(idx, const_cost, *with_builtin_costs);

                // If withdraw_gas succeeds, we don't need to take
                // future_wallet_value into account, so we simply return.
                if *success {
                    return WalletInfo::from(cost);
                }
                cost
            }
            BranchCost::RedepositGas => 0,
        };
        let future_wallet_value = wallet_at_fn(&idx.next(&branch_info.target));
        WalletInfo { value: branch_cost_val } + future_wallet_value
    }
}

impl<'a> PostcostContext<'a> {
    /// Computes the cost of the withdraw_gas libfunc.
    fn compute_withdraw_gas_cost(
        &self,
        idx: &StatementIdx,
        const_cost: &ConstCost,
        with_builtin_costs: bool,
    ) -> i32 {
        let mut amount = const_cost.cost();

        if with_builtin_costs {
            let steps = BuiltinCostWithdrawGasLibfunc::cost_computation_steps(|token_type| {
                self.precost_gas_info.variable_values[(*idx, token_type)].into_or_panic()
            })
            .into_or_panic::<i32>();
            amount += ConstCost { steps, ..Default::default() }.cost();
        }

        amount
    }
}
